//! Blocking stdio msgpack-RPC client for an embedded Oxvim child.
//!
//! Stdout is decoded on one reader thread and forwarded over a channel. This
//! keeps the public API blocking while allowing redraw notifications to arrive
//! during a synchronous request without losing or reordering them. Stderr is
//! drained concurrently so a verbose child cannot fill its pipe and deadlock;
//! captured bytes are returned with process-edge errors for the terminal owner
//! to report after restoring terminal state.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ox_rpc::{DecodeError, IncrementalDecoder, Message, MsgidCounter, RedrawEvent};
use ox_types::{ApiError, Dict, Object, OxStr};

const READ_BUFFER_SIZE: usize = 16 * 1024;

/// Failure at the embedded-child or msgpack-RPC boundary.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The configured embedded process could not be started.
    #[error("could not start the embedded editor: {source}")]
    Spawn {
        /// Operating-system spawn failure.
        #[source]
        source: io::Error,
    },
    /// A required stdio pipe was not made available by the child process.
    #[error("embedded editor did not expose piped {0}")]
    MissingPipe(&'static str),
    /// A transport worker could not be started.
    #[error("could not start the {worker} worker: {source}")]
    ThreadSpawn {
        /// Worker role.
        worker: &'static str,
        /// Operating-system thread creation failure.
        #[source]
        source: io::Error,
    },
    /// A request could not be written to the child.
    #[error("could not write an RPC request: {source}")]
    Write {
        /// Standard-input write failure.
        #[source]
        source: io::Error,
    },
    /// The child's stdout could not be read.
    #[error("could not read the RPC stream: {source}")]
    Read {
        /// Standard-output read failure.
        #[source]
        source: io::Error,
    },
    /// The child emitted malformed msgpack-RPC.
    #[error("could not decode the RPC stream: {0}")]
    Decode(#[from] DecodeError),
    /// The reader stopped without delivering its terminal event.
    #[error("the RPC reader stopped unexpectedly")]
    ReaderStopped,
    /// The child closed its RPC stream.
    #[error("the embedded editor closed its RPC stream (exit code {exit_code:?})")]
    Eof {
        /// Exit code when the child had already exited.
        exit_code: Option<i32>,
        /// Child diagnostics captured before the error was returned.
        stderr: Vec<u8>,
    },
    /// A response referred to a request other than the one being awaited.
    #[error("received response {actual} while waiting for request {expected}")]
    UnexpectedResponse {
        /// Outstanding request id.
        expected: u32,
        /// Received request id.
        actual: u32,
    },
    /// The child sent a message shape unsupported at the UI client boundary.
    #[error("invalid RPC protocol state: {0}")]
    Protocol(String),
    /// The remote API returned an exception or validation error.
    #[error("embedded editor rejected the RPC request: {0}")]
    Remote(#[from] ApiError),
    /// Waiting for the child process failed.
    #[error("could not wait for the embedded editor: {source}")]
    Wait {
        /// Operating-system wait failure.
        #[source]
        source: io::Error,
    },
    /// The child exited unsuccessfully.
    #[error("embedded editor exited unsuccessfully (exit code {exit_code:?})")]
    NonZeroExit {
        /// Process exit code, or `None` when terminated by a signal.
        exit_code: Option<i32>,
        /// Complete captured child diagnostics.
        stderr: Vec<u8>,
    },
    /// A transport worker panicked while the client was shutting down.
    #[error("the {0} worker panicked")]
    WorkerPanicked(&'static str),
}

/// A synchronous client connected to an embedded editor over stdio.
pub struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: mpsc::Receiver<ReaderEvent>,
    reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    msgids: MsgidCounter,
    redraws: VecDeque<Vec<RedrawEvent>>,
}

impl Client {
    /// Spawn `command` with piped stdin, stdout, and stderr.
    pub fn spawn(mut command: Command) -> Result<Self, ClientError> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| ClientError::Spawn { source })?;

        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => return missing_pipe(&mut child, "stdin"),
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return missing_pipe(&mut child, "stdout"),
        };
        let child_stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => return missing_pipe(&mut child, "stderr"),
        };

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_sink = Arc::clone(&stderr);
        let stderr_reader = thread::Builder::new()
            .name("ox-tui-stderr".into())
            .spawn(move || drain_stderr(child_stderr, stderr_sink))
            .map_err(|source| {
                terminate_child(&mut child);
                ClientError::ThreadSpawn { worker: "stderr", source }
            })?;

        let (sender, incoming) = mpsc::channel();
        let reader = match thread::Builder::new()
            .name("ox-tui-rpc-reader".into())
            .spawn(move || read_messages(stdout, sender))
        {
            Ok(reader) => reader,
            Err(source) => {
                terminate_child(&mut child);
                let _ = stderr_reader.join();
                return Err(ClientError::ThreadSpawn { worker: "RPC reader", source });
            }
        };

        Ok(Self {
            child,
            stdin: Some(stdin),
            incoming,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            stderr,
            msgids: MsgidCounter::new(),
            redraws: VecDeque::new(),
        })
    }

    /// Send an arbitrary API request and block for its exactly matched response.
    ///
    /// Redraw notifications received while waiting are retained for
    /// [`Self::recv_redraw`]. Only one request is outstanding because all
    /// mutation requires `&mut self`.
    pub fn request(&mut self, method: OxStr, params: Vec<Object>) -> Result<Object, ClientError> {
        let (msgid, request) = make_request(&mut self.msgids, method, params);
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            ClientError::Protocol("cannot send a request after stdin has closed".into())
        })?;
        write_message(stdin, &request)?;

        loop {
            let message = self.next_message()?;
            if let Some(result) = handle_request_message(msgid, message, &mut self.redraws)? {
                return Ok(result);
            }
        }
    }

    /// Attach the external UI with the complete bundled-client extension set.
    pub fn attach(&mut self, width: u16, height: u16) -> Result<(), ClientError> {
        let result = self.request(OxStr::from("nvim_ui_attach"), attach_params(width, height))?;
        require_nil("nvim_ui_attach", result)
    }

    /// Forward terminal input and return the number of bytes consumed.
    pub fn input(&mut self, input: OxStr) -> Result<usize, ClientError> {
        let result = self.request(OxStr::from("nvim_input"), vec![Object::String(input)])?;
        let Object::Integer(consumed) = result else {
            return Err(ClientError::Protocol("nvim_input returned a non-integer result".into()));
        };
        usize::try_from(consumed)
            .map_err(|_| ClientError::Protocol("nvim_input returned a negative byte count".into()))
    }

    /// Send a mouse event to the server at terminal coordinates.
    ///
    /// Grid `0` lets the server decide which window the position targets, as
    /// documented for multigrid clients; the caller suppresses chrome-owned
    /// coordinates before calling this.
    pub fn input_mouse(
        &mut self,
        button: &str,
        action: &str,
        modifier: &str,
        row: u16,
        column: u16,
    ) -> Result<(), ClientError> {
        let result = self.request(
            OxStr::from("nvim_input_mouse"),
            vec![
                Object::String(OxStr::from(button)),
                Object::String(OxStr::from(action)),
                Object::String(OxStr::from(modifier)),
                Object::Integer(0),
                Object::Integer(i64::from(row)),
                Object::Integer(i64::from(column)),
            ],
        )?;
        require_nil("nvim_input_mouse", result)
    }

    /// Notify the server that the terminal grid changed size.
    pub fn try_resize(&mut self, width: u16, height: u16) -> Result<(), ClientError> {
        let result = self.request(
            OxStr::from("nvim_ui_try_resize"),
            vec![Object::Integer(i64::from(width)), Object::Integer(i64::from(height))],
        )?;
        require_nil("nvim_ui_try_resize", result)
    }

    /// Block until the next decoded redraw notification is available.
    pub fn recv_redraw(&mut self) -> Result<Vec<RedrawEvent>, ClientError> {
        if let Some(events) = self.redraws.pop_front() {
            return Ok(events);
        }
        loop {
            let message = self.next_message()?;
            match message {
                Message::Notification { method, params } if method.as_bytes() == b"redraw" => {
                    return parse_redraw(params);
                }
                Message::Notification { .. } => {}
                Message::Response { msgid, .. } => {
                    return Err(ClientError::Protocol(format!(
                        "received response {msgid} with no outstanding request"
                    )));
                }
                Message::Request { msgid, .. } => {
                    return Err(ClientError::Protocol(format!(
                        "received unsupported inbound request {msgid}"
                    )));
                }
            }
        }
    }

    /// Wait at most `timeout` for a redraw batch.
    ///
    /// A timeout is not an RPC failure; it gives the terminal loop a chance to
    /// process input and resize events without introducing a second client owner.
    pub fn recv_redraw_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Vec<RedrawEvent>>, ClientError> {
        if let Some(events) = self.redraws.pop_front() {
            return Ok(Some(events));
        }
        loop {
            let message = match self.incoming.recv_timeout(timeout) {
                Ok(ReaderEvent::Message(message)) => message,
                Ok(ReaderEvent::Decode(error)) => return Err(ClientError::Decode(error)),
                Ok(ReaderEvent::Read(source)) => return Err(ClientError::Read { source }),
                Ok(ReaderEvent::Eof) => return Err(self.eof_error()),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ClientError::ReaderStopped);
                }
            };
            match message {
                Message::Notification { method, params } if method.as_bytes() == b"redraw" => {
                    return parse_redraw(params).map(Some);
                }
                Message::Notification { .. } => {}
                Message::Response { msgid, .. } => {
                    return Err(ClientError::Protocol(format!(
                        "received response {msgid} with no outstanding request"
                    )));
                }
                Message::Request { msgid, .. } => {
                    return Err(ClientError::Protocol(format!(
                        "received unsupported inbound request {msgid}"
                    )));
                }
            }
        }
    }

    /// Return a snapshot of stderr captured so far.
    #[must_use]
    pub fn stderr(&self) -> Vec<u8> {
        stderr_snapshot(&self.stderr)
    }

    /// Close the RPC input, wait for the child, and join transport workers.
    ///
    /// A nonzero exit is returned with complete stderr instead of terminating
    /// this process, allowing the upper layer to restore terminal state first.
    pub fn shutdown(mut self) -> Result<(), ClientError> {
        self.stdin.take();
        let status = self.child.wait().map_err(|source| ClientError::Wait { source })?;
        join_worker(&mut self.reader, "RPC reader")?;
        join_worker(&mut self.stderr_reader, "stderr")?;
        if status.success() {
            Ok(())
        } else {
            Err(ClientError::NonZeroExit {
                exit_code: status.code(),
                stderr: stderr_snapshot(&self.stderr),
            })
        }
    }

    fn next_message(&mut self) -> Result<Message, ClientError> {
        match self.incoming.recv() {
            Ok(ReaderEvent::Message(message)) => Ok(message),
            Ok(ReaderEvent::Decode(error)) => Err(ClientError::Decode(error)),
            Ok(ReaderEvent::Read(source)) => Err(ClientError::Read { source }),
            Ok(ReaderEvent::Eof) => Err(self.eof_error()),
            Err(_) => Err(ClientError::ReaderStopped),
        }
    }

    fn eof_error(&mut self) -> ClientError {
        let status = match self.child.try_wait() {
            Ok(Some(status)) => Some(status),
            Ok(None) => {
                let _ = self.child.kill();
                self.child.wait().ok()
            }
            Err(_) => {
                terminate_child(&mut self.child);
                None
            }
        };
        let _ = join_worker(&mut self.stderr_reader, "stderr");
        ClientError::Eof {
            exit_code: status.and_then(|status| status.code()),
            stderr: stderr_snapshot(&self.stderr),
        }
    }
}

#[derive(Debug)]
enum ReaderEvent {
    Message(Message),
    Decode(DecodeError),
    Read(io::Error),
    Eof,
}

fn write_message(writer: &mut impl Write, message: &Message) -> Result<(), ClientError> {
    writer
        .write_all(&message.encode_bytes())
        .and_then(|()| writer.flush())
        .map_err(|source| ClientError::Write { source })
}

fn make_request(
    counter: &mut MsgidCounter,
    method: OxStr,
    params: Vec<Object>,
) -> (u32, Message) {
    let msgid = counter.next();
    (msgid, Message::Request { msgid, method, params })
}

fn read_messages(mut stdout: impl Read, sender: mpsc::Sender<ReaderEvent>) {
    let mut decoder = IncrementalDecoder::new();
    let mut buffer = [0; READ_BUFFER_SIZE];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => {
                let event = if decoder.is_empty() {
                    ReaderEvent::Eof
                } else {
                    ReaderEvent::Decode(DecodeError::Incomplete)
                };
                let _ = sender.send(event);
                return;
            }
            Ok(read) => match decoder.feed(&buffer[..read]) {
                Ok(messages) => {
                    for message in messages {
                        if sender.send(ReaderEvent::Message(message)).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(ReaderEvent::Decode(error));
                    return;
                }
            },
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => {
                let _ = sender.send(ReaderEvent::Read(source));
                return;
            }
        }
    }
}

fn drain_stderr(mut stderr: impl Read, sink: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0; 4096];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => match sink.lock() {
                Ok(mut bytes) => bytes.extend_from_slice(&buffer[..read]),
                Err(poisoned) => poisoned.into_inner().extend_from_slice(&buffer[..read]),
            },
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn handle_request_message(
    expected: u32,
    message: Message,
    redraws: &mut VecDeque<Vec<RedrawEvent>>,
) -> Result<Option<Object>, ClientError> {
    match message {
        Message::Response { msgid, result } if msgid == expected => {
            result.map(Some).map_err(ClientError::Remote)
        }
        Message::Response { msgid, .. } => {
            Err(ClientError::UnexpectedResponse { expected, actual: msgid })
        }
        Message::Notification { method, params } => {
            if method.as_bytes() == b"redraw" {
                redraws.push_back(parse_redraw(params)?);
            }
            Ok(None)
        }
        Message::Request { msgid, .. } => Err(ClientError::Protocol(format!(
            "received unsupported inbound request {msgid}"
        ))),
    }
}

fn parse_redraw(entries: Vec<Object>) -> Result<Vec<RedrawEvent>, ClientError> {
    let mut events = Vec::with_capacity(entries.len());
    for entry in entries {
        let Object::Array(mut fields) = entry else {
            return Err(ClientError::Protocol("redraw event must be an array".into()));
        };
        if fields.is_empty() {
            return Err(ClientError::Protocol("redraw event cannot be empty".into()));
        }
        let Object::String(name) = fields.remove(0) else {
            return Err(ClientError::Protocol("redraw event name must be a string".into()));
        };
        let mut argsets = Vec::with_capacity(fields.len());
        for field in fields {
            let Object::Array(args) = field else {
                return Err(ClientError::Protocol("redraw event arguments must be arrays".into()));
            };
            argsets.push(args);
        }
        events.push(RedrawEvent { name, argsets });
    }
    Ok(events)
}

fn attach_params(width: u16, height: u16) -> Vec<Object> {
    let enabled = [
        "ext_linegrid",
        "ext_multigrid",
        "ext_cmdline",
        "ext_messages",
        "ext_popupmenu",
        "ext_hlstate",
        "rgb",
        "ext_termcolors",
    ];
    let options = enabled
        .into_iter()
        .map(|name| (OxStr::from(name), Object::Boolean(true)))
        .collect();
    vec![
        Object::Integer(i64::from(width)),
        Object::Integer(i64::from(height)),
        Object::Dict(Dict(options)),
    ]
}

fn require_nil(method: &str, result: Object) -> Result<(), ClientError> {
    if result == Object::Nil {
        Ok(())
    } else {
        Err(ClientError::Protocol(format!("{method} returned a non-nil result")))
    }
}

fn stderr_snapshot(stderr: &Mutex<Vec<u8>>) -> Vec<u8> {
    match stderr.lock() {
        Ok(bytes) => bytes.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn join_worker(handle: &mut Option<JoinHandle<()>>, worker: &'static str) -> Result<(), ClientError> {
    let Some(handle) = handle.take() else {
        return Ok(());
    };
    handle.join().map_err(|_| ClientError::WorkerPanicked(worker))
}

fn missing_pipe<T>(child: &mut Child, name: &'static str) -> Result<T, ClientError> {
    terminate_child(child);
    Err(ClientError::MissingPipe(name))
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

    use super::*;

    fn response(msgid: u32, result: Object) -> Message {
        Message::Response { msgid, result: Ok(result) }
    }

    #[test]
    fn attach_request_has_exact_extension_set() {
        let request = Message::Request {
            msgid: 1,
            method: OxStr::from("nvim_ui_attach"),
            params: attach_params(120, 40),
        };
        let mut wire = Vec::new();
        write_message(&mut wire, &request).unwrap();
        let mut decoder = IncrementalDecoder::new();
        let decoded = decoder.feed(&wire).unwrap();
        assert_eq!(decoded, vec![request]);

        let Message::Request { params, .. } = &decoded[0] else {
            panic!("fixture must decode as a request");
        };
        assert_eq!(params[0], Object::Integer(120));
        assert_eq!(params[1], Object::Integer(40));
        let Object::Dict(options) = &params[2] else {
            panic!("attach options must be a dictionary");
        };
        let names: Vec<&[u8]> = options.iter().map(|(name, _)| name.as_bytes()).collect();
        assert_eq!(
            names,
            [
                b"ext_linegrid".as_slice(),
                b"ext_multigrid".as_slice(),
                b"ext_cmdline".as_slice(),
                b"ext_messages".as_slice(),
                b"ext_popupmenu".as_slice(),
                b"ext_hlstate".as_slice(),
                b"rgb".as_slice(),
                b"ext_termcolors".as_slice(),
            ]
        );
        assert!(options.iter().all(|(_, value)| value == &Object::Boolean(true)));
    }

    #[test]
    fn matching_response_returns_result_and_wrong_id_is_typed() {
        let mut redraws = VecDeque::new();
        let result = handle_request_message(7, response(7, Object::Integer(3)), &mut redraws)
            .unwrap();
        assert_eq!(result, Some(Object::Integer(3)));

        let error = handle_request_message(7, response(8, Object::Nil), &mut redraws)
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::UnexpectedResponse { expected: 7, actual: 8 }
        ));
    }

    #[test]
    fn writer_fixture_observes_monotonic_request_ids() {
        let mut counter = MsgidCounter::new();
        let (_, first) = make_request(&mut counter, OxStr::from("nvim_input"), vec![]);
        let (_, second) = make_request(&mut counter, OxStr::from("nvim_ui_try_resize"), vec![]);
        let mut wire = Vec::new();
        write_message(&mut wire, &first).unwrap();
        write_message(&mut wire, &second).unwrap();

        let mut decoder = IncrementalDecoder::new();
        let decoded = decoder.feed(&wire).unwrap();
        assert!(matches!(decoded[0], Message::Request { msgid: 1, .. }));
        assert!(matches!(decoded[1], Message::Request { msgid: 2, .. }));
    }

    #[test]
    fn redraw_is_queued_while_waiting_for_response() {
        let redraw = Message::Notification {
            method: OxStr::from("redraw"),
            params: vec![Object::Array(vec![
                Object::String(OxStr::from("flush")),
                Object::Array(vec![]),
            ])],
        };
        let mut decoder = IncrementalDecoder::new();
        let mut decoded = decoder.feed(&redraw.encode_bytes()).unwrap();
        let decoded_redraw = decoded.remove(0);
        let mut queued = VecDeque::new();
        assert_eq!(
            handle_request_message(2, decoded_redraw, &mut queued).unwrap(),
            None
        );
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0][0].name, OxStr::from("flush"));
        assert_eq!(queued[0][0].argsets, vec![Vec::<Object>::new()]);
        assert_eq!(
            handle_request_message(2, response(2, Object::Nil), &mut queued).unwrap(),
            Some(Object::Nil)
        );
    }

    #[test]
    fn partial_frame_at_eof_is_a_decode_error() {
        let request = Message::Request {
            msgid: 1,
            method: OxStr::from("nvim_input"),
            params: vec![Object::String(OxStr::from("x"))],
        };
        let mut bytes = request.encode_bytes();
        bytes.pop();
        let (sender, receiver) = mpsc::channel();
        read_messages(bytes.as_slice(), sender);
        assert!(matches!(receiver.recv().unwrap(), ReaderEvent::Decode(DecodeError::Incomplete)));
    }

    #[test]
    fn spawn_failure_is_typed() {
        let command = Command::new("/path/that/does/not/exist/oxvim");
        let error = Client::spawn(command).err().expect("spawn must fail");
        assert!(matches!(error, ClientError::Spawn { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn eof_reports_exit_code_and_captured_stderr() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf child-eof >&2"]);
        let mut client = Client::spawn(command).unwrap();
        let error = client.recv_redraw().unwrap_err();
        match error {
            ClientError::Eof { exit_code, stderr } => {
                assert!(exit_code.is_none() || exit_code == Some(0));
                assert_eq!(stderr, b"child-eof");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_reports_nonzero_exit_and_captured_stderr() {
        let mut command = Command::new("sh");
        command.args(["-c", "cat >/dev/null; printf child-failed >&2; exit 7"]);
        let client = Client::spawn(command).unwrap();
        let error = client.shutdown().unwrap_err();
        match error {
            ClientError::NonZeroExit { exit_code, stderr } => {
                assert_eq!(exit_code, Some(7));
                assert_eq!(stderr, b"child-failed");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
