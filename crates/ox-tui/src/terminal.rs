//! Terminal capability negotiation, lifecycle restoration, and retained output.
#![allow(missing_docs)]

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::QueueableCommand;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use thiserror::Error;

use crate::theme::{Ansi16Color, QuantizedColor, Rgb};

pub const PROBE_TIMEOUT: Duration = Duration::from_millis(100);
pub const KITTY_KEYBOARD_QUERY: &[u8] = b"\x1b[?u\x1b[c";
pub const SYNC_OUTPUT_QUERY: &[u8] = b"\x1b[?2026$p";
pub const UNDERCURL_QUERY: &[u8] = b"\x1b[0m\x1b[4:3m\x1bP$qm\x1b\\";
pub const OSC52_QUERY: &[u8] = b"\x1b]52;c;?\x1b\\";

const KITTY_PUSH: &[u8] = b"\x1b[>3u";
const KITTY_POP: &[u8] = b"\x1b[<u";
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const CURSOR_SETUP: &[u8] = b"\x1b[?25l";
const CURSOR_RESTORE: &[u8] = b"\x1b[0 q\x1b[?25h\x1b[0m";
const PALETTE_RESTORE: &[u8] = b"\x1b]104\x1b\\";
const TMUX_PALETTE_RESTORE: &[u8] = b"\x1bPtmux;\x1b\x1b]104\x1b\x1b\\\x1b\\";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEnvironment {
    pub colorterm: Option<String>,
    pub term: Option<String>,
    pub terminfo: Option<String>,
    pub terminfo_colors: Option<u16>,
    pub inside_tmux: bool,
    pub tmux_passthrough: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSupport {
    TrueColor,
    Xterm256,
    Ansi16,
    Mono,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeAnswer {
    Supported,
    Unsupported,
    Incomplete,
}

impl ProbeAnswer {
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteDecision {
    Direct,
    TmuxPassthrough,
    RestoreOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub colors: ColorSupport,
    pub kitty_keyboard: bool,
    pub synchronized_output: bool,
    pub undercurl: bool,
    pub colored_underline: bool,
    pub osc52_clipboard: bool,
    pub palette: PaletteDecision,
}

impl TerminalCapabilities {
    /// Environment-derived baseline: color support and the OSC 4 palette
    /// safety decision. The probe-dependent features (kitty keyboard, sync
    /// output, undercurl, OSC 52) start disabled here and are populated by
    /// [`probe_terminal`] on the live terminal; environment hints only ever
    /// short-circuit color and palette decisions, never the negotiated probes.
    pub fn from_environment(environment: &TerminalEnvironment) -> Self {
        Self {
            colors: detect_color_support(environment),
            kitty_keyboard: false,
            synchronized_output: false,
            undercurl: false,
            colored_underline: false,
            osc52_clipboard: false,
            palette: osc4_decision(environment.inside_tmux, environment.tmux_passthrough),
        }
    }

    pub fn apply_probe_answers(
        &mut self,
        kitty: ProbeAnswer,
        sync: ProbeAnswer,
        undercurl: ProbeAnswer,
        osc52: ProbeAnswer,
    ) {
        self.kitty_keyboard = kitty.is_supported();
        self.synchronized_output = sync.is_supported();
        self.undercurl = undercurl.is_supported();
        self.colored_underline = undercurl.is_supported();
        self.osc52_clipboard = osc52.is_supported();
    }
}

/// Query the live terminal for the negotiated features within `policy.timeout`.
///
/// Raw mode is enabled for the query window so the terminal's escape
/// responses reach the reader, then restored; the caller's
/// [`TerminalSession::start`] re-enables raw mode for the interactive phase.
/// Incomplete or unanswered probes remain disabled.
pub fn probe_terminal<R: Read + AsRawFd, W: Write>(
    reader: &mut R,
    writer: &mut W,
    environment: &TerminalEnvironment,
    policy: ProbePolicy,
) -> Result<TerminalCapabilities, TerminalError> {
    enable_raw_mode().map_err(|error| TerminalError::io("raw-mode enable for probe", error))?;
    let result = probe_io(reader, writer, environment, policy);
    let _ = disable_raw_mode();
    result
}

/// Write every probe query, then read bounded responses and parse them.
///
/// Pure with respect to the reader/writer once raw mode is handled by the
/// caller, so it is driveable with local sockets in tests.
fn probe_io<R: Read + AsRawFd, W: Write>(
    reader: &mut R,
    writer: &mut W,
    environment: &TerminalEnvironment,
    policy: ProbePolicy,
) -> Result<TerminalCapabilities, TerminalError> {
    let mut capabilities = TerminalCapabilities::from_environment(environment);
    for query in [KITTY_KEYBOARD_QUERY, SYNC_OUTPUT_QUERY, UNDERCURL_QUERY, OSC52_QUERY] {
        writer
            .write_all(query)
            .map_err(|error| TerminalError::io("capability probe write", error))?;
    }
    writer
        .flush()
        .map_err(|error| TerminalError::io("capability probe flush", error))?;

    let mut poll = Poll::new().map_err(|error| TerminalError::io("capability probe poll", error))?;
    let mut events = Events::with_capacity(8);
    {
        let fd = reader.as_raw_fd();
        let mut source = SourceFd(&fd);
        poll.registry()
            .register(&mut source, Token(0), Interest::READABLE)
            .map_err(|error| TerminalError::io("capability probe register", error))?;
    }
    let deadline = Instant::now()
        .checked_add(policy.timeout)
        .unwrap_or_else(Instant::now);
    let terminfo_smulx = terminfo_colored_underline(environment.terminfo.as_deref());
    let mut response = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let all_answered = [
            parse_kitty_keyboard_response(&response),
            parse_sync_output_response(&response),
            parse_undercurl_response(&response, terminfo_smulx),
            parse_osc52_response(&response),
        ]
        .iter()
        .all(|answer| *answer != ProbeAnswer::Incomplete);
        if all_answered || Instant::now() >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        poll.poll(&mut events, Some(remaining))
            .map_err(|error| TerminalError::io("capability probe wait", error))?;
        if events.is_empty() {
            continue;
        }
        let read =
            reader.read(&mut chunk).map_err(|error| TerminalError::io("capability probe read", error))?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
    }

    let kitty = parse_kitty_keyboard_response(&response);
    let sync = parse_sync_output_response(&response);
    let undercurl = parse_undercurl_response(&response, terminfo_smulx);
    let osc52 = parse_osc52_response(&response);
    capabilities.apply_probe_answers(kitty, sync, undercurl, osc52);
    Ok(capabilities)
}

/// Whether the terminal's own terminfo advertises the colored-underline
/// (Smulx) capability, allowing the DCS probe to be skipped.
fn terminfo_colored_underline(terminfo: Option<&str>) -> bool {
    terminfo.is_some_and(|capabilities| {
        capabilities
            .split([',', ':', '\n', '|'])
            .any(|capability| matches!(capability.trim().split('=').next(), Some("Smulx" | "smulx")))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbePolicy {
    pub timeout: Duration,
}

impl Default for ProbePolicy {
    fn default() -> Self {
        Self { timeout: PROBE_TIMEOUT }
    }
}

pub fn detect_color_support(environment: &TerminalEnvironment) -> ColorSupport {
    if environment
        .colorterm
        .as_deref()
        .is_some_and(colorterm_is_truecolor)
        || environment
            .terminfo
            .as_deref()
            .is_some_and(terminfo_has_truecolor)
    {
        ColorSupport::TrueColor
    } else {
        match environment.terminfo_colors.unwrap_or(0) {
            256.. => ColorSupport::Xterm256,
            16..=255 => ColorSupport::Ansi16,
            _ => ColorSupport::Mono,
        }
    }
}

pub fn colorterm_is_truecolor(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
}

pub fn terminfo_has_truecolor(capabilities: &str) -> bool {
    let mut tc = false;
    let mut rgb = false;
    let mut foreground = false;
    let mut background = false;
    for capability in capabilities.split([',', ':', '\n', '|']) {
        let name = capability.trim().split('=').next().unwrap_or_default();
        tc |= name == "Tc";
        rgb |= name == "RGB";
        foreground |= matches!(name, "setrgbf" | "setaf24");
        background |= matches!(name, "setrgbb" | "setab24");
    }
    tc || rgb || (foreground && background)
}

pub const fn osc4_decision(inside_tmux: bool, tmux_passthrough: bool) -> PaletteDecision {
    match (inside_tmux, tmux_passthrough) {
        (true, true) => PaletteDecision::TmuxPassthrough,
        (true, false) => PaletteDecision::RestoreOnly,
        (false, _) => PaletteDecision::Direct,
    }
}

pub fn parse_kitty_keyboard_response(bytes: &[u8]) -> ProbeAnswer {
    if find_csi(bytes, b'u', |parameters| {
        parameters.first() == Some(&b'?')
            && parameters[1..].iter().all(|byte| byte.is_ascii_digit() || *byte == b';')
    }) {
        ProbeAnswer::Supported
    } else if find_csi(bytes, b'c', |parameters| parameters.first() == Some(&b'?')) {
        ProbeAnswer::Unsupported
    } else {
        ProbeAnswer::Incomplete
    }
}

pub fn parse_sync_output_response(bytes: &[u8]) -> ProbeAnswer {
    parse_decrpm(bytes, 2026)
}

pub fn parse_undercurl_response(bytes: &[u8], terminfo_smulx: bool) -> ProbeAnswer {
    if terminfo_smulx || contains(bytes, b"\x1bP1$r4:3m\x1b\\") {
        ProbeAnswer::Supported
    } else if contains(bytes, b"\x1bP0$r") {
        ProbeAnswer::Unsupported
    } else {
        ProbeAnswer::Incomplete
    }
}

pub fn parse_osc52_response(bytes: &[u8]) -> ProbeAnswer {
    let Some(start) = find_subslice(bytes, b"\x1b]52;") else {
        return ProbeAnswer::Incomplete;
    };
    let payload = &bytes[start + 5..];
    let terminated = payload.contains(&0x07) || find_subslice(payload, b"\x1b\\").is_some();
    if !terminated {
        return ProbeAnswer::Incomplete;
    }
    if payload.iter().any(|byte| *byte == b';') {
        ProbeAnswer::Supported
    } else {
        ProbeAnswer::Unsupported
    }
}

fn parse_decrpm(bytes: &[u8], mode: u16) -> ProbeAnswer {
    let prefix = format!("\x1b[?{mode};");
    let Some(start) = find_subslice(bytes, prefix.as_bytes()) else {
        return ProbeAnswer::Incomplete;
    };
    let response = &bytes[start + prefix.len()..];
    let Some(end) = find_subslice(response, b"$y") else {
        return ProbeAnswer::Incomplete;
    };
    match std::str::from_utf8(&response[..end]).ok().and_then(|value| value.parse::<u8>().ok()) {
        Some(1..=4) => ProbeAnswer::Supported,
        Some(0) => ProbeAnswer::Unsupported,
        _ => ProbeAnswer::Incomplete,
    }
}

fn find_csi(bytes: &[u8], final_byte: u8, predicate: impl Fn(&[u8]) -> bool) -> bool {
    let mut offset = 0;
    while let Some(relative) = find_subslice(&bytes[offset..], b"\x1b[") {
        let start = offset + relative + 2;
        let mut end = start;
        while end < bytes.len() && (0x20..=0x3f).contains(&bytes[end]) {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == final_byte && predicate(&bytes[start..end]) {
            return true;
        }
        offset = start;
    }
    false
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find_subslice(haystack, needle).is_some()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("terminal {operation} failed: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Frame(#[from] FrameError),
}

impl TerminalError {
    fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame dimensions require {expected} cells, received {actual}")]
    CellCount { expected: usize, actual: usize },
    #[error("frame dimensions overflow the host address space")]
    DimensionsOverflow,
    #[error("cell {index} has empty text but is not a continuation")]
    EmptyCell { index: usize },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnderlineStyle {
    #[default]
    None,
    Straight,
    Curl,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellAttributes {
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub reverse: bool,
    pub underline: UnderlineStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalColor {
    Default,
    Rgb(Rgb),
    Xterm256(QuantizedColor),
    Ansi16(Ansi16Color),
}

impl Default for TerminalColor {
    fn default() -> Self {
        Self::Default
    }
}

impl TerminalColor {
    fn crossterm(self) -> Color {
        match self {
            Self::Default => Color::Reset,
            Self::Rgb(rgb) => Color::Rgb { r: rgb.r, g: rgb.g, b: rgb.b },
            Self::Xterm256(color) => Color::AnsiValue(color.index),
            Self::Ansi16(color) => match color {
                Ansi16Color::Black => Color::Black,
                Ansi16Color::Red => Color::DarkRed,
                Ansi16Color::Green => Color::DarkGreen,
                Ansi16Color::Yellow => Color::DarkYellow,
                Ansi16Color::Blue => Color::DarkBlue,
                Ansi16Color::Magenta => Color::DarkMagenta,
                Ansi16Color::Cyan => Color::DarkCyan,
                Ansi16Color::White => Color::Grey,
                Ansi16Color::BrightBlack => Color::DarkGrey,
                Ansi16Color::BrightRed => Color::Red,
                Ansi16Color::BrightGreen => Color::Green,
                Ansi16Color::BrightYellow => Color::Yellow,
                Ansi16Color::BrightBlue => Color::Blue,
                Ansi16Color::BrightMagenta => Color::Magenta,
                Ansi16Color::BrightCyan => Color::Cyan,
                Ansi16Color::BrightWhite => Color::White,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: Vec<u8>,
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub attributes: CellAttributes,
    pub continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: vec![b' '],
            foreground: TerminalColor::Default,
            background: TerminalColor::Default,
            attributes: CellAttributes::default(),
            continuation: false,
        }
    }
}

impl Cell {
    pub fn continuation() -> Self {
        Self { text: Vec::new(), continuation: true, ..Self::default() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Frame {
    pub fn new(width: u16, height: u16, cells: Vec<Cell>) -> Result<Self, FrameError> {
        let expected = usize::from(width)
            .checked_mul(usize::from(height))
            .ok_or(FrameError::DimensionsOverflow)?;
        if cells.len() != expected {
            return Err(FrameError::CellCount { expected, actual: cells.len() });
        }
        if let Some((index, _)) = cells
            .iter()
            .enumerate()
            .find(|(_, cell)| cell.text.is_empty() && !cell.continuation)
        {
            return Err(FrameError::EmptyCell { index });
        }
        Ok(Self { width, height, cells })
    }

    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    fn cell(&self, index: usize) -> Option<&Cell> {
        self.cells.get(index)
    }
}

pub struct DamageWriter<W> {
    writer: W,
    previous: Option<Frame>,
    undercurl: bool,
}

impl<W: Write> DamageWriter<W> {
    pub fn new(writer: W, undercurl: bool) -> Self {
        Self { writer, previous: None, undercurl }
    }

    pub fn previous(&self) -> Option<&Frame> {
        self.previous.as_ref()
    }

    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Writes only cells whose complete value differs from the retained frame.
    /// The retained snapshot advances only after every queued byte is flushed.
    pub fn render(&mut self, frame: &Frame) -> Result<usize, TerminalError> {
        let same_dimensions = self
            .previous
            .as_ref()
            .is_some_and(|previous| previous.width == frame.width && previous.height == frame.height);
        let mut changed = 0;
        for (index, cell) in frame.cells.iter().enumerate() {
            let unchanged = same_dimensions
                && self.previous.as_ref().and_then(|previous| previous.cell(index)) == Some(cell);
            if unchanged || cell.continuation {
                continue;
            }
            let x = (index % usize::from(frame.width)) as u16;
            let y = (index / usize::from(frame.width)) as u16;
            self.writer.queue(MoveTo(x, y)).map_err(|error| TerminalError::io("cursor move", error))?;
            self.writer
                .queue(SetAttribute(Attribute::Reset))
                .map_err(|error| TerminalError::io("attribute reset", error))?;
            self.writer
                .queue(SetForegroundColor(cell.foreground.crossterm()))
                .map_err(|error| TerminalError::io("foreground color", error))?;
            self.writer
                .queue(SetBackgroundColor(cell.background.crossterm()))
                .map_err(|error| TerminalError::io("background color", error))?;
            if cell.attributes.bold {
                self.writer
                    .queue(SetAttribute(Attribute::Bold))
                    .map_err(|error| TerminalError::io("bold attribute", error))?;
            }
            if cell.attributes.italic {
                self.writer
                    .queue(SetAttribute(Attribute::Italic))
                    .map_err(|error| TerminalError::io("italic attribute", error))?;
            }
            if cell.attributes.dim {
                self.writer
                    .queue(SetAttribute(Attribute::Dim))
                    .map_err(|error| TerminalError::io("dim attribute", error))?;
            }
            if cell.attributes.reverse {
                self.writer
                    .queue(SetAttribute(Attribute::Reverse))
                    .map_err(|error| TerminalError::io("reverse attribute", error))?;
            }
            if let Some(attribute) = resolved_underline(cell.attributes.underline, self.undercurl) {
                self.writer
                    .queue(SetAttribute(attribute))
                    .map_err(|error| TerminalError::io("underline attribute", error))?;
            }
            let safe = terminal_safe_bytes(&cell.text);
            self.writer
                .write_all(&safe)
                .map_err(|error| TerminalError::io("cell text", error))?;
            changed += 1;
        }
        self.writer.flush().map_err(|error| TerminalError::io("frame flush", error))?;
        self.previous = Some(frame.clone());
        Ok(changed)
    }

    pub fn reset_style(&mut self) -> Result<(), TerminalError> {
        self.writer.queue(ResetColor).map_err(|error| TerminalError::io("color reset", error))?;
        self.writer
            .queue(SetAttribute(Attribute::Reset))
            .map_err(|error| TerminalError::io("attribute reset", error))?;
        Ok(())
    }
}

fn resolved_underline(style: UnderlineStyle, undercurl: bool) -> Option<Attribute> {
    match style {
        UnderlineStyle::None => None,
        UnderlineStyle::Straight => Some(Attribute::Underlined),
        UnderlineStyle::Curl if undercurl => Some(Attribute::Undercurled),
        UnderlineStyle::Curl => Some(Attribute::Underlined),
    }
}

/// Expected UTF-8 sequence length for a leading byte, or 0 when it cannot
/// begin a valid sequence.
fn utf8_sequence_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// Caret notation (`^X`, `^@`, `^[`, `^?`, …) for a C0 control or DEL byte.
fn control_caret(byte: u8) -> [u8; 2] {
    let code = match byte {
        0x00 => b'@',
        0x01..=0x1a => b'@' + byte,
        0x1b => b'[',
        0x1c => b'\\',
        0x1d => b']',
        0x1e => b'^',
        0x1f => b'_',
        0x7f => b'?',
        _ => b'?',
    };
    [b'^', code]
}

/// Render `text` so it is safe to write to a terminal: every byte emitted is a
/// printable ASCII or valid printable-UTF-8 glyph, so hostile or malformed
/// cell content cannot inject terminal control sequences. C0 controls and DEL
/// become caret notation (`^X`); C1 controls and invalid/incomplete bytes
/// become `\xHH` hex escapes. The original bytes are retained unchanged in the
/// [`Frame`]/cell state (the byte-exact contract); only the bytes handed to the
/// terminal writer are re-encoded.
fn terminal_safe_bytes(text: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let first = text[index];
        match first {
            // C0 controls and DEL are escaped in caret notation.
            0x00..=0x1f | 0x7f => {
                output.extend_from_slice(&control_caret(first));
                index += 1;
            }
            // Printable ASCII passes through verbatim.
            0x20..=0x7e => {
                output.push(first);
                index += 1;
            }
            // Possible multi-byte UTF-8 leading byte.
            _ => {
                let len = utf8_sequence_len(first);
                let end = index.saturating_add(len);
                if len >= 2
                    && end <= text.len()
                    && let Ok(chunk) = std::str::from_utf8(&text[index..end])
                    && let Some(character) = chunk.chars().next()
                {
                    if character.is_control() {
                        // C1 control character (U+0080..U+009F).
                        output.extend_from_slice(format!("\\x{:02X}", character as u32).as_bytes());
                    } else {
                        output.extend_from_slice(&text[index..end]);
                    }
                    index = end;
                } else {
                    // Lone high byte or truncated multi-byte sequence: raw
                    // high bytes include the C1 control spans, so escape
                    // instead of leaking them into the terminal.
                    output.extend_from_slice(format!("\\x{:02X}", first).as_bytes());
                    index += 1;
                }
            }
        }
    }
    output
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SessionState {
    raw_mode: bool,
    cursor_configured: bool,
    kitty_keyboard: bool,
    synchronized_output: bool,
    palette_restore_pending: bool,
}

pub struct TerminalSession<W: Write> {
    writer: W,
    state: SessionState,
    capabilities: TerminalCapabilities,
}

impl<W: Write> TerminalSession<W> {
    pub fn start(writer: W, capabilities: TerminalCapabilities) -> Result<Self, TerminalError> {
        enable_raw_mode().map_err(|error| TerminalError::io("raw-mode enable", error))?;
        let mut session = Self {
            writer,
            state: SessionState {
                raw_mode: true,
                palette_restore_pending: true,
                ..SessionState::default()
            },
            capabilities,
        };
        session.state.cursor_configured = true;
        if let Err(error) = session.writer.write_all(CURSOR_SETUP) {
            let _ = session.restore();
            return Err(TerminalError::io("cursor setup", error));
        }
        if capabilities.kitty_keyboard {
            session.state.kitty_keyboard = true;
            if let Err(error) = session.writer.write_all(KITTY_PUSH) {
                let _ = session.restore();
                return Err(TerminalError::io("kitty keyboard push", error));
            }
        }
        if let Err(error) = session.writer.flush() {
            let _ = session.restore();
            return Err(TerminalError::io("session setup flush", error));
        }
        Ok(session)
    }

    pub const fn capabilities(&self) -> TerminalCapabilities {
        self.capabilities
    }

    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    pub fn begin_synchronized_output(&mut self) -> Result<bool, TerminalError> {
        if !self.capabilities.synchronized_output || self.state.synchronized_output {
            return Ok(false);
        }
        self.state.synchronized_output = true;
        self.writer
            .write_all(SYNC_BEGIN)
            .map_err(|error| TerminalError::io("synchronized-output begin", error))?;
        Ok(true)
    }

    pub fn end_synchronized_output(&mut self) -> Result<bool, TerminalError> {
        if !self.state.synchronized_output {
            return Ok(false);
        }
        self.writer
            .write_all(SYNC_END)
            .map_err(|error| TerminalError::io("synchronized-output end", error))?;
        self.state.synchronized_output = false;
        Ok(true)
    }

    pub fn program_palette(&mut self, entries: &[(u8, Rgb)]) -> Result<bool, TerminalError> {
        if self.capabilities.palette == PaletteDecision::RestoreOnly || entries.is_empty() {
            return Ok(false);
        }
        self.state.palette_restore_pending = true;
        for (slot, color) in entries {
            match self.capabilities.palette {
                PaletteDecision::Direct => write!(
                    self.writer,
                    "\x1b]4;{slot};rgb:{:02x}/{:02x}/{:02x}\x1b\\",
                    color.r, color.g, color.b
                ),
                PaletteDecision::TmuxPassthrough => write!(
                    self.writer,
                    "\x1bPtmux;\x1b\x1b]4;{slot};rgb:{:02x}/{:02x}/{:02x}\x1b\x1b\\\x1b\\",
                    color.r, color.g, color.b
                ),
                PaletteDecision::RestoreOnly => Ok(()),
            }
            .map_err(|error| TerminalError::io("palette program", error))?;
        }
        self.writer.flush().map_err(|error| TerminalError::io("palette flush", error))?;
        Ok(true)
    }

    /// Restores independent terminal features in the required order and keeps
    /// attempting later steps after an earlier write fails.
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        let mut first_error = None;
        if self.state.synchronized_output {
            match self.writer.write_all(SYNC_END) {
                Ok(()) => self.state.synchronized_output = false,
                Err(error) => first_error = Some(TerminalError::io("synchronized-output restore", error)),
            }
        }
        if self.state.kitty_keyboard {
            match self.writer.write_all(KITTY_POP) {
                Ok(()) => self.state.kitty_keyboard = false,
                Err(error) if first_error.is_none() => {
                    first_error = Some(TerminalError::io("kitty keyboard restore", error));
                }
                Err(_) => {}
            }
        }
        if self.state.cursor_configured {
            match self.writer.write_all(CURSOR_RESTORE) {
                Ok(()) => self.state.cursor_configured = false,
                Err(error) if first_error.is_none() => {
                    first_error = Some(TerminalError::io("cursor restore", error));
                }
                Err(_) => {}
            }
        }
        if self.state.palette_restore_pending {
            let restore = match self.capabilities.palette {
                PaletteDecision::TmuxPassthrough => TMUX_PALETTE_RESTORE,
                PaletteDecision::Direct | PaletteDecision::RestoreOnly => PALETTE_RESTORE,
            };
            match self.writer.write_all(restore) {
                Ok(()) => self.state.palette_restore_pending = false,
                Err(error) if first_error.is_none() => {
                    first_error = Some(TerminalError::io("palette restore", error));
                }
                Err(_) => {}
            }
        }
        if let Err(error) = self.writer.flush() {
            if first_error.is_none() {
                first_error = Some(TerminalError::io("terminal restore flush", error));
            }
        }
        if self.state.raw_mode {
            match disable_raw_mode() {
                Ok(()) => self.state.raw_mode = false,
                Err(error) if first_error.is_none() => {
                    first_error = Some(TerminalError::io("raw-mode restore", error));
                }
                Err(_) => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<W: Write> Drop for TerminalSession<W> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessFailureKind {
    Spawn,
    RpcEof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessFailure {
    pub kind: ProcessFailureKind,
    pub child_stderr: Vec<u8>,
    pub exit_code: Option<i32>,
}

impl ProcessFailure {
    pub fn write_diagnostic(&self, writer: &mut impl Write) -> Result<(), TerminalError> {
        let message = match self.kind {
            ProcessFailureKind::Spawn => {
                "Oxvim embed failed to spawn — check the server executable and retry.\n"
            }
            ProcessFailureKind::RpcEof => {
                "Oxvim embed connection closed — review the child error below and retry.\n"
            }
        };
        writer
            .write_all(message.as_bytes())
            .map_err(|error| TerminalError::io("process diagnostic", error))?;
        writer
            .write_all(&self.child_stderr)
            .map_err(|error| TerminalError::io("child stderr", error))?;
        if !self.child_stderr.is_empty() && !self.child_stderr.ends_with(b"\n") {
            writer
                .write_all(b"\n")
                .map_err(|error| TerminalError::io("child stderr separator", error))?;
        }
        match self.exit_code {
            Some(code) => writeln!(writer, "Embed exited with code {code}.")
                .map_err(|error| TerminalError::io("exit-code diagnostic", error)),
            None => Ok(()),
        }
    }
}

pub fn restore_after_process_failure<W: Write>(
    session: &mut TerminalSession<W>,
    failure: &ProcessFailure,
    diagnostics: &mut impl Write,
) -> Result<(), TerminalError> {
    let restore_result = session.restore();
    let diagnostic_result = failure.write_diagnostic(diagnostics);
    restore_result.and(diagnostic_result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            colors: ColorSupport::TrueColor,
            kitty_keyboard: true,
            synchronized_output: true,
            undercurl: true,
            colored_underline: true,
            osc52_clipboard: true,
            palette: PaletteDecision::Direct,
        }
    }

    fn cell(text: &str) -> Cell {
        Cell { text: text.as_bytes().to_vec(), ..Cell::default() }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
    }

    #[test]
    fn capability_detection_requires_explicit_evidence() {
        let environment = TerminalEnvironment {
            colorterm: Some("TRUECOLOR".to_owned()),
            ..TerminalEnvironment::default()
        };
        assert_eq!(detect_color_support(&environment), ColorSupport::TrueColor);
        assert!(colorterm_is_truecolor(" 24BIT "));
        assert!(terminfo_has_truecolor("xterm-direct|Tc, colors#256"));
        assert!(terminfo_has_truecolor("setrgbf=abc,setrgbb=def"));
        assert!(!terminfo_has_truecolor("setrgbf=abc,colors#256"));
        assert_eq!(
            detect_color_support(&TerminalEnvironment {
                terminfo_colors: Some(256),
                ..TerminalEnvironment::default()
            }),
            ColorSupport::Xterm256
        );
        assert_eq!(ProbePolicy::default().timeout, Duration::from_millis(100));
    }

    #[test]
    fn pure_probe_parsers_handle_supported_unsupported_and_partial_replies() {
        assert_eq!(parse_kitty_keyboard_response(b"noise\x1b[?3u\x1b[?1;2c"), ProbeAnswer::Supported);
        assert_eq!(parse_kitty_keyboard_response(b"\x1b[?1;2c"), ProbeAnswer::Unsupported);
        assert_eq!(parse_kitty_keyboard_response(b"\x1b[?3"), ProbeAnswer::Incomplete);
        assert_eq!(parse_sync_output_response(b"\x1b[?2026;1$y"), ProbeAnswer::Supported);
        assert_eq!(parse_sync_output_response(b"\x1b[?2026;0$y"), ProbeAnswer::Unsupported);
        assert_eq!(parse_sync_output_response(b"\x1b[?2026;"), ProbeAnswer::Incomplete);
        assert_eq!(
            parse_undercurl_response(b"\x1bP1$r4:3m\x1b\\", false),
            ProbeAnswer::Supported
        );
        assert_eq!(parse_undercurl_response(b"", true), ProbeAnswer::Supported);
        assert_eq!(parse_undercurl_response(b"\x1bP0$r\x1b\\", false), ProbeAnswer::Unsupported);
        assert_eq!(parse_osc52_response(b"\x1b]52;c;YWJj\x07"), ProbeAnswer::Supported);
        assert_eq!(parse_osc52_response(b"\x1b]52;c;YWJj"), ProbeAnswer::Incomplete);
    }

    #[test]
    fn live_probe_wires_queries_to_parsers() {
        let (mut server, mut client) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        server
            .write_all(b"\x1b[?3u\x1b[?2026;1$y\x1bP1$r4:3m\x1b\\\x1b]52;c;dGVzdA\x07")
            .expect("write fixtures");
        drop(server);
        let mut writer = Vec::new();
        let environment = TerminalEnvironment {
            terminfo: None,
            ..TerminalEnvironment::default()
        };
        let capabilities =
            probe_io(&mut client, &mut writer, &environment, ProbePolicy::default())
                .expect("probe succeeds");
        assert!(capabilities.kitty_keyboard);
        assert!(capabilities.synchronized_output);
        assert!(capabilities.undercurl);
        assert!(capabilities.colored_underline);
        assert!(capabilities.osc52_clipboard);
        assert!(writer.starts_with(KITTY_KEYBOARD_QUERY));
        assert!(writer.windows(SYNC_OUTPUT_QUERY.len()).any(|w| w == SYNC_OUTPUT_QUERY));
        assert!(writer.windows(UNDERCURL_QUERY.len()).any(|w| w == UNDERCURL_QUERY));
        assert!(writer.windows(OSC52_QUERY.len()).any(|w| w == OSC52_QUERY));
    }

    #[test]
    fn terminfo_smulx_short_circuits_the_undercurl_probe() {
        assert!(terminfo_colored_underline(Some("xterm-kitty|Smulx")));
        assert!(terminfo_colored_underline(Some("setrgbf=abc,smulx")));
        assert!(!terminfo_colored_underline(Some("xterm-256color")));
        assert!(!terminfo_colored_underline(None));
    }

    #[test]
    fn unmatched_probe_response_leaves_features_disabled() {
        let (mut server, mut client) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        server.write_all(b"\x1b[\x1b]52;unterminated").expect("write noise");
        drop(server);
        let mut writer = Vec::new();
        let environment = TerminalEnvironment::default();
        let capabilities =
            probe_io(&mut client, &mut writer, &environment, ProbePolicy::default())
                .expect("probe succeeds");
        assert!(!capabilities.kitty_keyboard);
        assert!(!capabilities.synchronized_output);
        assert!(!capabilities.undercurl);
        assert!(!capabilities.osc52_clipboard);
    }

    #[test]
    fn tmux_without_passthrough_never_programs_palette() {
        assert_eq!(osc4_decision(false, false), PaletteDecision::Direct);
        assert_eq!(osc4_decision(true, true), PaletteDecision::TmuxPassthrough);
        assert_eq!(osc4_decision(true, false), PaletteDecision::RestoreOnly);
    }

    #[test]
    fn frame_validation_is_typed() {
        assert_eq!(
            Frame::new(2, 1, vec![Cell::default()]),
            Err(FrameError::CellCount { expected: 2, actual: 1 })
        );
        assert_eq!(
            Frame::new(1, 1, vec![Cell { text: Vec::new(), ..Cell::default() }]),
            Err(FrameError::EmptyCell { index: 0 })
        );
    }

    #[test]
    fn damage_writer_emits_only_changed_cells() {
        let initial = Frame::new(2, 1, vec![cell("a"), cell("b")]).expect("valid frame");
        let changed = Frame::new(2, 1, vec![cell("a"), cell("c")]).expect("valid frame");
        let mut writer = DamageWriter::new(Vec::new(), true);
        assert_eq!(writer.render(&initial).expect("initial render"), 2);
        let first_len = writer.writer.len();
        assert_eq!(writer.render(&initial).expect("unchanged render"), 0);
        assert_eq!(writer.writer.len(), first_len);
        assert_eq!(writer.render(&changed).expect("changed render"), 1);
        let delta = &writer.writer[first_len..];
        assert!(delta.windows(4).any(|window| window == b"\x1b[2G") || delta.ends_with(b"c"));
    }

    #[test]
    fn terminal_safe_bytes_escapes_controls_and_invalid_bytes() {
        // Printable ASCII and multibyte UTF-8 pass through unchanged.
        assert_eq!(terminal_safe_bytes(b"plain \xe7\x95\x8c"), b"plain \xe7\x95\x8c");
        // C0 controls become caret notation.
        assert_eq!(terminal_safe_bytes(b"\x00\x01\x1b\x7f"), b"^@^A^[^?");
        // C1 controls and invalid lone bytes become hex escapes.
        assert_eq!(terminal_safe_bytes(b"\x9b\xff\xc0\xb0"), b"\\x9B\\xFF\\xC0\\xB0");
        // A validly-encoded C1 control and a lone continuation byte.
        assert_eq!(terminal_safe_bytes(b"a\xc2\x85b\x80"), b"a\\x85b\\x80");
        // Escaping is never skipped: every output byte is printable ASCII.
        assert!(terminal_safe_bytes(b"\x1b[2J\x00\xfe").iter().all(|b| b.is_ascii()));
    }

    #[test]
    fn damage_writer_skips_continuation_cells() {
        let frame = Frame::new(2, 1, vec![cell("界"), Cell::continuation()]).expect("valid frame");
        let mut writer = DamageWriter::new(Vec::new(), false);
        // Only the leading glyph is drawn; the continuation cell is skipped.
        assert_eq!(writer.render(&frame).expect("render"), 1);
    }

    #[test]
    fn damage_writer_escapes_control_bytes_in_cell_text() {
        let frame = Frame::new(
            1,
            1,
            vec![Cell { text: b"a\x1b[2J\x7f\x00\x9b\xff\xc0\xb0".to_vec(), ..Cell::default() }],
        )
        .expect("valid frame");
        let mut writer = DamageWriter::new(Vec::new(), false);
        writer.render(&frame).expect("render");
        let out = &writer.writer;
        // The cell text is re-encoded to a printable visible form...
        let escaped = b"a^[[2J^?^@\\x9B\\xFF\\xC0\\xB0";
        assert!(
            out.windows(escaped.len()).any(|window| window == escaped),
            "escaped cell text must be present, got {out:?}"
        );
        // ...and the raw control sequence and control bytes never reach the terminal.
        assert!(!out.windows(4).any(|window| window == b"\x1b[2J"), "ESC sequence leaked: {out:?}");
        for forbidden in [b"\x00", b"\x7f", b"\x9b", b"\xff", b"\xc0"] {
            assert!(
                !out.windows(forbidden.len()).any(|window| window == forbidden),
                "raw byte leaked: {forbidden:?} in {out:?}"
            );
        }
    }

    #[test]
    fn resize_invalidates_the_retained_frame() {
        let narrow = Frame::new(1, 1, vec![cell("a")]).expect("valid frame");
        let wide = Frame::new(2, 1, vec![cell("a"), cell("b")]).expect("valid frame");
        let mut writer = DamageWriter::new(Vec::new(), false);
        assert_eq!(writer.render(&narrow).expect("narrow render"), 1);
        assert_eq!(writer.render(&wide).expect("wide render"), 2);
    }

    #[test]
    fn failed_output_does_not_advance_retained_frame() {
        let frame = Frame::new(1, 1, vec![cell("a")]).expect("valid frame");
        let mut writer = DamageWriter::new(FailingWriter, false);
        let error = writer.render(&frame).expect_err("write must fail");
        assert!(matches!(error, TerminalError::Io { operation: "cursor move", .. }));
        assert!(writer.previous().is_none());
    }

    #[test]
    fn undercurl_has_a_plain_underline_fallback() {
        assert_eq!(resolved_underline(UnderlineStyle::Curl, true), Some(Attribute::Undercurled));
        assert_eq!(resolved_underline(UnderlineStyle::Curl, false), Some(Attribute::Underlined));
    }

    #[test]
    fn palette_program_and_restore_sequences_are_ordered() {
        let mut session = TerminalSession {
            writer: Vec::new(),
            state: SessionState {
                raw_mode: false,
                cursor_configured: true,
                kitty_keyboard: true,
                synchronized_output: true,
                palette_restore_pending: true,
            },
            capabilities: capabilities(),
        };
        session
            .program_palette(&[(0, Rgb::new(0x16, 0x18, 0x1d))])
            .expect("palette program");
        session.restore().expect("restore");
        let bytes = String::from_utf8(session.writer.clone()).expect("ANSI is UTF-8");
        let sync = bytes.find("\x1b[?2026l").expect("sync close");
        let kitty = bytes.find("\x1b[<u").expect("kitty pop");
        let cursor = bytes.find("\x1b[0 q").expect("cursor reset");
        let palette = bytes.find("\x1b]104").expect("palette restore");
        assert!(sync < kitty && kitty < cursor && cursor < palette);
    }

    #[test]
    fn tmux_palette_sequences_use_dcs_passthrough() {
        let mut tmux_capabilities = capabilities();
        tmux_capabilities.palette = PaletteDecision::TmuxPassthrough;
        let mut session = TerminalSession {
            writer: Vec::new(),
            state: SessionState::default(),
            capabilities: tmux_capabilities,
        };
        session
            .program_palette(&[(2, Rgb::new(0x11, 0x22, 0x33))])
            .expect("tmux palette program");
        session.restore().expect("tmux palette restore");
        assert!(session.writer.starts_with(b"\x1bPtmux;\x1b\x1b]4;2;rgb:11/22/33"));
        assert!(session.writer.ends_with(TMUX_PALETTE_RESTORE));
    }

    #[test]
    fn process_failure_restores_before_preserving_child_stderr() {
        let mut session = TerminalSession {
            writer: Vec::new(),
            state: SessionState { cursor_configured: true, ..SessionState::default() },
            capabilities: capabilities(),
        };
        let failure = ProcessFailure {
            kind: ProcessFailureKind::RpcEof,
            child_stderr: b"server detail".to_vec(),
            exit_code: Some(7),
        };
        let mut diagnostics = Vec::new();
        restore_after_process_failure(&mut session, &failure, &mut diagnostics).expect("process edge");
        assert!(session.writer.starts_with(CURSOR_RESTORE));
        assert!(diagnostics.windows(13).any(|window| window == b"server detail"));
        assert!(diagnostics.ends_with(b"Embed exited with code 7.\n"));
    }
}
