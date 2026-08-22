//! End-to-end MessagePack smoke coverage for the embedded stdio server.

use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::collections::VecDeque;

use rmpv::Value;

struct Embedded {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: i64,
    pending: VecDeque<Value>,
}

impl Embedded {
    fn spawn() -> Self {
        Self::spawn_with(&[])
    }

    fn spawn_with(arguments: &[&str]) -> Self {
        let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxvim"));
        command.arg("--embed").args(arguments).env("OXVIM_RUNTIME", runtime);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn oxvim --embed");
        let input = child.stdin.take().expect("embedded stdin");
        let output = BufReader::new(child.stdout.take().expect("embedded stdout"));
        Self { child, input, output, next_id: 1, pending: VecDeque::new() }
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
        loop {
            let response = rmpv::decode::read_value(&mut self.output).expect("decode response");
            let Value::Array(fields) = response else { panic!("response is not an array") };
            if fields.first() == Some(&Value::from(2)) {
                self.pending.push_back(Value::Array(fields));
                continue;
            }
            assert_eq!(fields.len(), 4);
            assert_eq!(fields[0], Value::from(1));
            assert_eq!(fields[1], Value::from(id));
            assert_eq!(fields[2], Value::Nil, "RPC error: {:?}", fields[2]);
            return fields[3].clone();
        }
    }

    fn next_message(&mut self) -> Value {
        self.pending.pop_front().unwrap_or_else(|| {
            rmpv::decode::read_value(&mut self.output).expect("decode message")
        })
    }

    fn request_error(&mut self, method: &str, params: Vec<Value>) -> Value {
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
        loop {
            let response = rmpv::decode::read_value(&mut self.output).expect("decode response");
            let Value::Array(fields) = response else { panic!("response is not an array") };
            if fields.first() == Some(&Value::from(2)) {
                self.pending.push_back(Value::Array(fields));
                continue;
            }
            assert_eq!(fields.len(), 4);
            assert_eq!(fields[0], Value::from(1));
            assert_eq!(fields[1], Value::from(id));
            assert!(!matches!(fields[2], Value::Nil), "RPC did not return an error");
            return fields[2].clone();
        }
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

fn redraw_names(value: &Value) -> Vec<&str> {
    let Value::Array(fields) = value else { panic!("redraw is not an array") };
    assert_eq!(fields[0], Value::from(2));
    assert_eq!(fields[1], Value::from("redraw"));
    let Value::Array(events) = &fields[2] else { panic!("redraw params are not an array") };
    events
        .iter()
        .filter_map(|event| event.as_array()?.first()?.as_str())
        .collect()
}

fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value.as_str() == Some(expected),
        Value::Array(values) => values.iter().any(|value| contains_string(value, expected)),
        Value::Map(entries) => entries
            .iter()
            .any(|(key, value)| contains_string(key, expected) || contains_string(value, expected)),
        _ => false,
    }
}

fn stream_request<W: Write, R: Read>(
    writer: &mut W,
    reader: &mut R,
    id: i64,
    method: &str,
    params: Vec<Value>,
) -> Value {
    let request = Value::Array(vec![
        Value::from(0),
        Value::from(id),
        Value::from(method),
        Value::Array(params),
    ]);
    rmpv::encode::write_value(writer, &request).expect("encode RPC request");
    writer.flush().expect("flush RPC request");
    let response = rmpv::decode::read_value(reader).expect("decode RPC response");
    let Value::Array(fields) = response else { panic!("RPC response is not an array") };
    assert_eq!(fields[0], Value::from(1));
    assert_eq!(fields[1], Value::from(id));
    assert_eq!(fields[2], Value::Nil);
    fields[3].clone()
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

    let atomic = oxvim.request(
        "nvim_call_atomic",
        vec![Value::Array(vec![Value::Array(vec![
            Value::from("nvim_get_api_info"),
            Value::Array(vec![]),
        ])])],
    );
    let Value::Array(atomic) = atomic else { panic!("atomic result is not an array") };
    let Value::Array(results) = &atomic[0] else { panic!("atomic results are not an array") };
    let Value::Array(nested_info) = &results[0] else { panic!("nested API info is not an array") };
    assert_eq!(nested_info[0], Value::from(1));

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
fn lua_variable_and_option_tables_use_editor_state() {
    let mut oxvim = Embedded::spawn();
    let source = r#"
        assert(vim.v.servername == '')
        assert(vim.g.missing == nil)

        vim.g.answer = { value = 42, enabled = true }
        assert(vim.g.answer.value == 42 and vim.g.answer.enabled)
        vim.b.buffer_value = 'buffer'
        vim.w.window_value = 'window'
        vim.t.tab_value = 'tabpage'
        assert(vim.b.buffer_value == 'buffer' and vim.b[1].buffer_value == 'buffer')
        assert(vim.w.window_value == 'window' and vim.w[1].window_value == 'window')
        assert(vim.t.tab_value == 'tabpage' and vim.t[1].tab_value == 'tabpage')

        vim.g.answer = nil
        assert(vim.g.answer == nil)
        local ok, error_message = pcall(function() vim.v.servername = 'changed' end)
        assert(not ok and tostring(error_message):find('E46', 1, true))
        assert(vim.v.servername == '')

        local background = vim.go.background
        vim.o.background = background == 'dark' and 'light' or 'dark'
        assert(vim.o.background == vim.go.background and vim.o.background ~= background)
        local modifiable = vim.bo.modifiable
        vim.bo.modifiable = not modifiable
        assert(vim.bo[0].modifiable == not modifiable)
        local number = vim.wo.number
        vim.wo.number = not number
        assert(vim.wo[0].number == not number)
        return true
    "#;
    assert_eq!(
        oxvim.request(
            "nvim_exec_lua",
            vec![Value::from(source), Value::Array(vec![])],
        ),
        Value::Boolean(true),
    );
}

#[test]
fn terminal_channels_allocate_and_accept_bytes() {
    let mut oxvim = Embedded::spawn();
    let value = oxvim.request(
        "nvim_exec_lua",
        vec![
            Value::from("local c=vim.api.nvim_open_term(0, {}); local sent=vim.api.nvim_chan_send(c, 'echo'); return {c, sent, vim.api.nvim_get_chan_info(c)}"),
            Value::Array(vec![]),
        ],
    );
    let Value::Array(values) = value else { panic!("terminal result is not an array") };
    assert_eq!(values[0], Value::from(3));
    assert_eq!(values[1], Value::Nil);
    assert_eq!(map_get(&values[2], "id"), &Value::from(3));
    assert_eq!(map_get(&values[2], "mode"), &Value::from("terminal"));
    assert_eq!(map_get(&values[2], "stream"), &Value::from("socket"));
    assert_eq!(map_get(&values[2], "buffer"), &Value::from(1));

    assert_eq!(
        oxvim.request(
            "nvim_exec_lua",
            vec![Value::from("return vim.api.nvim_open_term(0, {})"), Value::Array(vec![])],
        ),
        Value::from(4),
    );
    let error = oxvim.request_error("nvim_chan_send", vec![Value::from(99), Value::from("bad")]);
    assert!(contains_string(&error, "Invalid channel: 99"));
}

#[test]
fn attached_ui_receives_flushed_initial_and_mutation_redraws() {
    let mut oxvim = Embedded::spawn();
    assert_eq!(
        oxvim.request(
            "nvim_buf_set_lines",
            vec![
                Value::from(0),
                Value::from(0),
                Value::from(-1),
                Value::Boolean(true),
                Value::Array(vec![Value::from("ox"), Value::from("vim")]),
            ],
        ),
        Value::Nil,
    );
    assert_eq!(
        oxvim.request(
            "nvim_ui_attach",
            vec![
                Value::from(80),
                Value::from(24),
                Value::Map(vec![(Value::from("rgb"), Value::Boolean(true))]),
            ],
        ),
        Value::Nil,
    );
    let initial = oxvim.next_message();
    let initial_names = redraw_names(&initial);
    assert_eq!(initial_names.last(), Some(&"flush"));
    assert!(initial_names.contains(&"option_set"));
    assert!(initial_names.contains(&"default_colors_set"));
    assert!(initial_names.contains(&"hl_attr_define"));
    assert!(initial_names.contains(&"mode_info_set"));
    assert!(contains_string(&initial, "o"));

    assert_eq!(
        oxvim.request("nvim_command", vec![Value::from("normal! dd")]),
        Value::Nil,
    );
    let mutation = oxvim.next_message();
    let mutation_names = redraw_names(&mutation);
    assert!(mutation_names.contains(&"grid_line"));
    assert_eq!(mutation_names.last(), Some(&"flush"));
    assert!(contains_string(&mutation, "v"));
    assert!(!contains_string(&mutation, "o"));
    assert_eq!(
        oxvim.request("nvim_ui_try_resize", vec![Value::from(100), Value::from(30)]),
        Value::Nil,
    );
    let resized = oxvim.next_message();
    assert_eq!(redraw_names(&resized).last(), Some(&"flush"));
    assert_eq!(oxvim.request("nvim_ui_detach", vec![]), Value::Nil);
}

#[test]
fn startup_orders_pre_config_post_and_vimenter_lua_callback() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("oxvim-startup-{}-{unique}.lua", std::process::id()));
    fs::write(
        &path,
        r#"
local function suffix(value)
  local line = vim.api.nvim_buf_get_lines(0, 0, 1, true)[1]
  vim.api.nvim_buf_set_lines(0, 0, 1, true, { line .. value })
end
suffix('-init')
vim.api.nvim_create_autocmd('VimEnter', { callback = function() suffix('-enter') end })
"#,
    )
    .unwrap();
    let path_string = path.to_string_lossy().into_owned();
    let mut oxvim = Embedded::spawn_with(&[
        "--cmd",
        "call setline(1, 'pre')",
        "-u",
        &path_string,
        "+call setline(1, getline(1) . '-post')",
    ]);
    let lines = oxvim.request(
        "nvim_buf_get_lines",
        vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)],
    );
    assert_eq!(lines, Value::Array(vec![Value::from("pre-init-post-enter")]));

    let mut clean = Embedded::spawn_with(&["--clean", "-u", &path_string]);
    let clean_lines = clean.request(
        "nvim_buf_get_lines",
        vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)],
    );
    assert_eq!(clean_lines, Value::Array(vec![Value::from("")]));
    fs::remove_file(path).unwrap();
}

#[test]
fn tcp_listener_allocates_dynamic_channel_and_serves_api_info() {
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve TCP address");
    let address = reservation.local_addr().expect("reserved address");
    drop(reservation);
    let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxvim"))
        .args(["--headless", "--listen", &address.to_string()])
        .env("OXVIM_RUNTIME", runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn TCP listener");
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("connect TCP listener: {error}"),
        }
    };
    let mut first_writer = stream.try_clone().expect("clone first TCP stream");
    let mut first_reader = BufReader::new(stream);
    let first_info = stream_request(&mut first_writer, &mut first_reader, 1, "nvim_get_api_info", vec![]);
    let Value::Array(first_info) = first_info else { panic!("API info is not an array") };
    assert_eq!(first_info[0], Value::from(3));
    assert_eq!(
        stream_request(
            &mut first_writer,
            &mut first_reader,
            2,
            "nvim_exec_lua",
            vec![
                Value::from("return vim.v.servername"),
                Value::Array(vec![]),
            ],
        ),
        Value::from(address.to_string()),
    );

    let second = TcpStream::connect(address).expect("connect second TCP peer");
    let mut second_writer = second.try_clone().expect("clone second TCP stream");
    let mut second_reader = BufReader::new(second);
    let second_info = stream_request(&mut second_writer, &mut second_reader, 1, "nvim_get_api_info", vec![]);
    let Value::Array(second_info) = second_info else { panic!("API info is not an array") };
    assert_eq!(second_info[0], Value::from(4));

    assert_eq!(
        stream_request(
            &mut first_writer,
            &mut first_reader,
            3,
            "nvim_buf_set_lines",
            vec![
                Value::from(0),
                Value::from(0),
                Value::from(-1),
                Value::Boolean(true),
                Value::Array(vec![Value::from("shared")]),
            ],
        ),
        Value::Nil,
    );
    assert_eq!(
        stream_request(
            &mut second_writer,
            &mut second_reader,
            2,
            "nvim_buf_get_lines",
            vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)],
        ),
        Value::Array(vec![Value::from("shared")]),
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn pipe_listener_reuses_requested_address_and_reports_servername() {
    use std::os::unix::net::UnixStream;

    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let socket = std::env::temp_dir().join(format!("oxvim-listen-{}-{unique}.sock", std::process::id()));
    fs::write(&socket, b"stale").expect("create stale listen path");
    let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");

    for generation in 0..2 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oxvim"))
            .args(["--headless", "--listen", socket.to_str().unwrap()])
            .env("OXVIM_RUNTIME", &runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn pipe listener");
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(&socket) {
                Ok(stream) => break stream,
                Err(error) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                    let _ = error;
                }
                Err(error) => panic!("connect generation {generation} pipe listener: {error}"),
            }
        };
        let mut writer = stream.try_clone().expect("clone pipe stream");
        let mut reader = BufReader::new(stream);
        assert_eq!(
            stream_request(
                &mut writer,
                &mut reader,
                1,
                "nvim_exec_lua",
                vec![Value::from("return vim.v.servername"), Value::Array(vec![])],
            ),
            Value::from(socket.to_string_lossy().into_owned()),
        );
        let quit = Value::Array(vec![
            Value::from(2),
            Value::from("nvim_command"),
            Value::Array(vec![Value::from("qa!")]),
        ]);
        rmpv::encode::write_value(&mut writer, &quit).expect("encode quit notification");
        writer.flush().expect("flush quit notification");
        let exit_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("poll pipe listener") {
                assert!(status.success(), "generation {generation} exited with {status}");
                break;
            }
            assert!(Instant::now() < exit_deadline, "generation {generation} did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!socket.exists(), "generation {generation} left its socket path behind");
    }
}

#[cfg(unix)]
#[test]
fn embedded_listener_serves_stdio_and_cleans_up_on_exit() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let socket = std::env::temp_dir().join(format!("oxvim-embed-listen-{}-{unique}.sock", std::process::id()));
    fs::write(&socket, b"stale").expect("create stale embedded listen path");
    let socket_text = socket.to_string_lossy().into_owned();
    let mut oxvim = Embedded::spawn_with(&["--headless", "--listen", &socket_text]);

    assert_eq!(
        oxvim.request(
            "nvim_exec_lua",
            vec![Value::from("return vim.v.servername"), Value::Array(vec![])],
        ),
        Value::from(socket_text),
    );
    oxvim.notify("nvim_command", vec![Value::from("qa!")]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = oxvim.child.try_wait().expect("poll embedded listener") {
            assert!(status.success(), "embedded listener exited with {status}");
            break;
        }
        assert!(Instant::now() < deadline, "embedded listener did not exit after stdin quit");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists(), "embedded listener left its socket path behind");
}

#[cfg(unix)]
#[test]
fn embedded_listener_exits_when_stdio_reaches_eof() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let socket = std::env::temp_dir().join(format!("oxvim-embed-eof-{}-{unique}.sock", std::process::id()));
    let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxvim"))
        .args(["--embed", "--headless", "--listen", socket.to_str().unwrap()])
        .env("OXVIM_RUNTIME", runtime)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn embedded EOF listener");
    let mut input = child.stdin.take().expect("embedded EOF stdin");
    let mut output = BufReader::new(child.stdout.take().expect("embedded EOF stdout"));
    assert_eq!(
        stream_request(&mut input, &mut output, 1, "nvim_get_vvar", vec![Value::from("servername")]),
        Value::from(socket.to_string_lossy().into_owned()),
    );
    drop(input);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("poll embedded EOF listener") {
            assert!(status.success(), "embedded EOF listener exited with {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("embedded listener did not exit after stdin EOF");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists(), "embedded EOF listener left its socket path behind");
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

#[test]
fn rejected_quit_on_modified_buffer_emits_error_and_redraw() {
    let mut oxvim = Embedded::spawn();
    assert_eq!(
        oxvim.request(
            "nvim_buf_set_lines",
            vec![
                Value::from(0),
                Value::from(0),
                Value::from(-1),
                Value::Boolean(true),
                Value::Array(vec![Value::from("changed")]),
            ],
        ),
        Value::Nil,
    );
    assert_eq!(
        oxvim.request(
            "nvim_ui_attach",
            vec![
                Value::from(80),
                Value::from(24),
                Value::Map(vec![
                    (Value::from("rgb"), Value::Boolean(true)),
                    (Value::from("ext_messages"), Value::Boolean(true)),
                ]),
            ],
        ),
        Value::Nil,
    );
    let _initial = oxvim.next_message();

    // Enter the command line with :q (no <CR> yet).
    assert_eq!(oxvim.request("nvim_input", vec![Value::from(":q")]), Value::from(2));
    let cmdline = oxvim.next_message();
    let cmdline_names = redraw_names(&cmdline);
    assert_eq!(cmdline_names.last(), Some(&"flush"));
    assert!(cmdline_names.contains(&"cmdline_show"), "cmdline_show missing: {cmdline_names:?}");

    // Press <CR>. The quit is rejected because the buffer is modified.
    let error = oxvim.request_error("nvim_input", vec![Value::from("\r")]);
    let Value::Array(error_fields) = error else { panic!("error response is not an array") };
    assert_eq!(error_fields.len(), 2);
    assert_eq!(error_fields[0], Value::from(0));
    let error_text = error_fields[1].as_str().expect("error text is not a string");
    assert!(error_text.contains("E37"), "expected E37, got {error_text}");

    let redraw = oxvim.next_message();
    let redraw_names = redraw_names(&redraw);
    assert_eq!(redraw_names.last(), Some(&"flush"));
    assert!(redraw_names.contains(&"cmdline_hide"), "cmdline_hide missing: {redraw_names:?}");
    assert!(redraw_names.contains(&"msg_show"), "msg_show missing: {redraw_names:?}");
    assert!(contains_string(&redraw, error_text), "redraw should contain the error text");
}

#[test]
fn lua_integration_smoke() {
    let mut oxvim = Embedded::spawn();

    // Call a Lua function that uses vim.api against the current editor.
    assert_eq!(
        oxvim.request("nvim_command", vec![Value::from("lua _G.oxvim_ex_buffer = vim.api.nvim_get_current_buf(); vim.g.oxvim_ex_value = 42")]),
        Value::Nil,
    );
    assert_eq!(
        oxvim.request("nvim_exec_lua", vec![Value::from("return {_G.oxvim_ex_buffer, vim.g.oxvim_ex_value}"), Value::Array(vec![])]),
        Value::Array(vec![Value::from(1), Value::from(42)]),
    );

    // The runtime dynamic wrapper builds nvim_cmd({ cmd = 'highlight', ... }).
    assert_eq!(
        oxvim.request("nvim_command", vec![Value::from("lua vim.cmd.highlight('clear')")]),
        Value::Nil,
    );

    let command = Value::Map(vec![
        (Value::from("cmd"), Value::from("echo")),
        (Value::from("args"), Value::Array(vec![Value::from("'structured output'")])),
    ]);
    let options = Value::Map(vec![(Value::from("output"), Value::Boolean(true))]);
    assert_eq!(
        oxvim.request("nvim_cmd", vec![command, options]),
        Value::from("structured output"),
    );

    let autocmd = Value::Map(vec![
        (Value::from("pattern"), Value::from("*.py,*.pyi")),
        (Value::from("command"), Value::from("echo 'Should Have Errored")),
        (Value::from("callback"), Value::from("NotAllowed")),
    ]);
    let error = oxvim.request_error(
        "nvim_create_autocmd",
        vec![Value::from("BufReadPost"), autocmd],
    );
    assert!(contains_string(&error, "Conflict: 'callback' not allowed with 'command'"), "{error:?}");
}

#[test]
fn startup_file_arguments_become_named_buffers_with_first_current() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let first = std::env::temp_dir().join(format!(
        "oxvim-startup-files-{}-{unique}-first.txt",
        std::process::id()
    ));
    let second = std::env::temp_dir().join(format!(
        "oxvim-startup-files-{}-{unique}-second.txt",
        std::process::id()
    ));
    fs::write(&first, "first buffer\n").unwrap();
    let _ = fs::remove_file(&second);

    let first_string = first.to_string_lossy().into_owned();
    let second_string = second.to_string_lossy().into_owned();
    let mut oxvim = Embedded::spawn_with(&[&first_string, &second_string]);

    // main.c create_windows()/edit_buffers(): one named buffer per file
    let buffers = oxvim.request("nvim_list_bufs", vec![]);
    let Value::Array(handles) = buffers else { panic!("buffer list is not an array") };
    assert_eq!(handles.len(), 2, "one buffer per file argument: {handles:?}");
    let current = oxvim.request("nvim_get_current_buf", vec![]);
    assert_eq!(current, handles[0].clone(), "first file is the current buffer");
    assert_eq!(
        oxvim.request("nvim_buf_get_name", vec![Value::from(0)]),
        Value::from(first_string.clone()),
    );
    assert_eq!(
        oxvim.request(
            "nvim_buf_get_lines",
            vec![Value::from(0), Value::from(0), Value::from(-1), Value::Boolean(true)],
        ),
        Value::Array(vec![Value::from("first buffer")]),
    );
    // A missing file still gets a named empty buffer, like upstream's
    // buffer creation during argument-list setup.
    assert_eq!(
        oxvim.request("nvim_buf_get_name", vec![Value::from(2)]),
        Value::from(second_string.clone()),
    );
    assert_eq!(
        oxvim.request(
            "nvim_buf_get_lines",
            vec![Value::from(2), Value::from(0), Value::from(-1), Value::Boolean(true)],
        ),
        Value::Array(vec![Value::from("")]),
    );

    let _ = fs::remove_file(&first);
}

#[test]
fn write_without_name_targets_current_buffers_file() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let target = std::env::temp_dir().join(format!(
        "oxvim-write-target-{}-{unique}.txt",
        std::process::id()
    ));
    fs::write(&target, "original\n").unwrap();
    let target_string = target.to_string_lossy().into_owned();

    let mut oxvim = Embedded::spawn_with(&[&target_string]);
    // The oldtest runner pattern: replace the file buffer with a scratch
    // buffer, wipe the scratch, then `:write` must fall back onto the
    // remaining named buffer's file rather than raising E32.
    assert_eq!(oxvim.request("nvim_command", vec![Value::from("enew")]), Value::Nil);
    assert_eq!(oxvim.request("nvim_command", vec![Value::from("bwipeout!")]), Value::Nil);
    let remaining = oxvim.request("nvim_list_bufs", vec![]);
    let Value::Array(remaining) = remaining else { panic!("buffer list is not an array") };
    assert_eq!(remaining.len(), 1, "only the file buffer survives: {remaining:?}");
    assert_eq!(
        oxvim.request("nvim_get_current_buf", vec![]),
        remaining[0].clone(),
        "wiping the scratch buffer falls back onto the file buffer",
    );
    assert_eq!(
        oxvim.request(
            "nvim_buf_set_lines",
            vec![
                Value::from(0),
                Value::from(0),
                Value::from(-1),
                Value::Boolean(false),
                Value::Array(vec![Value::from("rewritten by write")]),
            ],
        ),
        Value::Nil,
    );
    assert_eq!(oxvim.request("nvim_command", vec![Value::from("write")]), Value::Nil);
    assert_eq!(fs::read_to_string(&target).unwrap(), "rewritten by write\n");

    let _ = fs::remove_file(&target);
}

#[test]
fn write_without_name_on_nameless_buffer_raises_e32() {
    let mut oxvim = Embedded::spawn();
    let error = oxvim.request_error("nvim_command", vec![Value::from("write")]);
    assert!(format!("{error:?}").contains("E32"), "{error:?}");
}

#[test]
fn exec_lua_reply_refs_are_freed_after_encoding() {
    let mut oxvim = Embedded::spawn();
    // The converter's `ox-lua.refs` registry table counts ever-issued slots
    // (`__next`); freed slots are recycled, so the counter only grows while
    // live references accumulate.
    let probe = "local registry = debug.getregistry() \
                 local refs = registry['ox-lua.refs'] \
                 if refs == nil then \
                   for _, value in pairs(registry) do \
                     if type(value) == 'table' and rawget(value, '__next') ~= nil then refs = value end \
                   end \
                 end \
                 return refs and refs.__next or -1";
    let registry_next = |oxvim: &mut Embedded| match oxvim.request(
        "nvim_exec_lua",
        vec![Value::from(probe), Value::Array(vec![])],
    ) {
        Value::Integer(next) => next.as_i64().expect("registry slot count exceeds i64"),
        other => panic!("registry probe returned {other:?}"),
    };
    // The refs table is created lazily by the first conversion that stores a
    // reference, so warm it up before probing.
    oxvim.request("nvim_exec_lua", vec![Value::from("return function() end"), Value::Array(vec![])]);
    let base = registry_next(&mut oxvim);
    assert!(base >= 1, "ox-lua.refs table was not found ({base})");

    // Request replies: each converted function result is packed as a
    // "<Lua n>" hint string; its registry slot must be released right after.
    for index in 0..12_000 {
        let reply = oxvim.request(
            "nvim_exec_lua",
            vec![Value::from("return function() end"), Value::Array(vec![])],
        );
        if index == 0 {
            let Value::String(text) = &reply else {
                panic!("first reply is not a Lua hint: {reply:?}")
            };
            assert!(text.as_bytes().starts_with(b"<Lua "), "unexpected hint {text:?}");
        }
    }

    // Fire-and-forget exec still converts its result; with no reply to
    // encode, the notification path owns the release.
    for _ in 0..2_000 {
        oxvim.notify(
            "nvim_exec_lua",
            vec![Value::from("return { hook = function() end }"), Value::Array(vec![])],
        );
    }

    // Ex `:lua` chunks discard their results; those references must not
    // accumulate either.
    for _ in 0..1_000 {
        oxvim.request("nvim_command", vec![Value::from("lua return function() end")]);
    }

    let settled = registry_next(&mut oxvim);
    assert!(
        settled - base <= 64,
        "Lua registry slots grew by {} across the stress (base {base}, settled {settled})",
        settled - base,
    );
}
