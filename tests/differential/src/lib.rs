#![forbid(unsafe_code)]
#![allow(missing_docs)]

use std::fs;
use std::io::{self, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rmpv::Value;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};

pub const ORACLE: &str = ".references/neovim/build/bin/nvim";
pub const OXVIM: &str = "target/release/oxvim";

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn binary(relative: &str) -> PathBuf {
    root().join(relative)
}

pub fn api_info(program: &Path) -> Result<Value, String> {
    let mut child = Command::new(program)
        .arg("--api-info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {} --api-info: {error}", program.display()))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{} --api-info had no stdout", program.display()));
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{} --api-info had no stderr", program.display()));
    };
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = BufReader::new(stdout).read_to_end(&mut bytes);
        (result, bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = BufReader::new(stderr).read_to_end(&mut bytes);
        (result, bytes)
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not poll {} --api-info: {error}", program.display()));
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!("{} --api-info timed out after 10 seconds", program.display()));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (stdout_result, stdout) = stdout_reader.join().map_err(|_| format!("{} stdout reader panicked", program.display()))?;
    stdout_result.map_err(|error| format!("could not read {} stdout: {error}", program.display()))?;
    let (stderr_result, stderr) = stderr_reader.join().map_err(|_| format!("{} stderr reader panicked", program.display()))?;
    stderr_result.map_err(|error| format!("could not read {} stderr: {error}", program.display()))?;
    if !status.success() {
        return Err(format!(
            "{} --api-info exited {status}: {stderr}",
            program.display(),
            stderr = String::from_utf8_lossy(&stderr)
        ));
    }
    let mut input = Cursor::new(&stdout);
    let value = rmpv::decode::read_value(&mut input)
        .map_err(|error| format!("could not decode {} --api-info: {error}", program.display()))?;
    if input.position() != stdout.len() as u64 {
        return Err(format!("{} --api-info emitted trailing bytes", program.display()));
    }
    Ok(value)
}

pub fn normalize_api(mut value: Value) -> Value {
    normalize_build(&mut value);
    normalize(value)
}

fn normalize_build(value: &mut Value) {
    let Value::Map(root) = value else { return };
    let Some((_, Value::Map(version))) = root.iter_mut().find(|(key, _)| key.as_str() == Some("version")) else { return };
    if let Some((_, build @ Value::String(_))) = version.iter_mut().find(|(key, _)| key.as_str() == Some("build")) {
        *build = Value::from("<allowed version.build>");
    }
}

pub fn normalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(normalize).collect()),
        Value::Map(values) => {
            let mut values: Vec<_> = values
                .into_iter()
                .map(|(key, value)| (normalize(key), normalize(value)))
                .collect();
            values.sort_by(|left, right| stable_value(&left.0).cmp(&stable_value(&right.0)));
            Value::Map(values)
        }
        other => other,
    }
}

pub fn readable_diff(expected_name: &str, expected: &Value, actual_name: &str, actual: &Value) -> String {
    let expected = stable_value(expected);
    let actual = stable_value(actual);
    let diff = TextDiff::from_lines(&expected, &actual);
    let mut rendered = format!("--- {expected_name}\n+++ {actual_name}\n");
    for change in diff.iter_all_changes() {
        let marker = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        rendered.push(marker);
        rendered.push_str(change.value());
    }
    rendered
}

pub fn stable_value(value: &Value) -> String {
    let normalized = normalize(value.clone());
    serde_json::to_string_pretty(&normalized)
        .map(|rendered| format!("{rendered}\n"))
        .unwrap_or_else(|_| format!("{normalized:#?}\n"))
}

pub fn divergence_fingerprint(expected: &Value, actual: &Value) -> String {
    let mut digest = Sha256::new();
    digest.update(stable_value(expected));
    digest.update([0]);
    digest.update(stable_value(actual));
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest.finalize() {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Step {
    Send { send: serde_yaml::Value },
    ExpectResponse { expect_response: i64 },
    ExpectNotification { expect_notification: String },
}

pub struct Embedded {
    child: Child,
    input: Option<ChildStdin>,
    incoming: mpsc::Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    label: String,
}

impl Embedded {
    pub fn spawn(program: &Path) -> Result<Self, String> {
        let mut command = Command::new(program);
        command
            .arg("--embed")
            .env("OXVIM_RUNTIME", root().join("runtime"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|error| format!("could not spawn {} --embed: {error}", program.display()))?;
        let Some(input) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("embedded child had no stdin".to_owned());
        };
        let Some(output) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("embedded child had no stdout".to_owned());
        };
        let label = program.display().to_string();
        let (sender, incoming) = mpsc::channel();
        let reader_label = label.clone();
        let reader_result = thread::Builder::new().name("differential-rpc-reader".to_owned()).spawn(move || {
            let mut output = BufReader::new(output);
            loop {
                let decoded = rmpv::decode::read_value(&mut output)
                    .map_err(|error| format!("could not decode response from {reader_label}: {error}"));
                let failed = decoded.is_err();
                if sender.send(decoded).is_err() || failed { break; }
            }
        });
        let reader = match reader_result {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not start RPC reader for {label}: {error}"));
            }
        };
        Ok(Self { child, input: Some(input), incoming, reader: Some(reader), label })
    }

    pub fn send(&mut self, message: &Value) -> Result<(), String> {
        let input = self.input.as_mut().ok_or_else(|| format!("{} stdin is closed", self.label))?;
        rmpv::encode::write_value(input, message).map_err(|error| format!("could not encode request to {}: {error}", self.label))?;
        input.flush().map_err(|error| format!("could not flush request to {}: {error}", self.label))
    }

    pub fn read(&mut self) -> Result<Value, String> {
        self.incoming.recv_timeout(Duration::from_secs(10))
            .map_err(|error| format!("timed out waiting for response from {}: {error}", self.label))?
    }

    pub fn drain_quiescent(&mut self) -> Result<Vec<Value>, String> {
        let mut drained = Vec::new();
        loop {
            match self.incoming.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(message)) => drained.push(message),
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(drained),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(drained),
            }
        }
    }

    pub fn request(&mut self, id: i64, method: &str, params: Vec<Value>) -> Result<(Value, Vec<Value>), String> {
        self.send(&Value::Array(vec![Value::from(0), Value::from(id), Value::from(method), Value::Array(params)]))?;
        let mut stream = Vec::new();
        loop {
            let message = self.read()?;
            let done = response_id(&message) == Some(id);
            stream.push(message.clone());
            if done { return Ok((message, stream)); }
        }
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() { let _ = reader.join(); }
    }
}

pub fn run_session(program: &Path, steps: &[Step]) -> Result<Vec<Value>, String> {
    let mut child = Embedded::spawn(program)?;
    let mut stream = Vec::new();
    for step in steps {
        match step {
            Step::Send { send } => child.send(&yaml_to_msgpack(send)?)?,
            Step::ExpectResponse { expect_response } => {
                if stream.iter().any(|message| response_id(message) == Some(*expect_response)) {
                    continue;
                }
                loop {
                    let message = child.read()?;
                    let matched = response_id(&message) == Some(*expect_response);
                    stream.push(message);
                    if matched { break; }
                }
            },
            Step::ExpectNotification { expect_notification } => {
                if stream.iter().any(|message| notification_satisfies(expect_notification, message)) {
                    continue;
                }
                loop {
                    let message = child.read()?;
                    let matched = notification_satisfies(expect_notification, &message);
                    stream.push(message);
                    if matched { break; }
                }
            },
        }
    }
    stream.extend(child.drain_quiescent()?);
    Ok(stream.into_iter().map(normalize).collect())
}

pub fn load_session(path: &Path) -> Result<Vec<Step>, String> {
    let source = fs::read_to_string(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    serde_yaml::from_str(&source).map_err(|error| format!("could not parse {}: {error}", path.display()))
}

pub fn response_id(message: &Value) -> Option<i64> {
    let fields = message.as_array()?;
    (fields.first()?.as_i64() == Some(1)).then(|| fields.get(1)?.as_i64()).flatten()
}

pub fn notification_method(message: &Value) -> Option<&str> {
    let fields = message.as_array()?;
    (fields.first()?.as_i64() == Some(2)).then(|| fields.get(1)?.as_str()).flatten()
}

fn yaml_to_msgpack(value: &serde_yaml::Value) -> Result<Value, String> {
    match value {
        serde_yaml::Value::Null => Ok(Value::Nil),
        serde_yaml::Value::Bool(value) => Ok(Value::Boolean(*value)),
        serde_yaml::Value::Number(value) => {
            if let Some(value) = value.as_i64() { return Ok(Value::from(value)); }
            if let Some(value) = value.as_u64() { return Ok(Value::from(value)); }
            value.as_f64().map(Value::F64).ok_or_else(|| "unsupported YAML number".to_owned())
        }
        serde_yaml::Value::String(value) => Ok(Value::from(value.as_str())),
        serde_yaml::Value::Sequence(values) => values.iter().map(yaml_to_msgpack).collect::<Result<Vec<_>, _>>().map(Value::Array),
        serde_yaml::Value::Mapping(values) => values
            .iter()
            .map(|(key, value)| Ok((yaml_to_msgpack(key)?, yaml_to_msgpack(value)?)))
            .collect::<Result<Vec<_>, String>>()
            .map(Value::Map),
        serde_yaml::Value::Tagged(_) => Err("tagged YAML values are not supported".to_owned()),
    }
}

fn notification_satisfies(method: &str, message: &Value) -> bool {
    if notification_method(message) != Some(method) { return false; }
    if method != "redraw" { return true; }
    message
        .as_array()
        .and_then(|fields| fields.get(2))
        .and_then(Value::as_array)
        .and_then(|events| events.last())
        .and_then(Value::as_array)
        .and_then(|event| event.first())
        .and_then(Value::as_str)
        == Some("flush")
}

pub fn read_skips() -> io::Result<Vec<String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("SKIPS.md");
    Ok(fs::read_to_string(path)?.lines().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_sorts_maps_and_allows_only_string_build() {
        let left = Value::Map(vec![(Value::from("z"), Value::from(1)), (Value::from("a"), Value::from(2))]);
        let right = Value::Map(vec![(Value::from("a"), Value::from(2)), (Value::from("z"), Value::from(1))]);
        assert_eq!(normalize(left), normalize(right));

        let string_build = Value::Map(vec![(Value::from("version"), Value::Map(vec![(Value::from("build"), Value::from("one"))]))]);
        let other_string = Value::Map(vec![(Value::from("version"), Value::Map(vec![(Value::from("build"), Value::from("two"))]))]);
        assert_eq!(normalize_api(string_build), normalize_api(other_string));

        let integer_build = Value::Map(vec![(Value::from("version"), Value::Map(vec![(Value::from("build"), Value::from(1))]))]);
        assert_ne!(normalize_api(integer_build), normalize_api(Value::Map(vec![(Value::from("version"), Value::Map(vec![(Value::from("build"), Value::from(2))]))])));
    }

    #[test]
    fn readable_diff_exposes_semantic_values() {
        let diff = readable_diff("oracle", &Value::from(1), "oxvim", &Value::from(2));
        assert!(diff.contains("--- oracle"));
        assert!(diff.contains("+++ oxvim"));
        assert!(diff.contains("-1"));
        assert!(diff.contains("+2"));
    }
}
