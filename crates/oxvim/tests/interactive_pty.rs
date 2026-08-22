//! Real-terminal acceptance for the default TUI-to-embedded-server fork.
#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn default_fork_edits_echoes_quits_and_restores_terminal() {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("open PTY");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_oxvim"));
    command.env("TERM", "xterm-256color");
    command.env("OXVIM_TUI_MOTION", "reduced");
    let mut child = pair.slave.spawn_command(command).expect("spawn default oxvim path");
    drop(pair.slave);

    let (output_sender, output_receiver) = mpsc::channel();
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let reader_thread = thread::spawn(move || {
        let mut bytes = [0_u8; 4096];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if output_sender.send(bytes[..count].to_vec()).is_err() { break; }
                }
            }
        }
    });
    let mut output = Vec::new();
    let mut writer = pair.master.take_writer().expect("take PTY writer");
    wait_for_raw_bytes(&output_receiver, &mut output, b"\x1b[?25l", "TUI raw-session setup");

    let before_insert = printable_text(&output).len();
    writer.write_all(b"iHello\x1b").expect("send insert input");
    writer.flush().expect("flush insert input");
    wait_for_rendered_text(&output_receiver, &mut output, before_insert, b"Hello", "rendered inserted text");

    let before_echo = printable_text(&output).len();
    writer.write_all(b":echo 1+1\r").expect("send echo command");
    writer.flush().expect("flush echo command");
    wait_for_rendered_text(&output_receiver, &mut output, before_echo, b"2", "rendered echo result");

    writer.write_all(b":q!\r").expect("send quit command");
    writer.flush().expect("flush quit command");
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child status") { break status; }
        assert!(Instant::now() < deadline, "oxvim did not exit after :q!");
        thread::sleep(Duration::from_millis(10));
    };

    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    output.extend(output_receiver.try_iter().flatten());
    assert_eq!(
        status.exit_code(),
        0,
        "default oxvim process status: {status}; output: {}",
        String::from_utf8_lossy(&output),
    );
    assert!(output.windows(b"\x1b[0 q".len()).any(|window| window == b"\x1b[0 q"), "cursor mode was not restored");
    assert!(output.windows(b"\x1b]104".len()).any(|window| window == b"\x1b]104"), "terminal palette was not restored");
}

fn wait_for_raw_bytes(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, needle: &[u8], description: &str) {
    wait_for_output(receiver, output, description, |bytes| bytes.windows(needle.len()).any(|window| window == needle));
}

fn wait_for_rendered_text(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, offset: usize, needle: &[u8], description: &str) {
    wait_for_output(receiver, output, description, |bytes| {
        let rendered = printable_text(bytes);
        rendered.get(offset..).is_some_and(|text| text.windows(needle.len()).any(|window| window == needle))
    });
}

fn wait_for_output(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, description: &str, ready: impl Fn(&[u8]) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if ready(output) { return; }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {description}; output: {}",
            String::from_utf8_lossy(output),
        );
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
