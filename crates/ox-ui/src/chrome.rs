//! Server-side message, command-line, and popup-menu state.

use ox_types::{Object, OxStr};

use crate::channel::UiEvent;

/// Highlighted text chunk used by messages and command lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentChunk {
    /// Highlight id.
    pub hl_id: u64,
    /// Chunk text.
    pub text: OxStr,
}

impl ContentChunk {
    /// Creates a content chunk.
    #[must_use]
    pub fn new(hl_id: u64, text: impl Into<OxStr>) -> Self { Self { hl_id, text: text.into() } }

    fn to_object(&self) -> Object {
        Object::Array(vec![
            Object::Integer(i64::try_from(self.hl_id).unwrap_or(i64::MAX)),
            Object::String(self.text.clone()),
        ])
    }
}

/// Externally rendered message state.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageState {
    /// Message kind from `ui_events.in.h` (`echo`, `echomsg`, `emsg`, ...).
    pub kind: OxStr,
    /// Highlighted message contents.
    pub content: Vec<ContentChunk>,
    /// Whether the preceding message should be replaced.
    pub replace_last: bool,
    /// Whether the message is retained in message history.
    pub history: bool,
    /// Whether content appends to the preceding message.
    pub append: bool,
    /// Message identity supplied by the producer.
    pub id: Object,
    /// Command or event that triggered the message.
    pub trigger: OxStr,
}

/// Externally rendered command-line state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CmdlineState {
    /// Highlighted command-line contents.
    pub content: Vec<ContentChunk>,
    /// Cursor byte position.
    pub position: usize,
    /// First-character prompt such as `:` or `/`.
    pub first_char: OxStr,
    /// Prompt text.
    pub prompt: OxStr,
    /// Indentation in screen cells.
    pub indent: usize,
    /// Command-line nesting level.
    pub level: usize,
    /// Highlight id for the command-line prompt.
    pub hl_id: u64,
}

/// Popup-menu item in the public four-string layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PopupItem {
    /// Inserted word.
    pub word: OxStr,
    /// Display kind.
    pub kind: OxStr,
    /// Menu annotation.
    pub menu: OxStr,
    /// Extra information.
    pub info: OxStr,
}

impl PopupItem {
    /// Creates a popup-menu item.
    #[must_use]
    pub fn new(
        word: impl Into<OxStr>,
        kind: impl Into<OxStr>,
        menu: impl Into<OxStr>,
        info: impl Into<OxStr>,
    ) -> Self {
        Self { word: word.into(), kind: kind.into(), menu: menu.into(), info: info.into() }
    }

    fn to_object(&self) -> Object {
        Object::Array(vec![
            Object::String(self.word.clone()),
            Object::String(self.kind.clone()),
            Object::String(self.menu.clone()),
            Object::String(self.info.clone()),
        ])
    }
}

/// Externally rendered popup-menu state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PopupmenuState {
    /// Available items.
    pub items: Vec<PopupItem>,
    /// Selected item or `-1`.
    pub selected: i64,
    /// Anchor row.
    pub row: usize,
    /// Anchor column.
    pub col: usize,
    /// Grid containing the anchor.
    pub grid: i64,
}

/// Mode cursor descriptor sent by `mode_info_set`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModeInfo {
    /// Cursor shape name.
    pub cursor_shape: OxStr,
    /// Percentage of the cell filled by the cursor.
    pub cell_percentage: u8,
    /// Optional highlight group name.
    pub attr_id: Option<u64>,
    /// Short mode name.
    pub short_name: OxStr,
    /// Human-readable mode name.
    pub name: OxStr,
}

impl ModeInfo {
    fn to_object(&self) -> Object {
        let mut values = vec![
            (OxStr::from("cursor_shape"), Object::String(self.cursor_shape.clone())),
            (OxStr::from("cell_percentage"), Object::Integer(i64::from(self.cell_percentage))),
            (OxStr::from("short_name"), Object::String(self.short_name.clone())),
            (OxStr::from("name"), Object::String(self.name.clone())),
        ];
        if let Some(attr_id) = self.attr_id {
            values.push((OxStr::from("attr_id"), Object::Integer(i64::try_from(attr_id).unwrap_or(i64::MAX))));
        }
        Object::Dict(ox_types::Dict(values))
    }
}

/// Complete server-owned chrome state plus ordered pending transitions.
#[derive(Clone, Debug)]
pub struct ChromeState {
    /// Last visible message.
    pub message: Option<MessageState>,
    /// Active command line.
    pub cmdline: Option<CmdlineState>,
    /// Active command-line block.
    pub cmdline_block: Vec<Vec<ContentChunk>>,
    /// Active popup menu.
    pub popupmenu: Option<PopupmenuState>,
    /// Current mode name and index.
    pub mode: Option<(OxStr, usize)>,
    /// Cursor styles advertised to the UI.
    pub mode_info: Vec<ModeInfo>,
    /// Whether mode-specific cursor styling is enabled.
    pub mode_info_enabled: bool,
    /// Window title.
    pub title: OxStr,
    /// Window icon label.
    pub icon: OxStr,
    /// Whether the editor is busy.
    pub busy: bool,
    /// Whether mouse reporting is enabled.
    pub mouse: bool,
    pending: Vec<UiEvent>,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            message: None,
            cmdline: None,
            cmdline_block: Vec::new(),
            popupmenu: None,
            mode: None,
            mode_info: Vec::new(),
            mode_info_enabled: false,
            title: OxStr::from(""),
            icon: OxStr::from(""),
            busy: false,
            mouse: false,
            pending: Vec::new(),
        }
    }
}

impl ChromeState {
    /// Creates empty chrome state.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Shows or replaces a message.
    pub fn show_message(&mut self, message: MessageState) {
        self.pending.push(UiEvent::new("msg_show", vec![
            Object::String(message.kind.clone()),
            chunks(&message.content),
            Object::Boolean(message.replace_last),
            Object::Boolean(message.history),
            Object::Boolean(message.append),
            message.id.clone(),
            Object::String(message.trigger.clone()),
        ]));
        self.message = Some(message);
    }

    /// Clears externally rendered messages.
    pub fn clear_message(&mut self) {
        if self.message.take().is_some() { self.pending.push(UiEvent::new("msg_clear", vec![])); }
    }

    /// Shows a command line.
    pub fn show_cmdline(&mut self, state: CmdlineState) {
        self.pending.push(UiEvent::new("cmdline_show", vec![
            chunks(&state.content),
            integer(state.position),
            Object::String(state.first_char.clone()),
            Object::String(state.prompt.clone()),
            integer(state.indent),
            integer(state.level),
            Object::Integer(i64::try_from(state.hl_id).unwrap_or(i64::MAX)),
        ]));
        self.cmdline = Some(state);
    }

    /// Moves the command-line cursor.
    pub fn set_cmdline_position(&mut self, position: usize, level: usize) {
        if let Some(state) = self.cmdline.as_mut() { state.position = position; }
        self.pending.push(UiEvent::new("cmdline_pos", vec![integer(position), integer(level)]));
    }

    /// Hides the active command line.
    pub fn hide_cmdline(&mut self, level: usize, abort: bool) {
        if self.cmdline.take().is_some() {
            self.pending.push(UiEvent::new("cmdline_hide", vec![integer(level), Object::Boolean(abort)]));
        }
    }

    /// Shows a command-line block.
    pub fn show_cmdline_block(&mut self, lines: Vec<Vec<ContentChunk>>) {
        self.pending.push(UiEvent::new(
            "cmdline_block_show",
            vec![Object::Array(lines.iter().map(|line| chunks(line)).collect())],
        ));
        self.cmdline_block = lines;
    }

    /// Appends a line to the command-line block.
    pub fn append_cmdline_block(&mut self, line: Vec<ContentChunk>) {
        self.pending.push(UiEvent::new(
            "cmdline_block_append",
            vec![Object::Array(vec![chunks(&line)])],
        ));
        self.cmdline_block.push(line);
    }

    /// Hides the command-line block.
    pub fn hide_cmdline_block(&mut self) {
        if !self.cmdline_block.is_empty() {
            self.cmdline_block.clear();
            self.pending.push(UiEvent::new("cmdline_block_hide", vec![]));
        }
    }

    /// Shows a popup menu.
    pub fn show_popupmenu(&mut self, state: PopupmenuState) {
        self.pending.push(UiEvent::new("popupmenu_show", vec![
            Object::Array(state.items.iter().map(PopupItem::to_object).collect()),
            Object::Integer(state.selected),
            integer(state.row),
            integer(state.col),
            Object::Integer(state.grid),
        ]));
        self.popupmenu = Some(state);
    }

    /// Changes the selected popup-menu item.
    pub fn select_popupmenu(&mut self, selected: i64) {
        if let Some(state) = self.popupmenu.as_mut() { state.selected = selected; }
        self.pending.push(UiEvent::new("popupmenu_select", vec![Object::Integer(selected)]));
    }

    /// Hides the popup menu.
    pub fn hide_popupmenu(&mut self) {
        if self.popupmenu.take().is_some() { self.pending.push(UiEvent::new("popupmenu_hide", vec![])); }
    }

    /// Advertises cursor styles when they change.
    pub fn set_mode_info(&mut self, enabled: bool, modes: Vec<ModeInfo>) {
        if self.mode_info_enabled == enabled && self.mode_info == modes { return; }
        self.pending.push(UiEvent::new("mode_info_set", vec![
            Object::Boolean(enabled),
            Object::Array(modes.iter().map(ModeInfo::to_object).collect()),
        ]));
        self.mode_info_enabled = enabled;
        self.mode_info = modes;
    }

    /// Changes the active mode, emitting only on transition.
    pub fn set_mode(&mut self, name: impl Into<OxStr>, index: usize) {
        let name = name.into();
        if self.mode.as_ref() == Some(&(name.clone(), index)) { return; }
        self.pending.push(UiEvent::new("mode_change", vec![Object::String(name.clone()), integer(index)]));
        self.mode = Some((name, index));
    }

    /// Sets title and emits only on change.
    pub fn set_title(&mut self, title: impl Into<OxStr>) {
        let title = title.into();
        if self.title == title { return; }
        self.title = title.clone();
        self.pending.push(UiEvent::new("set_title", vec![Object::String(title)]));
    }

    /// Sets icon label and emits only on change.
    pub fn set_icon(&mut self, icon: impl Into<OxStr>) {
        let icon = icon.into();
        if self.icon == icon { return; }
        self.icon = icon.clone();
        self.pending.push(UiEvent::new("set_icon", vec![Object::String(icon)]));
    }

    /// Starts or ends busy state.
    pub fn set_busy(&mut self, busy: bool) {
        if self.busy == busy { return; }
        self.busy = busy;
        self.pending.push(UiEvent::new(if busy { "busy_start" } else { "busy_stop" }, vec![]));
    }

    /// Enables or disables mouse reporting.
    pub fn set_mouse(&mut self, mouse: bool) {
        if self.mouse == mouse { return; }
        self.mouse = mouse;
        self.pending.push(UiEvent::new(if mouse { "mouse_on" } else { "mouse_off" }, vec![]));
    }

    /// Emits a bell without persistent state.
    pub fn bell(&mut self, visual: bool) {
        self.pending.push(UiEvent::new(if visual { "visual_bell" } else { "bell" }, vec![]));
    }

    /// Drains ordered pending state transitions.
    pub fn take_events(&mut self) -> Vec<UiEvent> { std::mem::take(&mut self.pending) }

    /// Builds the current state events needed to initialize a newly attached UI.
    #[must_use]
    pub fn snapshot_events(&self) -> Vec<UiEvent> {
        let mut events = Vec::new();
        if !self.mode_info.is_empty() || self.mode_info_enabled {
            events.push(UiEvent::new("mode_info_set", vec![
                Object::Boolean(self.mode_info_enabled),
                Object::Array(self.mode_info.iter().map(ModeInfo::to_object).collect()),
            ]));
        }
        if let Some((name, index)) = &self.mode {
            events.push(UiEvent::new("mode_change", vec![Object::String(name.clone()), integer(*index)]));
        }
        if !self.title.as_bytes().is_empty() {
            events.push(UiEvent::new("set_title", vec![Object::String(self.title.clone())]));
        }
        if !self.icon.as_bytes().is_empty() {
            events.push(UiEvent::new("set_icon", vec![Object::String(self.icon.clone())]));
        }
        if self.busy { events.push(UiEvent::new("busy_start", vec![])); }
        if self.mouse { events.push(UiEvent::new("mouse_on", vec![])); }
        if let Some(message) = &self.message {
            events.push(UiEvent::new("msg_show", vec![
                Object::String(message.kind.clone()),
                chunks(&message.content),
                Object::Boolean(message.replace_last),
                Object::Boolean(message.history),
                Object::Boolean(message.append),
                message.id.clone(),
                Object::String(message.trigger.clone()),
            ]));
        }
        if let Some(state) = &self.cmdline {
            events.push(UiEvent::new("cmdline_show", vec![
                chunks(&state.content), integer(state.position), Object::String(state.first_char.clone()),
                Object::String(state.prompt.clone()), integer(state.indent), integer(state.level),
                Object::Integer(i64::try_from(state.hl_id).unwrap_or(i64::MAX)),
            ]));
        }
        if !self.cmdline_block.is_empty() {
            events.push(UiEvent::new("cmdline_block_show", vec![Object::Array(
                self.cmdline_block.iter().map(|line| chunks(line)).collect(),
            )]));
        }
        if let Some(state) = &self.popupmenu {
            events.push(UiEvent::new("popupmenu_show", vec![
                Object::Array(state.items.iter().map(PopupItem::to_object).collect()),
                Object::Integer(state.selected), integer(state.row), integer(state.col), Object::Integer(state.grid),
            ]));
        }
        events
    }
}

fn chunks(content: &[ContentChunk]) -> Object {
    Object::Array(content.iter().map(ContentChunk::to_object).collect())
}

fn integer(value: usize) -> Object {
    Object::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}
