//! End-to-end MessagePack smoke coverage for the embedded stdio server.

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use rmpv::Value;

struct Embedded {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: i64,
}

impl Embedded {
    fn spawn() -> Self {
        let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        let mut child = Command::new(env!("CARGO_BIN_EXE_oxvim"))
            .arg("--embed")
            .env("OXVIM_RUNTIME", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn oxvim --embed");
        let input = child.stdin.take().expect("embedded stdin");
        let output = BufReader::new(child.stdout.take().expect("embedded stdout"));
        Self { child, input, output, next_id: 1 }
    }

    fn notify(&mut self, method: &str, params: Vec<Value>) {
        let message = Value::Array(vec![
            Value::from(2),
            Value::from(method),
            Value::Array(params),
        ]);
        rmpv::encode::write_value(&mut self.input, &message).expect("encode notification");
        self.input.flush().expect("flush notification");
    }

    fn request(&mut self, method: &str, params: Vec<Value>) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = Value::Array(vec![
            Value::from(0),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        rmpv::encode::write_value(&mut self.input, &request).expect("encode request");
        self.input.flush().expect("flush request");
        let response = rmpv::decode::read_value(&mut self.output).expect("decode response");
        let Value::Array(fields) = response else { panic!("response is not an array") };
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], Value::from(1));
        assert_eq!(fields[1], Value::from(id));
        assert_eq!(fields[2], Value::Nil, "RPC error: {:?}", fields[2]);
        fields[3].clone()
    }

    fn next_message(&mut self) -> Value {
        rmpv::decode::read_value(&mut self.output).expect("decode message")
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn map_get<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Map(entries) = value else { panic!("expected map") };
    entries
        .iter()
        .find_map(|(candidate, value)| (candidate.as_str() == Some(key)).then_some(value))
        .unwrap_or_else(|| panic!("missing map key {key}"))
}

#[test]
fn embedded_stdio_serves_core_rpc_contracts() {
    let mut oxvim = Embedded::spawn();

    let info = oxvim.request("nvim_get_api_info", vec![]);
    let Value::Array(info) = info else { panic!("api info is not an array") };
    assert_eq!(info[0], Value::from(1));
    assert_eq!(map_get(&info[1], "version").as_map().and_then(|version| {
        version.iter().find_map(|(key, value)| (key.as_str() == Some("api_level")).then_some(value))
    }), Some(&Value::from(15)));

    assert_eq!(
        oxvim.request(
            "nvim_buf_set_lines",
            vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true), Value::Array(vec![Value::from("ox"), Value::from("vim")])],
        ),
        Value::Nil,
    );
    assert_eq!(
        oxvim.request("nvim_buf_get_lines", vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)]),
        Value::Array(vec![Value::from("ox"), Value::from("vim")]),
    );

    assert_eq!(
        oxvim.request("nvim_exec_lua", vec![Value::from("return vim.fn.has('nvim-0.13')"), Value::Array(vec![])]),
        Value::from(1),
    );

    assert_eq!(oxvim.request("nvim_command", vec![Value::from("normal! ggdd")]), Value::Nil);
    assert_eq!(
        oxvim.request("nvim_buf_get_lines", vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)]),
        Value::Array(vec![Value::from("vim")]),
    );
}

#[test]
fn failed_notification_emits_nvim_error_event() {
    let mut oxvim = Embedded::spawn();

    oxvim.notify("missing", vec![]);
    let message = oxvim.next_message();
    let Value::Array(fields) = message else { panic!("error event is not an array") };
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0], Value::from(2));
    assert_eq!(fields[1], Value::from("nvim_error_event"));
    let Value::Array(args) = &fields[2] else { panic!("error event params are not an array") };
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], Value::from(0));
    assert_eq!(args[1], Value::from("Invalid method: missing"));
}

#[test]
fn valid_notification_produces_no_response() {
    let mut oxvim = Embedded::spawn();

    // A notification that succeeds must not write anything to stdout.  We
    // prove this by following it with a request: if the notification had
    // emitted a stray frame, `request()` would decode that frame instead of
    // the expected response and panic.
    oxvim.notify("nvim_get_api_info", vec![]);
    let info = oxvim.request("nvim_get_api_info", vec![]);
    let Value::Array(info) = info else { panic!("api info is not an array") };
    assert_eq!(info[0], Value::from(1));
}
