#![cfg(unix)]
#![allow(missing_docs)]

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ox_rpc::RedrawEvent;
use ox_tui::chrome::{Chrome, MessageLifetime, MessageUpdate, PopupItem, TextChunk, TimeMs};
use ox_tui::{MotionPolicy, TuiState};
use ox_types::{Dict, Object, OxStr};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use differential::{OXVIM, binary};

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn edits_quits_and_restores_terminal_palette() {
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).expect("open PTY");
    let mut command = CommandBuilder::new(binary(OXVIM));
    command.env("TERM", "xterm-256color");
    command.env("OXVIM_TUI_MOTION", "reduced");
    let mut child = pair.slave.spawn_command(command).expect("spawn release oxvim in PTY");
    drop(pair.slave);

    let (sender, receiver) = mpsc::channel();
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    let mut output = Vec::new();
    let mut writer = pair.master.take_writer().expect("take PTY writer");
    wait_for(&receiver, &mut output, "terminal setup", |bytes| contains(bytes, b"\x1b[?25l"));

    let text_offset = printable_text(&output).len();
    writer.write_all(b"iHello\x1b").expect("send insert sequence");
    writer.flush().expect("flush insert sequence");
    wait_for(&receiver, &mut output, "inserted grid content", |bytes| {
        printable_text(bytes).get(text_offset..).is_some_and(|text| contains(text, b"Hello"))
    });

    let cmdline_offset = printable_text(&output).len();
    writer.write_all(b":edit x").expect("type command-line overlay");
    writer.flush().expect("flush command-line overlay");
    wait_for(&receiver, &mut output, "rendered :edit x overlay", |bytes| {
        printable_text(bytes).get(cmdline_offset..).is_some_and(|text| contains(text, b"editx"))
    });

    let nested_offset = printable_text(&output).len();
    writer.write_all(b"\x12=1+1").expect("type nested expression command line");
    writer.flush().expect("flush nested expression command line");
    wait_for(&receiver, &mut output, "rendered Ctrl-R = level", |bytes| {
        printable_text(bytes).get(nested_offset..).is_some_and(|text| contains(text, b"1+1"))
    });

    writer.write_all(b"\x1b").expect("cancel nested command line");
    writer.flush().expect("flush nested cancel");
    writer.write_all(b"\x1b").expect("cancel outer command line");
    writer.flush().expect("flush outer cancel");

    writer.write_all(b":q!\r").expect("send forced quit");
    writer.flush().expect("flush forced quit");

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll oxvim") { break status; }
        assert!(Instant::now() < deadline, "oxvim did not exit after :q!");
        thread::sleep(Duration::from_millis(10));
    };
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    output.extend(receiver.try_iter().flatten());

    assert_eq!(status.exit_code(), 0, "PTY status {status}; output {}", String::from_utf8_lossy(&output));
    assert!(contains(&output, b"\x1b[0 q"), "cursor mode restore missing");
    assert!(contains(&output, b"\x1b]104"), "OSC 104 palette restore missing");
}

#[test]
fn cmdline_overlay_nesting_and_wildmenu_strip_follow_protocol_levels() {
    let mut chrome = Chrome::default();
    chrome.cmdline_show(1, vec![chunk("edit x")], 6, ":", None, 0, -1);
    let first = chrome.cmdline.active().expect("command-line overlay").clone();
    assert_eq!(first.level, 1);
    assert_eq!(first.content, vec![chunk("edit x")]);
    assert!(chrome.layout(80, 24, Some(0)).cmdline.is_some());

    chrome.cmdline_show(2, vec![chunk("1+1")], 3, "=", None, 0, -1);
    assert_eq!(chrome.cmdline.active().map(|level| level.level), Some(2), "Ctrl-R = opens level two");
    assert!(chrome.cmdline_hide(2, true), "Esc hides nested level");
    assert_eq!(chrome.cmdline.active(), Some(&first), "nested Esc restores level one");

    chrome.popupmenu_show(vec![PopupItem::new("one", "", "", "")], Some(0), 0, 0, -1);
    assert!(chrome.message_show(update("wildlist", "one  two", Object::Nil)).is_ok());
    assert!(chrome.messages.is_empty(), "wildmenu text must be stripped from normal messages");
    let layout = chrome.layout(80, 24, Some(0));
    assert!(layout.cmdline.is_some() && layout.wildmenu.is_some() && layout.wildlist.is_some());
}

#[test]
fn sticky_expiry_and_same_id_replacement_are_distinct() {
    let mut chrome = Chrome::default();
    assert!(chrome.message_show(update("emsg", "sticky", Object::Integer(1))).is_ok());
    assert!(chrome.message_show(update("echo", "old", Object::Integer(2))).is_ok());
    chrome.finish_batch(TimeMs(10));
    assert!(matches!(chrome.messages[0].lifetime, MessageLifetime::StickyUntilKeypress));
    assert!(matches!(chrome.messages[1].lifetime, MessageLifetime::Expiring { .. }));

    assert!(chrome.message_show(update("echo", "new", Object::Integer(2))).is_ok());
    chrome.finish_batch(TimeMs(20));
    assert_eq!(chrome.messages.len(), 2, "same kind and id replaces in place");
    assert_eq!(chrome.messages[1].content, vec![chunk("new")]);
    chrome.advance_time(TimeMs(4_020));
    assert_eq!(chrome.messages.len(), 1, "echo expires after its stable batch");
    chrome.keypress();
    assert!(chrome.messages.is_empty(), "error-kind message persists only to keypress");
}

#[test]
fn colorscheme_highlights_retheme_at_one_batch_boundary() {
    let mut state = TuiState::new(Some("15;0"), MotionPolicy::Reduced);
    let generation = state.theme.generation();
    let event = RedrawEvent {
        name: OxStr::from("hl_attr_define"),
        argsets: vec![vec![
            Object::Integer(1),
            Object::Dict(Dict(vec![
                (OxStr::from("foreground"), Object::Integer(0x11_22_33)),
                (OxStr::from("background"), Object::Integer(0xee_dd_cc)),
            ])),
            Object::Dict(Dict(Vec::new())),
            Object::Array(vec![Object::Dict(Dict(vec![(
                OxStr::from("hi_name"),
                Object::String(OxStr::from("NormalFloat")),
            )]))]),
        ]],
    };
    state.apply_redraw(&[event], TimeMs(1)).expect("apply complete colorscheme batch");
    assert_eq!(state.theme.generation(), generation + 1);
    assert!(state.theme.received_highlights());
}

fn chunk(text: &str) -> TextChunk {
    TextChunk::new(0, text, -1)
}

fn update(kind: &str, text: &str, id: Object) -> MessageUpdate {
    MessageUpdate {
        kind: OxStr::from(kind),
        content: vec![chunk(text)],
        replace_last: false,
        history: true,
        append: false,
        id,
        prompt: false,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn wait_for(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, description: &str, ready: impl Fn(&[u8]) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if ready(output) { return; }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {description}; output: {}", String::from_utf8_lossy(output));
        if let Ok(bytes) = receiver.recv_timeout(remaining.min(Duration::from_millis(50))) { output.extend(bytes); }
    }
}

fn printable_text(bytes: &[u8]) -> Vec<u8> {
    #[derive(Clone, Copy)]
    enum State { Ground, Escape, Csi, Osc, OscEscape, Dcs, DcsEscape }
    let mut state = State::Ground;
    let mut text = Vec::new();
    for &byte in bytes {
        state = match state {
            State::Ground if byte == 0x1b => State::Escape,
            State::Ground => { if byte >= b' ' || matches!(byte, b'\n' | b'\r' | b'\t') { text.push(byte); } State::Ground }
            State::Escape => match byte { b'[' => State::Csi, b']' => State::Osc, b'P' => State::Dcs, _ => State::Ground },
            State::Csi => if (0x40..=0x7e).contains(&byte) { State::Ground } else { State::Csi },
            State::Osc => match byte { 0x07 => State::Ground, 0x1b => State::OscEscape, _ => State::Osc },
            State::OscEscape => if byte == b'\\' { State::Ground } else { State::Osc },
            State::Dcs => if byte == 0x1b { State::DcsEscape } else { State::Dcs },
            State::DcsEscape => if byte == b'\\' { State::Ground } else { State::Dcs },
        };
    }
    text
}
