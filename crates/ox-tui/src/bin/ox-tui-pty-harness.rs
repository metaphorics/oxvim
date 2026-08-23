//! PTY harness for the bundled TUI client's integration tests.
//!
//! A TUI claim is only worth as much as a rendered frame, and rendering needs a
//! controlling terminal, so the client has to run as its own process on a PTY
//! slave. libtest writes its own progress chatter to stdout, which on a PTY
//! would land in the cells under test; this binary exists so the process on the
//! PTY writes nothing but client output.
//!
//! Two roles, chosen by the first argument:
//!
//! - `client <script>` runs the real client against this same binary as its
//!   embedded server, so every layer under test is the shipping one.
//! - `server <script>` speaks msgpack-RPC on stdio and emits one scripted
//!   sequence of redraw batches.

use std::env;
use std::io::{self, Read, Write};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

use ox_rpc::{IncrementalDecoder, Message};
use ox_types::{Dict, Object, OxStr};

/// Long enough for the client to receive a batch and paint a frame, short
/// enough to keep the suite quick.
const RENDER_PAUSE: Duration = Duration::from_millis(120);

fn main() -> ExitCode {
    let mut arguments = env::args().skip(1);
    let role = arguments.next().unwrap_or_default();
    let script = arguments.next().unwrap_or_default();
    match role.as_str() {
        "client" => client(&script),
        "server" => server(&script),
        other => {
            let _ = writeln!(io::stderr(), "unknown harness role {other:?}");
            ExitCode::FAILURE
        }
    }
}

fn client(script: &str) -> ExitCode {
    let Ok(executable) = env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let mut command = Command::new(executable);
    command.arg("server").arg(script);
    match ox_tui::run_command(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "client failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Answer every request with nil, and on the first `nvim_ui_attach` play the
/// script. A script whose name ends in `-hold` keeps the process alive so the
/// test can signal a running client; every other script closes stdout, which
/// the client sees as a clean end of stream.
fn server(script: &str) -> ExitCode {
    let mut decoder = IncrementalDecoder::new();
    let mut input = io::stdin();
    let mut buffer = [0_u8; 8192];
    let mut played = false;
    loop {
        let read = match input.read(&mut buffer) {
            Ok(0) | Err(_) => return ExitCode::SUCCESS,
            Ok(count) => count,
        };
        let Ok(messages) = decoder.feed(&buffer[..read]) else {
            return ExitCode::FAILURE;
        };
        for message in messages {
            let Message::Request { msgid, method, .. } = message else {
                continue;
            };
            if !respond(msgid) {
                return ExitCode::FAILURE;
            }
            if played || method.as_bytes() != b"nvim_ui_attach" {
                continue;
            }
            played = true;
            if !play(script) {
                return ExitCode::FAILURE;
            }
            if !script.ends_with("-hold") {
                thread::sleep(RENDER_PAUSE);
                return ExitCode::SUCCESS;
            }
        }
    }
}

fn respond(msgid: u32) -> bool {
    let message = Message::Response { msgid, result: Ok(Object::Nil) };
    let mut output = io::stdout();
    output.write_all(&message.encode_bytes()).is_ok() && output.flush().is_ok()
}

fn play(script: &str) -> bool {
    for batch in batches(script) {
        let message =
            Message::Notification { method: OxStr::from("redraw"), params: batch };
        let mut output = io::stdout();
        if output.write_all(&message.encode_bytes()).is_err() || output.flush().is_err() {
            return false;
        }
        thread::sleep(RENDER_PAUSE);
    }
    true
}

fn event(name: &str, argsets: Vec<Vec<Object>>) -> Object {
    let mut fields = Vec::with_capacity(argsets.len() + 1);
    fields.push(Object::String(OxStr::from(name)));
    fields.extend(argsets.into_iter().map(Object::Array));
    Object::Array(fields)
}

fn text(value: &str) -> Object {
    Object::String(OxStr::from(value))
}

fn number(value: i64) -> Object {
    Object::Integer(value)
}

/// A `[attr_id, text]` chunk line, the shape every externalized event uses for
/// highlighted text.
fn chunks(parts: &[&str]) -> Object {
    Object::Array(
        parts
            .iter()
            .map(|part| Object::Array(vec![number(0), text(part)]))
            .collect(),
    )
}

fn resize(columns: i64, rows: i64) -> Object {
    event("grid_resize", vec![vec![number(1), number(columns), number(rows)]])
}

fn flush() -> Object {
    event("flush", vec![vec![]])
}

/// `msg_show(kind, content, replace_last, history, append, id, trigger)`.
fn msg_show(kind: &str, body: &str, replace_last: bool, append: bool, id: Object) -> Object {
    event(
        "msg_show",
        vec![vec![
            text(kind),
            chunks(&[body]),
            Object::Boolean(replace_last),
            Object::Boolean(true),
            Object::Boolean(append),
            id,
            text(""),
        ]],
    )
}

/// `popupmenu_show(items, selected, row, col, grid)` with
/// `items = [[word, kind, menu, info], ...]`.
fn popupmenu_show(items: &[[&str; 4]], selected: i64, row: i64, column: i64) -> Object {
    let items = items
        .iter()
        .map(|item| Object::Array(item.iter().map(|field| text(field)).collect()))
        .collect();
    event(
        "popupmenu_show",
        vec![vec![
            Object::Array(items),
            number(selected),
            number(row),
            number(column),
            number(1),
        ]],
    )
}

/// `cmdline_show(content, pos, firstc, prompt, indent, level, hl_id)`.
fn cmdline_show(body: &str, first: &str, level: i64) -> Object {
    event(
        "cmdline_show",
        vec![vec![
            chunks(&[body]),
            number(i64::try_from(body.len()).unwrap_or(0)),
            text(first),
            text(""),
            number(0),
            number(level),
            number(-1),
        ]],
    )
}

/// One `hl_attr_define` naming a client group, so the theme swap path is
/// exercised rather than only the built-in fallbacks.
fn highlight_group(id: i64, name: &str, foreground: i64, background: i64) -> Object {
    let rgb = Dict(vec![
        (OxStr::from("foreground"), number(foreground)),
        (OxStr::from("background"), number(background)),
    ]);
    let info = Object::Array(vec![Object::Dict(Dict(vec![(
        OxStr::from("ui_name"),
        text(name),
    )]))]);
    event(
        "hl_attr_define",
        vec![vec![
            number(id),
            Object::Dict(rgb.clone()),
            Object::Dict(rgb),
            info,
        ]],
    )
}

/// Every batch a script sends, in order. Each batch is one `redraw`
/// notification, so each is one rendered frame.
fn batches(script: &str) -> Vec<Vec<Object>> {
    let wide = || vec![resize(80, 24), flush()];
    let narrow = || vec![resize(20, 24), flush()];
    match script.trim_end_matches("-hold") {
        // The command line as a top-third overlay, then a second recursion
        // level. The last batch decides the final frame, so nesting and its
        // restore are two scripts rather than one racy one.
        "cmdline" => vec![
            wide(),
            vec![cmdline_show("edit alpha", ":", 1), flush()],
            vec![cmdline_show("1+1", "=", 2), flush()],
        ],
        "cmdline-restore" => vec![
            wide(),
            vec![cmdline_show("edit alpha", ":", 1), flush()],
            vec![cmdline_show("1+1", "=", 2), flush()],
            vec![
                event("cmdline_hide", vec![vec![number(2), Object::Boolean(true)]]),
                flush(),
            ],
        ],
        // A message replaced by id, and a shell stream appended to in a later
        // batch. Both are `msg_show` contracts that a simplification would
        // render as two separate lines.
        "messages" => vec![
            wide(),
            vec![
                msg_show("emsg", "first failure", false, false, number(7)),
                flush(),
            ],
            vec![
                msg_show("emsg", "second failure", false, false, number(7)),
                flush(),
            ],
            vec![msg_show("shell_out", "stream head", false, false, Object::Nil), flush()],
            vec![msg_show("shell_out", " and tail", false, true, Object::Nil), flush()],
        ],
        // The completion menu with a selected row and a documentation preview
        // built from the selected item's info field.
        "popupmenu" => vec![
            wide(),
            vec![
                highlight_group(1, "Pmenu", 0x00c9_ccd4, 0x001d_2026),
                popupmenu_show(
                    &[
                        ["alpha", "f", "", "alpha documentation"],
                        ["beta", "v", "", "beta documentation"],
                    ],
                    1,
                    3,
                    4,
                ),
                flush(),
            ],
        ],
        // The same popup at 25 columns, where three columns are left beside the
        // menu: enough for a border strip, not enough for text. A clipping
        // client paints that strip; this one paints nothing there.
        "strip" => vec![
            vec![resize(25, 24), flush()],
            vec![
                popupmenu_show(&[["alphabetagamma", "", "", "documentation body"]], 0, 3, 0),
                flush(),
            ],
        ],
        // The same popup at 20 columns, where no side can hold a preview.
        "narrow" => vec![
            narrow(),
            vec![
                popupmenu_show(
                    &[["alphabetagamma", "", "", "documentation body"]],
                    0,
                    3,
                    0,
                ),
                flush(),
            ],
        ],
        // Nothing but the grid: the palette and its restore are the subject.
        _ => vec![wide()],
    }
}
