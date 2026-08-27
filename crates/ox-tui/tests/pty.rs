//! Client behavior asserted on visible cells, under a real PTY.
//!
//! Every test here spawns the harness binary on a PTY slave, so the client runs
//! with a controlling terminal, negotiates capabilities, programs the palette
//! and writes real escape sequences. Those bytes are fed through a small
//! terminal emulator, and the assertions read the resulting cells: internal
//! state can agree with the design while the frame does not.
#![cfg(unix)]
#![allow(missing_docs, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const HARNESS: &str = env!("CARGO_BIN_EXE_ox-tui-pty-harness");
const TIMEOUT: Duration = Duration::from_secs(20);

// The dark palette this client programs, as the design system fixes it.
const BG: Color = Color::Rgb(0x16, 0x18, 0x1d);
const FLOAT_BG: Color = Color::Rgb(0x1d, 0x20, 0x26);
const ACCENT: Color = Color::Rgb(0xda, 0x83, 0x4f);
const ERROR: Color = Color::Rgb(0xf2, 0x6f, 0x74);
const FG: Color = Color::Rgb(0xc9, 0xcc, 0xd4);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Color {
    #[default]
    Default,
    Rgb(u8, u8, u8),
    Indexed(u8),
}

#[derive(Clone, Debug)]
struct Cell {
    glyph: String,
    foreground: Color,
    background: Color,
}

impl Default for Cell {
    fn default() -> Self {
        Self { glyph: " ".into(), foreground: Color::Default, background: Color::Default }
    }
}

/// Just enough terminal to read back what the damage writer painted: cursor
/// addressing, SGR color selection, and printable text. Everything else the
/// client emits (mode sets, kitty keyboard, synchronized output, OSC strings)
/// is consumed and, for OSC, retained for the palette assertions.
struct Terminal {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    row: usize,
    column: usize,
    foreground: Color,
    background: Color,
}

impl Terminal {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::default(); width * height],
            row: 0,
            column: 0,
            foreground: Color::Default,
            background: Color::Default,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                0x1b => index += self.escape(&bytes[index..]),
                b'\r' => {
                    self.column = 0;
                    index += 1;
                }
                b'\n' => {
                    self.row = self.row.saturating_add(1);
                    index += 1;
                }
                byte if byte < 0x20 || byte == 0x7f => index += 1,
                byte => {
                    let length = utf8_length(byte);
                    let end = (index + length).min(bytes.len());
                    self.put(String::from_utf8_lossy(&bytes[index..end]).into_owned());
                    index = end;
                }
            }
        }
    }

    /// Consume one escape sequence and return the bytes used.
    fn escape(&mut self, bytes: &[u8]) -> usize {
        match bytes.get(1) {
            Some(b'[') => {
                let mut end = 2;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                if end >= bytes.len() {
                    return bytes.len();
                }
                let parameters = &bytes[2..end];
                match bytes[end] {
                    b'H' if !parameters.contains(&b'?') => self.move_to(parameters),
                    b'm' => self.select_graphic(parameters),
                    _ => {}
                }
                end + 1
            }
            // OSC and DCS strings run to ST or BEL and paint nothing.
            Some(b']') | Some(b'P') => {
                let mut end = 2;
                while end < bytes.len() {
                    if bytes[end] == 0x07 {
                        return end + 1;
                    }
                    if bytes[end] == 0x1b && bytes.get(end + 1) == Some(&b'\\') {
                        return end + 2;
                    }
                    end += 1;
                }
                bytes.len()
            }
            Some(_) => 2,
            None => 1,
        }
    }

    fn move_to(&mut self, parameters: &[u8]) {
        let text = String::from_utf8_lossy(parameters);
        let mut parts = text.split(';');
        let row = parts.next().and_then(|value| value.parse::<usize>().ok()).unwrap_or(1);
        let column = parts.next().and_then(|value| value.parse::<usize>().ok()).unwrap_or(1);
        self.row = row.saturating_sub(1);
        self.column = column.saturating_sub(1);
    }

    fn select_graphic(&mut self, parameters: &[u8]) {
        let text = String::from_utf8_lossy(parameters);
        let values: Vec<u32> =
            text.split(';').map(|value| value.parse::<u32>().unwrap_or(0)).collect();
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => {
                    self.foreground = Color::Default;
                    self.background = Color::Default;
                    index += 1;
                }
                39 => {
                    self.foreground = Color::Default;
                    index += 1;
                }
                49 => {
                    self.background = Color::Default;
                    index += 1;
                }
                selector @ (38 | 48) => {
                    let (color, used) = parse_extended(&values[index..]);
                    if selector == 38 {
                        self.foreground = color;
                    } else {
                        self.background = color;
                    }
                    index += used;
                }
                _ => index += 1,
            }
        }
    }

    fn put(&mut self, glyph: String) {
        if self.row < self.height && self.column < self.width {
            let index = self.row * self.width + self.column;
            self.cells[index] =
                Cell { glyph, foreground: self.foreground, background: self.background };
        }
        self.column = self.column.saturating_add(1);
    }

    fn row_text(&self, row: usize) -> String {
        let start = row * self.width;
        self.cells[start..start + self.width].iter().map(|cell| cell.glyph.as_str()).collect()
    }

    fn screen_text(&self) -> String {
        (0..self.height).map(|row| self.row_text(row)).collect::<Vec<_>>().join("\n")
    }

    fn cell(&self, row: usize, column: usize) -> &Cell {
        &self.cells[row * self.width + column]
    }
}

fn parse_extended(values: &[u32]) -> (Color, usize) {
    match values.get(1) {
        Some(2) => {
            let component = |offset: usize| {
                u8::try_from(values.get(offset).copied().unwrap_or(0)).unwrap_or(0)
            };
            (Color::Rgb(component(2), component(3), component(4)), 5)
        }
        Some(5) => (
            Color::Indexed(u8::try_from(values.get(2).copied().unwrap_or(0)).unwrap_or(0)),
            3,
        ),
        _ => (Color::Default, 1),
    }
}

fn utf8_length(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// A running harness client and everything its terminal has received.
struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    receiver: mpsc::Receiver<Vec<u8>>,
    output: Vec<u8>,
    pid: Option<u32>,
    columns: usize,
    rows: usize,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Session {
    fn start(script: &str, columns: u16, rows: u16) -> Self {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols: columns, pixel_width: 0, pixel_height: 0 })
            .expect("open PTY");
        let mut command = CommandBuilder::new(HARNESS);
        command.arg("client");
        command.arg(script);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("OXVIM_TUI_MOTION", "reduced");
        // The variant is chosen from $COLORFGBG before any highlight arrives,
        // so the dark palette under test is selected explicitly.
        command.env("COLORFGBG", "15;0");
        let child = pair.slave.spawn_command(command).expect("spawn harness client");
        drop(pair.slave);
        let pid = child.process_id();

        let (sender, receiver) = mpsc::channel();
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                    Ok(_) => {}
                }
            }
        });
        Self {
            child,
            receiver,
            output: Vec::new(),
            pid,
            columns: usize::from(columns),
            rows: usize::from(rows),
            _master: pair.master,
        }
    }

    fn drain(&mut self) {
        while let Ok(chunk) = self.receiver.try_recv() {
            self.output.extend(chunk);
        }
    }

    fn wait_for(&mut self, description: &str, ready: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.drain();
            if ready(&self.output) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {description}; output so far:\n{}",
                    String::from_utf8_lossy(&self.output)
                );
            }
            match self.receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => self.output.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.drain();
                    if ready(&self.output) {
                        return;
                    }
                    panic!(
                        "PTY closed before {description}; output:\n{}",
                        String::from_utf8_lossy(&self.output)
                    );
                }
            }
        }
    }

    /// Wait until `needle` is visible on the emulated screen.
    ///
    /// The raw stream cannot be searched for text: cells are addressed one
    /// glyph at a time, and the damage writer skips a cell whose value already
    /// matches, so a word arrives with its unchanged letters missing. Only the
    /// composed screen holds the word.
    fn wait_for_screen(&mut self, needle: &str) {
        let (columns, rows) = (self.columns, self.rows);
        self.wait_for(needle, |bytes| {
            render(bytes, columns, rows).screen_text().contains(needle)
        });
    }

    /// Wait for the client to exit and collect every remaining byte.
    fn finish(mut self) -> (Vec<u8>, bool) {
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            self.drain();
            match self.child.try_wait().expect("poll harness client") {
                Some(status) => break status,
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                None => panic!(
                    "harness client did not exit; output:\n{}",
                    String::from_utf8_lossy(&self.output)
                ),
            }
        };
        thread::sleep(Duration::from_millis(50));
        self.drain();
        (self.output, status.success())
    }

    fn signal(&self, name: &str) {
        let pid = self.pid.expect("harness client pid");
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(pid.to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{name} {pid} failed");
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn render(bytes: &[u8], columns: usize, rows: usize) -> Terminal {
    let mut terminal = Terminal::new(columns, rows);
    terminal.feed(bytes);
    terminal
}

#[test]
fn the_command_line_renders_as_a_top_third_overlay() {
    let mut session = Session::start("cmdline", 80, 24);
    session.wait_for_screen("=1+1");
    let (output, success) = session.finish();
    assert!(success, "client exited badly:\n{}", String::from_utf8_lossy(&output));

    let terminal = render(&output, 80, 24);
    let screen = terminal.screen_text();
    // Level two is active, so it owns the overlay and level one is not painted
    // beside it.
    let row = (0..24)
        .find(|row| terminal.row_text(*row).contains("=1+1"))
        .unwrap_or_else(|| panic!("no rendered command line in:\n{screen}"));
    assert!(row < 24 / 2, "the overlay sits in the top third, not row {row}");
    assert!(!screen.contains("edit alpha"), "two levels painted at once:\n{screen}");

    // The overlay is a float: its text sits on the float background, while the
    // grid around it keeps the terminal's own colors.
    let column = terminal.row_text(row).find("=1+1").expect("overlay column");
    assert_eq!(terminal.cell(row, column).background, FLOAT_BG);
    assert_eq!(terminal.cell(row, 0).background, Color::Default);
}

#[test]
fn cancelling_a_nested_command_line_restores_the_level_below_it() {
    let mut session = Session::start("cmdline-restore", 80, 24);
    session.wait_for_screen(":edit alpha");
    let (output, success) = session.finish();
    assert!(success, "client exited badly:\n{}", String::from_utf8_lossy(&output));

    let terminal = render(&output, 80, 24);
    let screen = terminal.screen_text();
    assert!(screen.contains(":edit alpha"), "level one was not restored:\n{screen}");
    assert!(!screen.contains("1+1"), "the cancelled level is still painted:\n{screen}");
}

#[test]
fn the_message_stack_replaces_by_id_and_appends_to_a_stream() {
    let mut session = Session::start("messages", 80, 24);
    session.wait_for_screen("stream head and tail");
    let (output, success) = session.finish();
    assert!(success, "client exited badly:\n{}", String::from_utf8_lossy(&output));

    let terminal = render(&output, 80, 24);
    let screen = terminal.screen_text();

    // Same id, so the second message replaces the first rather than stacking.
    assert!(screen.contains("second failure"), "replacement missing:\n{screen}");
    assert!(
        !screen.contains("first failure"),
        "the replaced message is still on screen:\n{screen}"
    );

    // append: the tail joins the head in one entry, on one row. Two rows would
    // mean the client treated a continuation as a new message.
    let stream = (0..24)
        .find(|row| terminal.row_text(*row).contains("stream head"))
        .unwrap_or_else(|| panic!("no stream row in:\n{screen}"));
    assert!(
        terminal.row_text(stream).contains("stream head and tail"),
        "the stream was split across rows:\n{screen}"
    );

    // The error carries its letter and its own color; the stream carries
    // neither, so neither channel can be coming from the surface style.
    let error = (0..24)
        .find(|row| terminal.row_text(*row).contains("second failure"))
        .expect("error row");
    let letter = terminal.row_text(error).find('E').expect("severity letter");
    assert_eq!(terminal.cell(error, letter).glyph, "E");
    assert_eq!(terminal.cell(error, letter).foreground, ERROR);
    let body = terminal.row_text(error).find("second").expect("error body");
    assert_eq!(terminal.cell(error, body).foreground, FG);
    assert_eq!(terminal.cell(stream, terminal.row_text(stream).find('s').expect("stream body")).foreground, FG);
}

#[test]
fn the_completion_menu_marks_its_selection_and_shows_the_documentation_preview() {
    let mut session = Session::start("popupmenu", 80, 24);
    session.wait_for_screen("beta documentation");
    let (output, success) = session.finish();
    assert!(success, "client exited badly:\n{}", String::from_utf8_lossy(&output));

    let terminal = render(&output, 80, 24);
    let screen = terminal.screen_text();
    let unselected = (0..24)
        .find(|row| terminal.row_text(*row).contains("alpha"))
        .unwrap_or_else(|| panic!("no menu in:\n{screen}"));
    let selected = unselected + 1;
    assert!(terminal.row_text(selected).contains("beta"), "menu rows out of order:\n{screen}");

    // The selected row is the one sanctioned use of the accent surface:
    // background-colored text on accent, 6.21:1. Ordinary foreground on accent
    // is 1.78:1 and forbidden, so the foreground is asserted too.
    let column = terminal.row_text(selected).find("beta").expect("selected word");
    assert_eq!(terminal.cell(selected, column).background, ACCENT);
    assert_eq!(terminal.cell(selected, column).foreground, BG);

    // The unselected row stays on the float background.
    let other = terminal.row_text(unselected).find("alpha").expect("unselected word");
    assert_eq!(terminal.cell(unselected, other).background, FLOAT_BG);
    assert_ne!(terminal.cell(unselected, other).foreground, BG);

    // The preview is built from the selected item's info field, beside the menu.
    let preview = (0..24)
        .find(|row| terminal.row_text(*row).contains("beta documentation"))
        .expect("preview row");
    let preview_column =
        terminal.row_text(preview).find("beta documentation").expect("preview column");
    assert!(
        preview_column > column,
        "the preview must sit beside the menu, not over it:\n{screen}"
    );
    assert!(
        !screen.contains("alpha documentation"),
        "the preview shows the selected item only:\n{screen}"
    );
}

#[test]
fn twenty_columns_drops_the_preview_and_keeps_the_menu() {
    let mut session = Session::start("narrow", 20, 24);
    session.wait_for_screen("alphabetagamma");
    let (output, success) = session.finish();
    assert!(success, "client exited badly:\n{}", String::from_utf8_lossy(&output));

    let terminal = render(&output, 20, 24);
    let screen = terminal.screen_text();
    let row = (0..24)
        .find(|row| terminal.row_text(*row).contains("alphabetagamma"))
        .unwrap_or_else(|| panic!("no menu in:\n{screen}"));

    // The menu survives and keeps its columns; the preview is dropped rather
    // than clipped to a border strip.
    assert!(
        !screen.contains("documentation"),
        "a 20-column terminal must not paint the preview:\n{screen}"
    );
    let column = terminal.row_text(row).find("alphabetagamma").expect("menu column");
    assert_eq!(terminal.cell(row, column).background, ACCENT, "selection still marked");
    // Nothing was written past the last column.
    for line in screen.lines() {
        assert_eq!(line.chars().count(), 20, "row wider than the terminal:\n{screen}");
    }
}

#[test]
fn a_preview_too_narrow_for_text_paints_nothing_at_all() {
    // Twenty-five columns leaves three beside the menu. Clipping the preview
    // into them would paint a strip of border with no text in it, which the
    // 20-column case cannot detect: there the strip is zero columns wide.
    let mut session = Session::start("strip", 25, 24);
    session.wait_for_screen("alphabetagamma");
    let (output, success) = session.finish();
    assert!(success, "client exited badly:\n{}", String::from_utf8_lossy(&output));

    let terminal = render(&output, 25, 24);
    let screen = terminal.screen_text();
    let row = (0..24)
        .find(|row| terminal.row_text(*row).contains("alphabetagamma"))
        .unwrap_or_else(|| panic!("no menu in:\n{screen}"));
    assert!(!screen.contains("documentation"), "the preview was painted:\n{screen}");

    // The menu ends at column 21; nothing claimed the columns after it.
    for column in 22..25 {
        assert_eq!(
            terminal.cell(row, column).background,
            Color::Default,
            "column {column} was claimed by a preview with no room for text"
        );
    }
}

#[test]
fn the_palette_is_programmed_and_restored_on_a_clean_exit() {
    let mut session = Session::start("palette", 80, 24);
    session.wait_for("the programmed palette", |bytes| {
        contains(bytes, b"\x1b]4;0;rgb:16/18/1d\x1b\\")
    });
    let (output, success) = session.finish();
    assert!(success, "client exited badly:\n{}", String::from_utf8_lossy(&output));

    // Every slot the client claims, with the design system's own values.
    for expected in [
        b"\x1b]4;0;rgb:16/18/1d\x1b\\".as_slice(),
        b"\x1b]4;1;rgb:f2/6f/74\x1b\\".as_slice(),
        b"\x1b]4;3;rgb:d0/a3/5c\x1b\\".as_slice(),
        b"\x1b]4;6;rgb:6f/a8/a3\x1b\\".as_slice(),
        b"\x1b]4;7;rgb:c9/cc/d4\x1b\\".as_slice(),
        b"\x1b]4;8;rgb:87/8c/99\x1b\\".as_slice(),
        b"\x1b]4;9;rgb:da/83/4f\x1b\\".as_slice(),
    ] {
        assert!(
            contains(&output, expected),
            "missing {}",
            String::from_utf8_lossy(expected)
        );
    }
    assert!(contains(&output, b"\x1b]104"), "OSC 104 palette restore missing on a clean exit");
    assert!(contains(&output, b"\x1b[?25h"), "the cursor was left hidden");
}

#[test]
fn the_palette_is_restored_when_the_client_is_signalled() {
    let mut session = Session::start("palette-hold", 80, 24);
    session.wait_for("the programmed palette", |bytes| {
        contains(bytes, b"\x1b]4;0;rgb:16/18/1d\x1b\\")
    });
    // The restore must be caused by the signal: with the client still running
    // and the palette already programmed, nothing has restored it yet.
    session.drain();
    assert!(
        !contains(&session.output, b"\x1b]104"),
        "the palette was restored before the signal, so this proves nothing"
    );

    session.signal("TERM");
    session.wait_for("the palette restore", |bytes| contains(bytes, b"\x1b]104"));
    let (output, success) = session.finish();

    assert!(contains(&output, b"\x1b]104"), "OSC 104 palette restore missing after SIGTERM");
    assert!(contains(&output, b"\x1b[?25h"), "the cursor was left hidden after SIGTERM");
    // SIGTERM's default action still ends the process, so it does not report a
    // successful exit: the client restores and then dies from the signal.
    assert!(!success, "the client must terminate from the signal, not exit cleanly");
}
