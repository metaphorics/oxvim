#![forbid(unsafe_code)]
//! Redesigned bundled TUI client.
//!
//! The crate is a pure stdio msgpack-RPC client. It owns only protocol-provided
//! chrome surfaces and never links editor or server-side UI implementation code.

pub mod chrome;
pub mod client;
pub mod screen;
pub mod terminal;
pub mod theme;

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Write};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrome::{
    Chrome, ChromeError, ChunkLine, HistoryEntry, MessageUpdate, PopupItem, Rect, TextChunk, TimeMs,
};
use client::{Client, ClientError};
use crossterm::Command as _;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ox_rpc::RedrawEvent;
use ox_types::{Dict, Object, OxStr};
use screen::{ApplyOutcome, ComposedGrid, Screen, ScreenError};
use terminal::{
    Cell as TerminalCell, CellAttributes, ColorSupport, DamageWriter, Frame, FrameError,
    ProcessFailure, ProcessFailureKind, ProbePolicy, TerminalCapabilities, TerminalColor,
    TerminalEnvironment, TerminalError, TerminalSession, UnderlineStyle,
};
use theme::{HighlightGroup, HighlightStyle, Rgb, Theme};
use thiserror::Error;

const LOOP_SLICE: Duration = Duration::from_millis(16);
const NOTIFICATION_FADE_MS: u64 = 150;

/// Motion policy selected from `OXVIM_TUI_MOTION`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MotionPolicy {
    /// Instant changes, including notifications.
    #[default]
    Reduced,
    /// The one sanctioned 150 ms notification opacity fade.
    Full,
}

impl MotionPolicy {
    /// Read the process setting. Only the exact value `full` opts into motion.
    #[must_use]
    pub fn from_environment() -> Self {
        match env::var("OXVIM_TUI_MOTION") {
            Ok(value) if value == "full" => Self::Full,
            _ => Self::Reduced,
        }
    }

    /// Notification opacity at `elapsed_ms`, using a cubic ease-out curve.
    #[must_use]
    pub fn notification_opacity(self, elapsed_ms: u64) -> f64 {
        if self == Self::Reduced || elapsed_ms >= NOTIFICATION_FADE_MS {
            return 1.0;
        }
        let progress = elapsed_ms as f64 / NOTIFICATION_FADE_MS as f64;
        1.0 - (1.0 - progress).powi(3)
    }
}

/// Complete headless state for one attached TUI.
#[derive(Clone, Debug)]
pub struct TuiState {
    /// Server grid state.
    pub screen: Screen,
    /// Client-owned externalized UI surfaces.
    pub chrome: Chrome,
    /// Current client-surface theme.
    pub theme: Theme,
    /// Selected motion posture.
    pub motion: MotionPolicy,
    /// Whether the editor has asked for mouse input (`mouse_on`). Drives
    /// crossterm's terminal mouse-capture enable/disable at the run boundary.
    pub mouse_capture: bool,
    highlight_groups: BTreeMap<HighlightGroup, HighlightStyle>,
    current_time: TimeMs,
    notification_started: Option<TimeMs>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new(None, MotionPolicy::Reduced)
    }
}

impl TuiState {
    /// Create a headless state with the pre-highlight `$COLORFGBG` fallback.
    #[must_use]
    pub fn new(colorfgbg: Option<&str>, motion: MotionPolicy) -> Self {
        Self {
            screen: Screen::new(),
            chrome: Chrome::default(),
            theme: Theme::new(colorfgbg),
            motion,
            mouse_capture: false,
            highlight_groups: BTreeMap::new(),
            current_time: TimeMs(0),
            notification_started: None,
        }
    }

    /// Apply a complete redraw batch and start stable-batch message timers.
    pub fn apply_redraw(
        &mut self,
        events: &[RedrawEvent],
        now: TimeMs,
    ) -> Result<(), TuiError> {
        self.current_time = now;
        for redraw in events {
            match self.screen.apply_event(redraw)? {
                ApplyOutcome::Applied => self.capture_highlight(redraw),
                ApplyOutcome::Unknown(event) => self.apply_client_event(&event)?,
            }
        }
        self.chrome.finish_batch(now);
        self.chrome.advance_time(now);
        Ok(())
    }

    /// Advance time-dependent chrome without requiring a server redraw.
    pub fn advance_time(&mut self, now: TimeMs) -> bool {
        self.current_time = now;
        let before = self.chrome.messages.len();
        self.chrome.advance_time(now);
        before != self.chrome.messages.len()
    }

    fn notification_opacity(&self) -> f64 {
        let elapsed = self.notification_started.map_or(NOTIFICATION_FADE_MS, |started| {
            self.current_time.0.saturating_sub(started.0)
        });
        self.motion.notification_opacity(elapsed)
    }

    /// Render the server grid without terminal escape sequences.
    pub fn render_to_string(&self) -> Result<String, TuiError> {
        Ok(self.screen.composed_grid()?.render_to_string())
    }

    fn capture_highlight(&mut self, event: &RedrawEvent) {
        if event.name.as_bytes() != b"hl_attr_define" {
            return;
        }
        for args in &event.argsets {
            let Some(Object::Dict(rgb)) = args.get(1) else {
                continue;
            };
            let Some(Object::Array(info)) = args.get(3) else {
                continue;
            };
            let style = highlight_style(rgb);
            for item in info {
                let Object::Dict(metadata) = item else {
                    continue;
                };
                for key in ["ui_name", "hi_name"] {
                    let Some(Object::String(name)) = dict_get(metadata, key) else {
                        continue;
                    };
                    let Some(group) = HighlightGroup::from_name(&name.to_string_lossy()) else {
                        continue;
                    };
                    self.highlight_groups.insert(group, style);
                }
            }
        }
        self.theme.reswap(self.highlight_groups.iter().map(|(group, style)| (*group, *style)));
    }

    fn apply_client_event(&mut self, event: &RedrawEvent) -> Result<(), TuiError> {
        for args in &event.argsets {
            match event.name.as_bytes() {
                b"mouse_on" => self.mouse_capture = true,
                b"mouse_off" => self.mouse_capture = false,
                b"flush" | b"option_set" | b"default_colors_set" | b"hl_group_set"
                | b"busy_start" | b"busy_stop"
                | b"bell" | b"visual_bell" | b"update_menu" | b"menu_show"
                | b"menu_hide" | b"set_title" | b"set_icon" => {
                    // These carry no client-rendered content, so they are
                    // consumed intentionally: the client must survive its
                    // first redraw regardless of which core negotiation/status
                    // events the server emits.
                }
                b"cmdline_show" => self.apply_cmdline_show(args)?,
                b"cmdline_pos" => {
                    self.chrome.cmdline_pos(as_u32(args, 1, "cmdline_pos")?, as_usize(args, 0, "cmdline_pos")?);
                }
                b"cmdline_special_char" => {
                    self.chrome.cmdline_special_char(
                        as_u32(args, 2, "cmdline_special_char")?,
                        as_string(args, 0, "cmdline_special_char")?.as_bytes(),
                        as_bool(args, 1, "cmdline_special_char")?,
                    );
                }
                b"cmdline_hide" => {
                    self.chrome.cmdline_hide(
                        as_u32(args, 0, "cmdline_hide")?,
                        as_bool(args, 1, "cmdline_hide")?,
                    );
                }
                b"cmdline_block_show" => {
                    self.chrome.cmdline_block_show(as_chunk_lines(args, 0, "cmdline_block_show")?);
                }
                b"cmdline_block_append" => {
                    for line in as_chunk_lines(args, 0, "cmdline_block_append")? {
                        self.chrome.cmdline_block_append(line);
                    }
                }
                b"cmdline_block_hide" => {
                    self.chrome.cmdline_block_hide();
                }
                b"popupmenu_show" => self.apply_popupmenu_show(args)?,
                b"popupmenu_select" => {
                    self.chrome.popupmenu_select(optional_selection(args, 0, "popupmenu_select")?);
                }
                b"popupmenu_hide" => self.chrome.popupmenu_hide(),
                b"wildmenu_show" => {
                    let items = as_string_array(args, 0, "wildmenu_show")?
                        .into_iter()
                        .map(|word| PopupItem::new(word.as_bytes(), b"", b"", b""))
                        .collect();
                    self.chrome.popupmenu_show(items, None, 0, 0, -1);
                }
                b"wildmenu_select" => {
                    self.chrome.popupmenu_select(optional_selection(args, 0, "wildmenu_select")?);
                }
                b"wildmenu_hide" => self.chrome.popupmenu_hide(),
                b"msg_show" => self.apply_message_show(args)?,
                b"msg_clear" => self.chrome.message_clear(),
                b"msg_history_show" => self.apply_history_show(args)?,
                b"msg_showcmd" | b"msg_showmode" | b"msg_ruler" => {
                    require_arity(args, 1, &event.name.to_string_lossy())?;
                    let kind = event.name.clone();
                    self.chrome.message_show(MessageUpdate {
                        kind: kind.clone(),
                        content: as_chunks(args, 0, &kind.to_string_lossy())?,
                        replace_last: false,
                        history: false,
                        append: false,
                        id: Object::String(kind),
                        prompt: false,
                    })?;
                }
                _ => return Err(TuiError::NotImplemented(event.name.clone())),
            }
        }
        Ok(())
    }

    fn apply_cmdline_show(&mut self, args: &[Object]) -> Result<(), TuiError> {
        require_arity(args, 7, "cmdline_show")?;
        let prompt = match as_string(args, 3, "cmdline_show")? {
            value if value.as_bytes().is_empty() => None,
            value => Some(value.clone()),
        };
        self.chrome.cmdline_show(
            as_u32(args, 5, "cmdline_show")?,
            as_chunks(args, 0, "cmdline_show")?,
            as_usize(args, 1, "cmdline_show")?,
            as_string(args, 2, "cmdline_show")?.as_bytes(),
            prompt,
            as_usize(args, 4, "cmdline_show")?,
            as_i64(args, 6, "cmdline_show")?,
        );
        Ok(())
    }

    fn apply_popupmenu_show(&mut self, args: &[Object]) -> Result<(), TuiError> {
        require_arity(args, 5, "popupmenu_show")?;
        self.chrome.popupmenu_show(
            as_popup_items(args, 0, "popupmenu_show")?,
            optional_selection(args, 1, "popupmenu_show")?,
            as_usize(args, 2, "popupmenu_show")?,
            as_usize(args, 3, "popupmenu_show")?,
            as_i64(args, 4, "popupmenu_show")?,
        );
        Ok(())
    }

    fn apply_message_show(&mut self, args: &[Object]) -> Result<(), TuiError> {
        require_arity(args, 7, "msg_show")?;
        let trigger = as_string(args, 6, "msg_show")?;
        self.chrome.message_show(MessageUpdate {
            kind: as_string(args, 0, "msg_show")?.clone(),
            content: as_chunks(args, 1, "msg_show")?,
            replace_last: as_bool(args, 2, "msg_show")?,
            history: as_bool(args, 3, "msg_show")?,
            append: as_bool(args, 4, "msg_show")?,
            id: args[5].clone(),
            prompt: !trigger.as_bytes().is_empty(),
        })?;
        self.notification_started = Some(self.current_time);
        Ok(())
    }

    fn apply_history_show(&mut self, args: &[Object]) -> Result<(), TuiError> {
        require_arity(args, 2, "msg_history_show")?;
        let Object::Array(entries) = &args[0] else {
            return Err(TuiError::Protocol("msg_history_show entries must be an array".into()));
        };
        let mut history = Vec::with_capacity(entries.len());
        for entry in entries {
            let Object::Array(fields) = entry else {
                return Err(TuiError::Protocol("message history entry must be an array".into()));
            };
            require_arity(fields, 2, "msg_history_show entry")?;
            history.push(HistoryEntry {
                kind: object_string(&fields[0], "msg_history_show kind")?.clone(),
                content: chunks_from_object(&fields[1], "msg_history_show content")?,
                append: false,
            });
        }
        self.chrome.history_show(history, as_bool(args, 1, "msg_history_show")?);
        Ok(())
    }
}

/// Main-loop or decoded-event failure.
#[derive(Debug, Error)]
pub enum TuiError {
    /// RPC transport failure.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// Server-grid event failure.
    #[error(transparent)]
    Screen(#[from] ScreenError),
    /// Client-chrome event failure.
    #[error(transparent)]
    Chrome(#[from] ChromeError),
    /// Terminal setup, rendering, or restoration failure.
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    /// Invalid terminal frame dimensions.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// Malformed externalized UI event.
    #[error("invalid UI event: {0}")]
    Protocol(String),
    /// A redraw command has no correct implementation yet.
    #[error("UI event is not implemented: {0:?}")]
    NotImplemented(OxStr),
    /// Terminal input could not be read.
    #[error("could not read terminal input: {0}")]
    Input(io::Error),
}

/// Run an already-spawned client until its RPC stream closes.
pub fn run(mut client: Client) -> Result<(), TuiError> {
    let (width, height) = crossterm::terminal::size().map_err(TuiError::Input)?;
    client.attach(width, height)?;

    let environment = terminal_environment();
    let mut probe_reader = io::stdin();
    let mut probe_writer = io::stdout();
    let capabilities = terminal::probe_terminal(
        &mut probe_reader,
        &mut probe_writer,
        &environment,
        ProbePolicy::default(),
    )?;
    let mut shared = SharedWriter::stdout();
    let mut session = TerminalSession::start(shared.clone(), capabilities)?;
    let mut damage = DamageWriter::new(shared.clone(), capabilities.undercurl);
    let mut state = TuiState::new(env::var("COLORFGBG").ok().as_deref(), MotionPolicy::from_environment());
    let tokens = state.theme.tokens();
    session.program_palette(&[
        (0, tokens.bg),
        (1, tokens.error),
        (3, tokens.warn),
        (6, tokens.hint),
        (7, tokens.fg),
        (8, tokens.fg_muted),
        (9, tokens.accent),
    ])?;
    let started = Instant::now();
    let mut mouse_capture_emitted = false;

    loop {
        match client.recv_redraw_timeout(LOOP_SLICE) {
            Ok(Some(events)) => {
                let now = TimeMs(duration_millis(started.elapsed()));
                state.apply_redraw(&events, now)?;
                if state.mouse_capture != mouse_capture_emitted {
                    mouse_capture_emitted = state.mouse_capture;
                    apply_mouse_capture(&mut shared, state.mouse_capture)?;
                }
                if let Ok(grid) = state.screen.composed_grid() {
                    let frame = render_frame(&grid, &state, capabilities)?;
                    session.begin_synchronized_output()?;
                    let rendered = damage.render(&frame);
                    let close_result = session.end_synchronized_output();
                    rendered?;
                    close_result?;
                }
            }
            Ok(None) => {
                let now = TimeMs(duration_millis(started.elapsed()));
                let expired = state.advance_time(now);
                let fading = state.motion == MotionPolicy::Full
                    && state.notification_opacity() < 1.0;
                if (expired || fading) && let Ok(grid) = state.screen.composed_grid() {
                    let frame = render_frame(&grid, &state, capabilities)?;
                    session.begin_synchronized_output()?;
                    let rendered = damage.render(&frame);
                    let close_result = session.end_synchronized_output();
                    rendered?;
                    close_result?;
                }
            }
            Err(error) => {
                if mouse_capture_emitted {
                    let _ = apply_mouse_capture(&mut shared, false);
                }
                session.restore()?;
                if clean_eof(&error) { return Ok(()); }
                let failure = process_failure(&error);
                failure.write_diagnostic(&mut io::stderr())?;
                return Err(TuiError::Client(error));
            }
        }

        while event::poll(Duration::ZERO).map_err(TuiError::Input)? {
            match event::read().map_err(TuiError::Input)? {
                Event::Key(key) => {
                    state.chrome.keypress();
                    if let Some(input) = encode_key(key) {
                        client.input(OxStr::from(input.as_str()))?;
                    }
                }
                Event::Resize(columns, rows) => client.try_resize(columns, rows)?,
                Event::Mouse(mouse) => {
                    let dimensions = state
                        .screen
                        .composed_grid()
                        .ok()
                        .map(|grid| (grid.width(), grid.height()));
                    let hit_chrome = dimensions
                        .is_some_and(|(columns, rows)| {
                            let cursor_row = state.screen.cursor().map(|cursor| cursor.row);
                            state.chrome.layout(columns, rows, cursor_row).contains(
                                usize::from(mouse.column),
                                usize::from(mouse.row),
                            )
                        });
                    if !hit_chrome {
                        let (button, action, modifier) = encode_mouse(mouse);
                        client.input_mouse(&button, &action, &modifier, mouse.row, mouse.column)?;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Spawn an embed command, restoring and diagnosing process-edge failures in `run`.
pub fn run_command(command: Command) -> Result<(), TuiError> {
    match Client::spawn(command) {
        Ok(client) => run(client),
        Err(error) => {
            let failure = process_failure(&error);
            failure.write_diagnostic(&mut io::stderr())?;
            Err(TuiError::Client(error))
        }
    }
}

/// Enable or disable terminal mouse capture to match the editor's `mouse_on`/
/// `mouse_off` signal. Forwarding mouse input to the editor is only reachable
/// while the terminal actually reports mouse events.
fn apply_mouse_capture(shared: &mut SharedWriter, enabled: bool) -> Result<(), TuiError> {
    let mut command = String::new();
    // EnableMouseCapture/DisableMouseCapture are distinct crossterm Command
    // types (no shared supertype), so they are serialized to ANSI here and
    // written as a single byte string.
    let result = if enabled {
        crossterm::event::EnableMouseCapture.write_ansi(&mut command)
    } else {
        crossterm::event::DisableMouseCapture.write_ansi(&mut command)
    };
    result.map_err(|error| TuiError::Input(io::Error::other(error)))?;
    shared.write_all(command.as_bytes()).map_err(TuiError::Input)?;
    shared.flush().map_err(TuiError::Input)
}

fn render_frame(
    grid: &ComposedGrid,
    state: &TuiState,
    capabilities: TerminalCapabilities,
) -> Result<Frame, TuiError> {
    let width = u16::try_from(grid.width()).map_err(|_| TuiError::Protocol("terminal width exceeds u16".into()))?;
    let height = u16::try_from(grid.height()).map_err(|_| TuiError::Protocol("terminal height exceeds u16".into()))?;
    let mut cells = grid
        .cells()
        .iter()
        .map(|cell| {
            if cell.text.as_bytes().is_empty() {
                return TerminalCell::continuation();
            }
            let mut rendered = TerminalCell {
                text: cell.text.as_bytes().to_vec(),
                ..TerminalCell::default()
            };
            if let Some(highlight) = state.screen.highlight(cell.highlight_id) {
                let attributes = if capabilities.colors == ColorSupport::TrueColor {
                    &highlight.rgb
                } else {
                    &highlight.cterm
                };
                rendered.foreground = terminal_highlight_color(attributes, "foreground", capabilities.colors);
                rendered.background = terminal_highlight_color(attributes, "background", capabilities.colors);
                rendered.attributes = CellAttributes {
                    bold: dict_bool(attributes, "bold"),
                    italic: dict_bool(attributes, "italic"),
                    dim: dict_bool(attributes, "standout"),
                    reverse: dict_bool(attributes, "reverse"),
                    underline: if dict_bool(attributes, "undercurl") {
                        UnderlineStyle::Curl
                    } else if dict_bool(attributes, "underline") {
                        UnderlineStyle::Straight
                    } else {
                        UnderlineStyle::None
                    },
                };
                if let Some(underlay_id) = cell.blend_underlay {
                    if let Some(underlay) = state.screen.highlight(underlay_id) {
                        if let (Some(top), Some(bottom)) = (
                            dict_color(&highlight.rgb, "foreground"),
                            dict_color(&underlay.rgb, "foreground"),
                        ) {
                            rendered.foreground = fallback_color(
                                premix(top, bottom, cell.blend_percentage),
                                capabilities.colors,
                            );
                        }
                        if let (Some(top), Some(bottom)) = (
                            dict_color(&highlight.rgb, "background"),
                            dict_color(&underlay.rgb, "background"),
                        ) {
                            rendered.background = fallback_color(
                                premix(top, bottom, cell.blend_percentage),
                                capabilities.colors,
                            );
                        }
                    }
                }
            }
            rendered
        })
        .collect::<Vec<_>>();
    let cursor_row = state.screen.cursor().map(|cursor| cursor.row);
    let layout = state.chrome.layout(grid.width(), grid.height(), cursor_row);
    if let (Some(rect), Some(popup)) = (layout.insert_popup, &state.chrome.insert_popup) {
        let text = popup.items.iter().flat_map(|item| item.word.as_bytes().iter().copied().chain(std::iter::once(b'\n'))).collect::<Vec<_>>();
        paint_surface(&mut cells, grid.width(), grid.height(), rect, &text, &state.theme, HighlightGroup::Pmenu, capabilities.colors, 1.0);
    }
    if let (Some(rect), Some(popup)) = (layout.documentation, &state.chrome.insert_popup) {
        if let Some(info) = popup.documentation() {
            paint_surface(&mut cells, grid.width(), grid.height(), rect, info, &state.theme, HighlightGroup::NormalFloat, capabilities.colors, 1.0);
        }
    }
    if let (Some(rect), Some(cmdline)) = (layout.cmdline, state.chrome.cmdline.active()) {
        let mut text = Vec::new();
        text.extend_from_slice(cmdline.first_character.as_bytes());
        if let Some(prompt) = &cmdline.prompt {
            text.extend_from_slice(prompt.as_bytes());
        }
        for chunk in &cmdline.content {
            text.extend_from_slice(chunk.text.as_bytes());
        }
        paint_surface(&mut cells, grid.width(), grid.height(), rect, &text, &state.theme, HighlightGroup::NormalFloat, capabilities.colors, 1.0);
    }
    if let (Some(rect), Some(wildlist)) = (layout.wildlist, &state.chrome.sticky_wildlist) {
        let text = wildlist.iter().flat_map(|chunk| chunk.text.as_bytes().iter().copied()).collect::<Vec<_>>();
        paint_surface(&mut cells, grid.width(), grid.height(), rect, &text, &state.theme, HighlightGroup::WildMenu, capabilities.colors, 1.0);
    }
    if let (Some(rect), Some(wildmenu)) = (layout.wildmenu, &state.chrome.wildmenu) {
        let text = wildmenu.items.iter().flat_map(|item| item.word.as_bytes().iter().copied().chain(std::iter::once(b' '))).collect::<Vec<_>>();
        paint_surface(&mut cells, grid.width(), grid.height(), rect, &text, &state.theme, HighlightGroup::WildMenu, capabilities.colors, 1.0);
    }
    if let Some(rect) = layout.messages {
        let visible = state.chrome.visible_messages();
        let mut text = Vec::new();
        for entry in &visible.entries {
            for chunk in &entry.content {
                text.extend_from_slice(chunk.text.as_bytes());
            }
            text.push(b'\n');
        }
        if let Some(badge) = visible.overflow_badge {
            text.extend_from_slice(badge.as_bytes());
        }
        paint_surface(&mut cells, grid.width(), grid.height(), rect, &text, &state.theme, HighlightGroup::MsgArea, capabilities.colors, state.notification_opacity());
    }
    if let (Some(rect), Some(search_count)) = (layout.search_count, &state.chrome.search_count) {
        let text = search_count.iter().flat_map(|chunk| chunk.text.as_bytes().iter().copied()).collect::<Vec<_>>();
        paint_surface(&mut cells, grid.width(), grid.height(), rect, &text, &state.theme, HighlightGroup::MsgArea, capabilities.colors, 1.0);
    }
    if let (Some(rect), Some(history)) = (layout.history, &state.chrome.history_float) {
        let mut text = Vec::new();
        for entry in &history.entries {
            for chunk in &entry.content {
                text.extend_from_slice(chunk.text.as_bytes());
            }
            text.push(b'\n');
        }
        paint_surface(&mut cells, grid.width(), grid.height(), rect, &text, &state.theme, HighlightGroup::NormalFloat, capabilities.colors, 1.0);
    }
    Ok(Frame::new(width, height, cells)?)
}

fn paint_surface(
    cells: &mut [TerminalCell],
    width: usize,
    height: usize,
    rect: Rect,
    text: &[u8],
    theme: &Theme,
    group: HighlightGroup,
    color_support: ColorSupport,
    opacity: f64,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let tokens = theme.tokens();
    let surface = theme.style(group);
    let border_style = theme.style(HighlightGroup::FloatBorder);
    let fade = ((1.0 - opacity.clamp(0.0, 1.0)) * 100.0).round() as u8;
    let foreground = fallback_color(
        premix(surface.foreground.unwrap_or(tokens.fg), tokens.bg, fade),
        color_support,
    );
    let background = fallback_color(
        premix(surface.background.unwrap_or(tokens.float_bg), tokens.bg, fade),
        color_support,
    );
    let border = fallback_color(
        premix(border_style.foreground.unwrap_or(tokens.accent), tokens.bg, fade),
        color_support,
    );
    for y in rect.y..rect.y.saturating_add(rect.height).min(height) {
        for x in rect.x..rect.x.saturating_add(rect.width).min(width) {
            let Some(index) = y.checked_mul(width).and_then(|row| row.checked_add(x)) else {
                continue;
            };
            let Some(cell) = cells.get_mut(index) else {
                continue;
            };
            cell.text = " ".into();
            cell.continuation = false;
            cell.foreground = foreground;
            cell.background = background;
            if y == rect.y || y + 1 == rect.y.saturating_add(rect.height) || x == rect.x || x + 1 == rect.x.saturating_add(rect.width) {
                cell.foreground = border;
            }
        }
    }
    let inner_x = rect.x.saturating_add(1);
    let inner_y = rect.y.saturating_add(1);
    let inner_width = rect.width.saturating_sub(2);
    let inner_height = rect.height.saturating_sub(2);
    let mut x = 0usize;
    let mut y = 0usize;
    let mut offset = 0usize;
    let mut last_glyph_x: Option<usize> = None;
    while offset < text.len() {
        if text[offset] == b'\n' {
            x = 0;
            y = y.saturating_add(1);
            last_glyph_x = None;
            offset = offset.saturating_add(1);
            continue;
        }
        let (consumed, mut advance) = chrome::decoded_cell_width(text, offset, x);
        if advance > 0 && x.saturating_add(advance) > inner_width {
            x = 0;
            y = y.saturating_add(1);
            last_glyph_x = None;
            advance = chrome::decoded_cell_width(text, offset, 0).1.min(inner_width);
        }
        if y >= inner_height {
            break;
        }
        let column = inner_x.saturating_add(x);
        let row = inner_y.saturating_add(y);
        if let Some(cell) = row.checked_mul(width).and_then(|base| base.checked_add(column)).and_then(|index| cells.get_mut(index)) {
            if advance > 0 {
                cell.text = text[offset..offset.saturating_add(consumed)].to_vec();
                cell.foreground = foreground;
                cell.background = background;
                // Overwriting a cell must clear any stale continuation marker
                // (e.g. an earlier wide glyph's trailing column) so the cell is
                // redrawn rather than skipped by the damage writer.
                cell.continuation = false;
                last_glyph_x = Some(x);
                // A double-width glyph paints its trailing column itself. Mark
                // that trailing cell as a continuation so the damage writer
                // skips it instead of overlaying a space over the glyph's
                // right half.
                if advance == 2 {
                    let trailing_column = column.saturating_add(1);
                    if let Some(trailing) = row.checked_mul(width).and_then(|base| base.checked_add(trailing_column)).and_then(|index| cells.get_mut(index)) {
                        trailing.text = Vec::new();
                        trailing.continuation = true;
                        trailing.foreground = foreground;
                        trailing.background = background;
                    }
                }
            } else if let Some(base_x) = last_glyph_x {
                // Zero-width combining/decorative marks overlay the preceding
                // glyph, preserving the original byte sequence.
                let base_column = inner_x.saturating_add(base_x);
                if let Some(base_cell) = row.checked_mul(width).and_then(|base| base.checked_add(base_column)).and_then(|index| cells.get_mut(index)) {
                    base_cell.text.extend_from_slice(&text[offset..offset.saturating_add(consumed)]);
                    base_cell.continuation = false;
                }
            }
        }
        x = x.saturating_add(advance);
        offset = offset.saturating_add(consumed);
    }
}

#[derive(Clone)]
struct SharedWriter(Arc<Mutex<io::Stdout>>);

impl SharedWriter {
    fn stdout() -> Self {
        Self(Arc::new(Mutex::new(io::stdout())))
    }
}

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().map_err(|_| io::Error::other("terminal writer lock poisoned"))?.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().map_err(|_| io::Error::other("terminal writer lock poisoned"))?.flush()
    }
}

fn terminal_environment() -> TerminalEnvironment {
    let term = env::var("TERM").ok();
    TerminalEnvironment {
        colorterm: env::var("COLORTERM").ok(),
        terminfo: term.clone(),
        terminfo_colors: term.as_deref().and_then(|value| value.contains("256color").then_some(256)),
        inside_tmux: env::var_os("TMUX").is_some(),
        tmux_passthrough: env::var("TMUX_PASSTHROUGH").is_ok_and(|value| value == "1"),
        term,
    }
}

fn process_failure(error: &ClientError) -> ProcessFailure {
    match error {
        ClientError::Spawn { .. } => ProcessFailure { kind: ProcessFailureKind::Spawn, child_stderr: Vec::new(), exit_code: None },
        ClientError::Eof { exit_code, stderr } | ClientError::NonZeroExit { exit_code, stderr } => ProcessFailure {
            kind: ProcessFailureKind::RpcEof,
            child_stderr: stderr.clone(),
            exit_code: *exit_code,
        },
        _ => ProcessFailure { kind: ProcessFailureKind::RpcEof, child_stderr: Vec::new(), exit_code: None },
    }
}

fn clean_eof(error: &ClientError) -> bool {
    matches!(error, ClientError::Eof { exit_code: Some(0), .. })
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn encode_key(key: KeyEvent) -> Option<String> {
    let key_name = match key.code {
        KeyCode::Char(character) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => return Some(character.to_string()),
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Enter => "CR".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Backspace => "BS".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "S-Tab".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Delete => "Del".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::F(number) => format!("F{number}"),
        KeyCode::Null | KeyCode::CapsLock | KeyCode::ScrollLock | KeyCode::NumLock | KeyCode::PrintScreen | KeyCode::Pause | KeyCode::Menu | KeyCode::KeypadBegin | KeyCode::Media(_) | KeyCode::Modifier(_) => return None,
    };
    let mut prefix = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) { prefix.push_str("C-"); }
    if key.modifiers.contains(KeyModifiers::ALT) { prefix.push_str("A-"); }
    if key.modifiers.contains(KeyModifiers::SHIFT) && !key_name.starts_with("S-") { prefix.push_str("S-"); }
    Some(format!("<{prefix}{key_name}>"))
}

/// Translate a decoded mouse event into the `nvim_input_mouse` `(button,
/// action, modifier)` tuple. Wheel directions map to the `up`/`down`/`left`/
/// `right` action names the editor expects.
fn encode_mouse(event: MouseEvent) -> (String, String, String) {
    let (button, action) = match event.kind {
        MouseEventKind::Down(button) => (mouse_button_name(button), "press"),
        MouseEventKind::Up(button) => (mouse_button_name(button), "release"),
        MouseEventKind::Drag(button) => (mouse_button_name(button), "drag"),
        MouseEventKind::Moved => ("move", "move"),
        MouseEventKind::ScrollUp => ("wheel", "up"),
        MouseEventKind::ScrollDown => ("wheel", "down"),
        MouseEventKind::ScrollLeft => ("wheel", "left"),
        MouseEventKind::ScrollRight => ("wheel", "right"),
    };
    (button.to_owned(), action.to_owned(), encode_mouse_modifiers(event.modifiers))
}

fn mouse_button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

fn encode_mouse_modifiers(modifiers: KeyModifiers) -> String {
    let mut encoded = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        encoded.push('C');
    }
    if modifiers.contains(KeyModifiers::ALT) {
        encoded.push('A');
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        encoded.push('S');
    }
    if modifiers.contains(KeyModifiers::META) {
        encoded.push('M');
    }
    encoded
}

fn terminal_highlight_color(dict: &Dict, key: &str, support: ColorSupport) -> TerminalColor {
    let Some(Object::Integer(value)) = dict_get(dict, key) else {
        return TerminalColor::Default;
    };
    match support {
        ColorSupport::TrueColor => {
            let Ok(value) = u32::try_from(*value) else {
                return TerminalColor::Default;
            };
            TerminalColor::Rgb(Rgb::new(
                ((value >> 16) & 0xff) as u8,
                ((value >> 8) & 0xff) as u8,
                (value & 0xff) as u8,
            ))
        }
        ColorSupport::Xterm256 | ColorSupport::Ansi16 => u8::try_from(*value)
            .ok()
            .map(|index| TerminalColor::Xterm256(theme::QuantizedColor {
                index,
                rgb: theme::xterm_rgb(index),
            }))
            .unwrap_or(TerminalColor::Default),
        ColorSupport::Mono => TerminalColor::Default,
    }
}

fn highlight_style(dict: &Dict) -> HighlightStyle {
    HighlightStyle {
        foreground: dict_color(dict, "foreground"),
        background: dict_color(dict, "background"),
        special: dict_color(dict, "special"),
        bold: dict_bool(dict, "bold"),
        italic: dict_bool(dict, "italic"),
        underline: dict_bool(dict, "underline"),
        undercurl: dict_bool(dict, "undercurl"),
        reverse: dict_bool(dict, "reverse"),
    }
}

fn dict_get<'a>(dict: &'a Dict, key: &str) -> Option<&'a Object> {
    dict.iter().find(|(candidate, _)| candidate.as_bytes() == key.as_bytes()).map(|(_, value)| value)
}

fn dict_color(dict: &Dict, key: &str) -> Option<Rgb> {
    let Object::Integer(value) = dict_get(dict, key)? else { return None; };
    let value = u32::try_from(*value).ok()?;
    Some(Rgb::new(((value >> 16) & 0xff) as u8, ((value >> 8) & 0xff) as u8, (value & 0xff) as u8))
}

fn dict_bool(dict: &Dict, key: &str) -> bool {
    matches!(dict_get(dict, key), Some(Object::Boolean(true)))
}

fn require_arity(args: &[Object], expected: usize, event: &str) -> Result<(), TuiError> {
    if args.len() == expected { Ok(()) } else { Err(TuiError::Protocol(format!("{event} expected {expected} arguments, got {}", args.len()))) }
}

fn as_i64(args: &[Object], index: usize, event: &str) -> Result<i64, TuiError> {
    match args.get(index) { Some(Object::Integer(value)) => Ok(*value), _ => Err(TuiError::Protocol(format!("{event} argument {index} must be an integer"))) }
}

fn as_usize(args: &[Object], index: usize, event: &str) -> Result<usize, TuiError> {
    usize::try_from(as_i64(args, index, event)?).map_err(|_| TuiError::Protocol(format!("{event} argument {index} must be non-negative")))
}

fn as_u32(args: &[Object], index: usize, event: &str) -> Result<u32, TuiError> {
    u32::try_from(as_i64(args, index, event)?).map_err(|_| TuiError::Protocol(format!("{event} argument {index} must fit u32")))
}

fn as_bool(args: &[Object], index: usize, event: &str) -> Result<bool, TuiError> {
    match args.get(index) { Some(Object::Boolean(value)) => Ok(*value), _ => Err(TuiError::Protocol(format!("{event} argument {index} must be a boolean"))) }
}

fn as_string<'a>(args: &'a [Object], index: usize, event: &str) -> Result<&'a OxStr, TuiError> {
    args.get(index).ok_or_else(|| TuiError::Protocol(format!("{event} missing argument {index}"))).and_then(|value| object_string(value, event))
}

fn object_string<'a>(value: &'a Object, event: &str) -> Result<&'a OxStr, TuiError> {
    match value { Object::String(value) => Ok(value), _ => Err(TuiError::Protocol(format!("{event} must be a string"))) }
}

fn as_chunks(args: &[Object], index: usize, event: &str) -> Result<ChunkLine, TuiError> {
    let value = args.get(index).ok_or_else(|| TuiError::Protocol(format!("{event} missing argument {index}")))?;
    chunks_from_object(value, event)
}

fn chunks_from_object(value: &Object, event: &str) -> Result<ChunkLine, TuiError> {
    let Object::Array(chunks) = value else { return Err(TuiError::Protocol(format!("{event} chunks must be an array"))); };
    chunks.iter().map(|chunk| {
        let Object::Array(fields) = chunk else { return Err(TuiError::Protocol(format!("{event} chunk must be an array"))); };
        if fields.len() < 2 { return Err(TuiError::Protocol(format!("{event} chunk needs attr and text"))); }
        let attr = match &fields[0] { Object::Integer(value) => *value, _ => return Err(TuiError::Protocol(format!("{event} chunk attr must be an integer"))) };
        let text = object_string(&fields[1], event)?;
        Ok(TextChunk::new(attr, text.as_bytes(), attr))
    }).collect()
}

fn as_chunk_lines(args: &[Object], index: usize, event: &str) -> Result<Vec<ChunkLine>, TuiError> {
    let Some(Object::Array(lines)) = args.get(index) else { return Err(TuiError::Protocol(format!("{event} lines must be an array"))); };
    lines.iter().map(|line| chunks_from_object(line, event)).collect()
}

fn as_string_array(args: &[Object], index: usize, event: &str) -> Result<Vec<OxStr>, TuiError> {
    let Some(Object::Array(items)) = args.get(index) else { return Err(TuiError::Protocol(format!("{event} items must be an array"))); };
    items.iter().map(|item| object_string(item, event).cloned()).collect()
}

fn optional_selection(args: &[Object], index: usize, event: &str) -> Result<Option<usize>, TuiError> {
    let value = as_i64(args, index, event)?;
    if value < 0 { Ok(None) } else { usize::try_from(value).map(Some).map_err(|_| TuiError::Protocol(format!("{event} selection is too large"))) }
}

fn as_popup_items(args: &[Object], index: usize, event: &str) -> Result<Vec<PopupItem>, TuiError> {
    let Some(Object::Array(items)) = args.get(index) else { return Err(TuiError::Protocol(format!("{event} items must be an array"))); };
    items.iter().map(|item| {
        let Object::Array(fields) = item else { return Err(TuiError::Protocol(format!("{event} item must be an array"))); };
        if fields.len() < 4 { return Err(TuiError::Protocol(format!("{event} item needs word, kind, menu, and info"))); }
        Ok(PopupItem::new(
            object_string(&fields[0], event)?.as_bytes(),
            object_string(&fields[1], event)?.as_bytes(),
            object_string(&fields[2], event)?.as_bytes(),
            object_string(&fields[3], event)?.as_bytes(),
        ))
    }).collect()
}

fn premix(top: Rgb, bottom: Rgb, blend_percentage: u8) -> Rgb {
    let underlay = u16::from(blend_percentage.min(100));
    let foreground = 100u16.saturating_sub(underlay);
    let channel = |top: u8, bottom: u8| {
        ((u16::from(top) * foreground + u16::from(bottom) * underlay + 50) / 100) as u8
    };
    Rgb::new(
        channel(top.r, bottom.r),
        channel(top.g, bottom.g),
        channel(top.b, bottom.b),
    )
}

fn fallback_color(rgb: Rgb, support: ColorSupport) -> TerminalColor {
    match support {
        ColorSupport::TrueColor => TerminalColor::Rgb(rgb),
        ColorSupport::Xterm256 | ColorSupport::Ansi16 => {
            let cube_component = |component: u8| -> u8 {
                if component < 48 { 0 } else if component < 115 { 1 } else { ((component - 35) / 40).min(5) }
            };
            let red = cube_component(rgb.r);
            let green = cube_component(rgb.g);
            let blue = cube_component(rgb.b);
            let cube_index = 16 + 36 * red + 6 * green + blue;
            let average = (u16::from(rgb.r) + u16::from(rgb.g) + u16::from(rgb.b)) / 3;
            let gray_step = ((average.saturating_sub(8) + 5) / 10).min(23) as u8;
            let gray_index = 232 + gray_step;
            let distance = |candidate: Rgb| {
                let dr = i32::from(candidate.r) - i32::from(rgb.r);
                let dg = i32::from(candidate.g) - i32::from(rgb.g);
                let db = i32::from(candidate.b) - i32::from(rgb.b);
                dr * dr + dg * dg + db * db
            };
            let index = if distance(theme::xterm_rgb(gray_index)) < distance(theme::xterm_rgb(cube_index)) { gray_index } else { cube_index };
            TerminalColor::Xterm256(theme::QuantizedColor { index, rgb: theme::xterm_rgb(index) })
        }
        ColorSupport::Mono => TerminalColor::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_motion_is_instant_and_full_is_bounded() {
        assert_eq!(MotionPolicy::Reduced.notification_opacity(0), 1.0);
        assert_eq!(MotionPolicy::Full.notification_opacity(150), 1.0);
        assert!(MotionPolicy::Full.notification_opacity(0) < MotionPolicy::Full.notification_opacity(75));
    }

    #[test]
    fn frame_preserves_bytes_continuations_and_highlights() {
        let mut state = TuiState::default();
        let rgb = Dict(vec![
            (OxStr::from("foreground"), Object::Integer(0x11_22_33)),
            (OxStr::from("background"), Object::Integer(0x44_55_66)),
            (OxStr::from("bold"), Object::Boolean(true)),
        ]);
        let events = vec![
            RedrawEvent {
                name: OxStr::from("grid_resize"),
                argsets: vec![vec![Object::Integer(1), Object::Integer(2), Object::Integer(1)]],
            },
            RedrawEvent {
                name: OxStr::from("hl_attr_define"),
                argsets: vec![vec![
                    Object::Integer(4),
                    Object::Dict(rgb),
                    Object::Dict(Dict(Vec::new())),
                    Object::Array(Vec::new()),
                ]],
            },
            RedrawEvent {
                name: OxStr::from("grid_line"),
                argsets: vec![vec![
                    Object::Integer(1),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Array(vec![
                        Object::Array(vec![Object::String(OxStr(vec![0xff])), Object::Integer(4)]),
                        Object::Array(vec![Object::String(OxStr(Vec::new()))]),
                    ]),
                    Object::Boolean(false),
                ]],
            },
        ];
        state.apply_redraw(&events, TimeMs(0)).unwrap();
        let grid = state.screen.composed_grid().unwrap();
        let capabilities = TerminalCapabilities {
            colors: ColorSupport::TrueColor,
            kitty_keyboard: false,
            synchronized_output: false,
            undercurl: false,
            colored_underline: false,
            osc52_clipboard: false,
            palette: terminal::PaletteDecision::Direct,
        };
        let frame = render_frame(&grid, &state, capabilities).unwrap();
        assert_eq!(frame.cells()[0].text, vec![0xff]);
        assert_eq!(frame.cells()[0].foreground, TerminalColor::Rgb(Rgb::new(0x11, 0x22, 0x33)));
        assert!(frame.cells()[0].attributes.bold);
        assert!(frame.cells()[1].continuation);
    }

    #[test]
    fn typed_not_implemented_rejects_unknown_events() {
        let mut state = TuiState::default();
        let result = state.apply_redraw(&[RedrawEvent { name: OxStr::from("future_event"), argsets: vec![vec![]] }], TimeMs(0));
        assert!(matches!(result, Err(TuiError::NotImplemented(_))));
    }

    #[test]
    fn initial_redraw_survives_core_negotiation_events() {
        let mut state = TuiState::default();
        let event = |name: &str, args: Vec<Object>| RedrawEvent {
            name: OxStr::from(name),
            argsets: vec![args],
        };
        let batch = vec![
            event("option_set", vec![Object::String(OxStr::from("title")), Object::Boolean(false)]),
            event(
                "default_colors_set",
                vec![
                    Object::Integer(0x11_22_33),
                    Object::Integer(0xee_ee_ee),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(0),
                ],
            ),
            event("mouse_on", vec![]),
            event("mouse_off", vec![]),
            event("busy_start", vec![]),
            event("busy_stop", vec![]),
            event("bell", vec![]),
            event("visual_bell", vec![]),
            event("hl_group_set", vec![Object::Integer(0), Object::Integer(1)]),
            event("update_menu", vec![Object::Integer(0)]),
        ];
        assert!(state.apply_redraw(&batch, TimeMs(0)).is_ok());
    }

    #[test]
    fn mouse_encoding_maps_buttons_actions_and_modifiers() {
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 4,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        };
        assert_eq!(encode_mouse(down), ("left".into(), "press".into(), "CS".into()));
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Right),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(encode_mouse(up), ("right".into(), "release".into(), String::new()));
        let scrolled = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::ALT,
        };
        assert_eq!(encode_mouse(scrolled), ("wheel".into(), "down".into(), "A".into()));
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Middle),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(encode_mouse(drag), ("middle".into(), "drag".into(), String::new()));
        let moved = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(encode_mouse(moved), ("move".into(), "move".into(), String::new()));
    }

    #[test]
    fn surface_rendering_advances_by_cell_width_and_preserves_bytes() {
        let width = 20usize;
        let height = 3usize;
        let mut cells = vec![TerminalCell::default(); width * height];
        let theme = Theme::new(None);
        let rect = Rect { x: 0, y: 0, width, height };
        let mut text: Vec<u8> = Vec::new();
        text.push(b'a');
        text.extend_from_slice("界".as_bytes());
        text.extend_from_slice("\u{301}".as_bytes());
        text.push(0xff);
        paint_surface(
            &mut cells,
            width,
            height,
            rect,
            &text,
            &theme,
            HighlightGroup::NormalFloat,
            ColorSupport::TrueColor,
            1.0,
        );
        let cell_text = |row: usize, column: usize| cells[row * width + column].text.clone();
        assert_eq!(cell_text(1, 1), b"a");
        assert_eq!(cell_text(1, 2), b"\xe7\x95\x8c\xcc\x81");
        // The wide glyph 界 (width 2) marks its trailing column as a
        // continuation so the damage writer skips it, rather than a space.
        let trailing = &cells[width + 3];
        assert!(trailing.continuation);
        assert!(trailing.text.is_empty());
        assert_eq!(cell_text(1, 4), vec![0xff]);
    }

    #[test]
    fn surface_advance_matches_chunk_map_for_multibyte_anchors() {
        // A wildmenu/cmdline anchor must agree with the byte-offset map: a
        // wide char consumes two columns and a combining mark none.
        let width = 20usize;
        let height = 3usize;
        let mut cells = vec![TerminalCell::default(); width * height];
        let theme = Theme::new(None);
        let rect = Rect { x: 0, y: 0, width, height };
        let mut text: Vec<u8> = Vec::new();
        text.extend_from_slice("界".as_bytes());
        text.push(b'x');
        text.extend_from_slice("\u{301}".as_bytes());
        text.push(b'y');
        paint_surface(
            &mut cells,
            width,
            height,
            rect,
            &text,
            &theme,
            HighlightGroup::NormalFloat,
            ColorSupport::TrueColor,
            1.0,
        );
        let cell_text = |row: usize, column: usize| cells[row * width + column].text.clone();
        // 界 at inner col 0 (2 wide), x lands on inner col 2, y lands on col 3.
        assert_eq!(cell_text(1, 1), "界".as_bytes());
        assert_eq!(cell_text(1, 3), b"\x78\xcc\x81");
        assert_eq!(cell_text(1, 4), b"y");
    }

    #[test]
    fn surface_overwrite_clears_stale_continuations() {
        // render_frame turns empty grid cells into continuation cells before
        // paint_surface draws chrome over them. Painting over such a cell must
        // clear the continuation marker, or the damage writer would skip the
        // repaint and leave stale background behind.
        let width = 6usize;
        let height = 3usize;
        let mut cells = vec![TerminalCell::default(); width * height];
        cells[width + 2] = TerminalCell::continuation();
        cells[width + 4] = TerminalCell::continuation();
        let theme = Theme::new(None);
        let rect = Rect { x: 0, y: 0, width, height };
        paint_surface(
            &mut cells,
            width,
            height,
            rect,
            b"ab",
            &theme,
            HighlightGroup::NormalFloat,
            ColorSupport::TrueColor,
            1.0,
        );
        // "ab" paints inner columns 0..2 -> absolute columns 1..2; the former
        // continuation at absolute column 2 is now a real glyph.
        let cell2 = &cells[width + 2];
        assert_eq!(cell2.text, b"b");
        assert!(!cell2.continuation);
        // Absolute column 4 is cleared by the rect wipe (no glyph there): its
        // continuation marker is dropped so it repaints as background space.
        let cell4 = &cells[width + 4];
        assert_eq!(cell4.text, b" ");
        assert!(!cell4.continuation);
    }

    #[test]
    fn mouse_on_off_drives_capture_state() {
        let mut state = TuiState::default();
        let event = |name: &str| RedrawEvent { name: OxStr::from(name), argsets: vec![vec![]] };
        assert!(!state.mouse_capture);
        state.apply_redraw(&[event("mouse_on")], TimeMs(0)).unwrap();
        assert!(state.mouse_capture);
        // A repeated mouse_on keeps capture enabled.
        state.apply_redraw(&[event("mouse_on")], TimeMs(0)).unwrap();
        assert!(state.mouse_capture);
        state.apply_redraw(&[event("mouse_off")], TimeMs(0)).unwrap();
        assert!(!state.mouse_capture);
    }
}
