//! Client-owned TUI chrome state.
//!
//! This module does not know about RPC transport or server grids.  It accepts
//! already-decoded UI event data, preserves server text verbatim, and exposes
//! deterministic state and layout descriptions to a renderer.
#![allow(missing_docs)]

use std::collections::BTreeMap;

use ox_types::{Object, OxStr};
use thiserror::Error;

/// Ephemeral messages remain visible for four seconds after a stable batch.
pub const EPHEMERAL_LIFETIME_MS: u64 = 4_000;
/// At most five ordinary messages are shown outside the history float.
pub const MAX_VISIBLE_MESSAGES: usize = 5;
/// The narrowest a client float may be and still show text: a one-cell frame
/// on each side plus six columns of content.
///
/// Only the popup-menu documentation preview is sized from leftover space, so
/// it is the only surface that can fall below this. It is dropped rather than
/// clipped: at 20 columns a full-width completion menu leaves nothing beside
/// it, and a two-column strip of border would be worse than no preview.
pub const MIN_FLOAT_COLUMNS: usize = 8;

/// A deterministic monotonic timestamp supplied by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeMs(pub u64);

impl TimeMs {
    #[must_use]
    pub fn after(self, duration_ms: u64) -> Self {
        Self(self.0.saturating_add(duration_ms))
    }
}

/// A highlighted text chunk from an externalized UI event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChunk {
    pub attr_id: i64,
    pub text: OxStr,
    pub hl_id: i64,
}

impl TextChunk {
    #[must_use]
    pub fn new(attr_id: i64, text: impl AsRef<[u8]>, hl_id: i64) -> Self {
        Self { attr_id, text: OxStr(text.as_ref().to_vec()), hl_id }
    }
}

/// One protocol line. Chunks are never joined or normalized in storage.
pub type ChunkLine = Vec<TextChunk>;

/// A byte offset and its rendered terminal column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteColumn {
    pub byte: usize,
    pub column: usize,
}

/// Maps every byte offset in a chunk sequence to a rendered column.
///
/// Offsets inside a UTF-8 code point map to the column at that code point's
/// start. Tabs advance to an eight-column stop and common wide code points use
/// two cells. The final byte offset maps to the column after the final glyph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedChunkMap {
    columns_by_byte: Vec<usize>,
    pub boundaries: Vec<ByteColumn>,
    pub byte_len: usize,
    pub rendered_columns: usize,
}

impl RenderedChunkMap {
    #[must_use]
    pub fn from_chunks(chunks: &[TextChunk]) -> Self {
        let byte_len: usize = chunks.iter().map(|chunk| chunk.text.as_bytes().len()).sum();
        let mut columns_by_byte = vec![0; byte_len.saturating_add(1)];
        let mut boundaries = Vec::new();
        let mut byte = 0usize;
        let mut column = 0usize;
        boundaries.push(ByteColumn { byte, column });

        for chunk in chunks {
            let bytes = chunk.text.as_bytes();
            let mut local = 0usize;
            while local < bytes.len() {
                let (consumed, width) = decoded_cell_width(bytes, local, column);
                let end = byte.saturating_add(consumed);
                if let Some(slice) = columns_by_byte.get_mut(byte..end) {
                    slice.fill(column);
                }
                local = local.saturating_add(consumed);
                byte = end;
                column = column.saturating_add(width);
                if let Some(slot) = columns_by_byte.get_mut(byte) {
                    *slot = column;
                }
                boundaries.push(ByteColumn { byte, column });
            }
        }

        Self { columns_by_byte, boundaries, byte_len, rendered_columns: column }
    }

    /// Convert a protocol byte position to a rendered column.
    ///
    /// Positions beyond the text clamp to its final column.
    #[must_use]
    pub fn column_for_byte(&self, byte: usize) -> usize {
        match self.columns_by_byte.get(byte) {
            Some(column) => *column,
            None => self.rendered_columns,
        }
    }
}

pub(crate) fn decoded_cell_width(bytes: &[u8], start: usize, column: usize) -> (usize, usize) {
    let first = match bytes.get(start) {
        Some(value) => *value,
        None => 0,
    };
    let expected = match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    };
    if expected == 0 || start.saturating_add(expected) > bytes.len() {
        return (1, 1);
    }
    let end = start.saturating_add(expected);
    let character = std::str::from_utf8(&bytes[start..end]).ok().and_then(|text| text.chars().next());
    match character {
        Some(character) => (expected, rendered_char_width(character, column)),
        None => (1, 1),
    }
}

fn rendered_char_width(character: char, column: usize) -> usize {
    match character {
        '\t' => 8usize.saturating_sub(column % 8),
        '\n' | '\r' | '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' => 0,
        c if is_combining(c) => 0,
        c if is_wide(c) => 2,
        _ => 1,
    }
}

// Terminal cell-width ranges used by Vim-compatible UIs. Combining marks are
// handled separately; ambiguous-width code points deliberately remain narrow.
fn is_wide(character: char) -> bool {
    matches!(
        character as u32,
        0x1100..=0x115f
            | 0x231a..=0x231b
            | 0x2329..=0x232a
            | 0x23e9..=0x23ec
            | 0x23f0
            | 0x23f3
            | 0x25fd..=0x25fe
            | 0x2614..=0x2615
            | 0x2648..=0x2653
            | 0x267f
            | 0x2693
            | 0x26a1
            | 0x26aa..=0x26ab
            | 0x26bd..=0x26be
            | 0x26c4..=0x26c5
            | 0x26ce
            | 0x26d4
            | 0x26ea
            | 0x26f2..=0x26f3
            | 0x26f5
            | 0x26fa
            | 0x26fd
            | 0x2705
            | 0x270a..=0x270b
            | 0x2728
            | 0x274c
            | 0x274e
            | 0x2753..=0x2755
            | 0x2757
            | 0x2795..=0x2797
            | 0x27b0
            | 0x27bf
            | 0x2b1b..=0x2b1c
            | 0x2b50
            | 0x2b55
            | 0x2e80..=0x303e
            | 0x3040..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f004
            | 0x1f0cf
            | 0x1f18e
            | 0x1f191..=0x1f19a
            | 0x1f200..=0x1f202
            | 0x1f210..=0x1f23b
            | 0x1f240..=0x1f248
            | 0x1f250..=0x1f251
            | 0x1f300..=0x1f64f
            | 0x1f680..=0x1f6ff
            | 0x1f900..=0x1f9ff
            | 0x20000..=0x3fffd
    )
}

fn is_combining(character: char) -> bool {
    matches!(
        character as u32,
        0x0300..=0x036f
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x05bf
            | 0x05c1..=0x05c2
            | 0x05c4..=0x05c5
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670
            | 0x06d6..=0x06ed
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
    )
}

/// A pending command-line special character such as the Ctrl-V marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdlineSpecial {
    pub text: OxStr,
    pub shift: bool,
}

/// One recursion level in the externalized command-line stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdlineLevel {
    pub level: u32,
    pub content: ChunkLine,
    pub cursor_byte: usize,
    pub first_character: OxStr,
    pub prompt: Option<OxStr>,
    pub indent: usize,
    pub prompt_hl_id: i64,
    pub special: Option<CmdlineSpecial>,
    pub block: Vec<ChunkLine>,
    pub rendered_map: RenderedChunkMap,
}

impl CmdlineLevel {
    #[must_use]
    pub fn cursor_column(&self) -> usize {
        self.indent
            .saturating_add(self.rendered_map.column_for_byte(self.cursor_byte))
    }
}

/// The most recent command-line hide event, including escape/abort state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmdlineHide {
    pub level: u32,
    pub aborted: bool,
}

/// Level-indexed command-line state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CmdlineState {
    levels: BTreeMap<u32, CmdlineLevel>,
    pub last_hide: Option<CmdlineHide>,
}

impl CmdlineState {
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        level: u32,
        content: ChunkLine,
        cursor_byte: usize,
        first_character: impl AsRef<[u8]>,
        prompt: Option<OxStr>,
        indent: usize,
        prompt_hl_id: i64,
    ) {
        let rendered_map = RenderedChunkMap::from_chunks(&content);
        let previous_block = match self.levels.get(&level) {
            Some(entry) => entry.block.clone(),
            None => Vec::new(),
        };
        self.levels.retain(|existing, _| *existing <= level);
        self.levels.insert(
            level,
            CmdlineLevel {
                level,
                content,
                cursor_byte,
                first_character: OxStr(first_character.as_ref().to_vec()),
                prompt,
                indent,
                prompt_hl_id,
                special: None,
                block: previous_block,
                rendered_map,
            },
        );
    }

    pub fn set_cursor(&mut self, level: u32, cursor_byte: usize) -> bool {
        if let Some(entry) = self.levels.get_mut(&level) {
            entry.cursor_byte = cursor_byte;
            true
        } else {
            false
        }
    }

    pub fn set_special(&mut self, level: u32, text: impl AsRef<[u8]>, shift: bool) -> bool {
        if let Some(entry) = self.levels.get_mut(&level) {
            entry.special = Some(CmdlineSpecial { text: OxStr(text.as_ref().to_vec()), shift });
            true
        } else {
            false
        }
    }

    /// Hide `level`; the next lower recursion level becomes active.
    pub fn hide(&mut self, level: u32, aborted: bool) -> bool {
        self.last_hide = Some(CmdlineHide { level, aborted });
        self.levels.remove(&level).is_some()
    }

    pub fn show_block(&mut self, lines: Vec<ChunkLine>) -> bool {
        if let Some(entry) = self.active_mut() {
            entry.block = lines;
            true
        } else {
            false
        }
    }

    pub fn append_block(&mut self, line: ChunkLine) -> bool {
        if let Some(entry) = self.active_mut() {
            entry.block.push(line);
            true
        } else {
            false
        }
    }

    pub fn hide_block(&mut self) -> bool {
        if let Some(entry) = self.active_mut() {
            entry.block.clear();
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn active(&self) -> Option<&CmdlineLevel> {
        self.levels.last_key_value().map(|(_, value)| value)
    }

    fn active_mut(&mut self) -> Option<&mut CmdlineLevel> {
        self.levels.last_key_value().map(|(key, _)| *key).and_then(|key| self.levels.get_mut(&key))
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.levels.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.levels.len()
    }
}

/// Completion item used by both wildmenu and insert completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupItem {
    pub word: OxStr,
    pub kind: OxStr,
    pub menu: OxStr,
    pub info: OxStr,
}

impl PopupItem {
    #[must_use]
    pub fn new(
        word: impl AsRef<[u8]>,
        kind: impl AsRef<[u8]>,
        menu: impl AsRef<[u8]>,
        info: impl AsRef<[u8]>,
    ) -> Self {
        Self {
            word: OxStr(word.as_ref().to_vec()),
            kind: OxStr(kind.as_ref().to_vec()),
            menu: OxStr(menu.as_ref().to_vec()),
            info: OxStr(info.as_ref().to_vec()),
        }
    }
}

/// Horizontal command-line completion state (`popupmenu_show`, grid = -1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WildmenuState {
    pub items: Vec<PopupItem>,
    pub selected: Option<usize>,
    pub anchor_byte: usize,
    pub anchor_column: usize,
}

/// A grid-relative insert completion anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridAnchor {
    pub grid: i64,
    pub row: usize,
    pub column: usize,
}

/// Vertical insert completion and its selected documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertPopupState {
    pub items: Vec<PopupItem>,
    pub selected: Option<usize>,
    pub anchor: GridAnchor,
}

impl InsertPopupState {
    #[must_use]
    pub fn documentation(&self) -> Option<&[u8]> {
        self.selected
            .and_then(|selected| self.items.get(selected))
            .map(|item| item.info.as_bytes())
            .filter(|info| !info.is_empty())
    }
}

/// Stable protocol message identifiers supported by upstream.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageId {
    Integer(i64),
    String(Vec<u8>),
}

impl MessageId {
    pub fn from_object(value: Object) -> Result<Option<Self>, ChromeError> {
        match value {
            Object::Nil => Ok(None),
            Object::Integer(value) => Ok(Some(Self::Integer(value))),
            Object::String(value) => Ok(Some(Self::String(value.0))),
            other => Err(ChromeError::InvalidMessageId(other)),
        }
    }
}

/// A message's replacement key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageKey {
    pub kind: OxStr,
    pub id: Option<MessageId>,
}

/// Lifecycle category derived from kind and event context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageClass {
    Sticky,
    Streaming,
    Progress,
    SearchCount,
    Ephemeral,
}

/// Diagnostic weight of a message.
///
/// The design system forbids color as the sole channel, so a diagnostic also
/// carries a letter. The letter is client-owned chrome painted beside the
/// message, never a rewrite of the server's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageSeverity {
    /// An error message kind (`emsg`, `echoerr`, `lua_error`, `rpc_error`,
    /// `shell_err`).
    Error,
    /// A warning message kind (`wmsg`).
    Warning,
    /// Every other kind, including kinds this client does not recognize.
    Plain,
}

impl MessageSeverity {
    /// Classify a `msg_show` kind.
    ///
    /// Unknown kinds are [`Self::Plain`]: upstream requires clients to treat a
    /// kind they do not know as the empty kind
    /// (`runtime/doc/api-ui-events.txt`, `msg_show`).
    #[must_use]
    pub fn from_kind(kind: &OxStr) -> Self {
        match kind.as_bytes() {
            b"emsg" | b"echoerr" | b"lua_error" | b"rpc_error" | b"shell_err" => Self::Error,
            b"wmsg" => Self::Warning,
            _ => Self::Plain,
        }
    }

    /// The non-color letter for a diagnostic, or `None` for plain output.
    #[must_use]
    pub const fn letter(self) -> Option<u8> {
        match self {
            Self::Error => Some(b'E'),
            Self::Warning => Some(b'W'),
            Self::Plain => None,
        }
    }
}

/// Current lifetime state of a visible message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageLifetime {
    StickyUntilKeypress,
    StreamingOpen,
    PendingBatch { generation: u64 },
    Expiring { deadline: TimeMs, generation: u64 },
}

/// A visible message entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageEntry {
    pub key: MessageKey,
    pub content: ChunkLine,
    pub history: bool,
    pub lifetime: MessageLifetime,
    pub sequence: u64,
}

impl MessageEntry {
    /// The diagnostic weight this entry's kind carries.
    #[must_use]
    pub fn severity(&self) -> MessageSeverity {
        MessageSeverity::from_kind(&self.key.kind)
    }
}

/// Input for `msg_show` after RPC decoding.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageUpdate {
    pub kind: OxStr,
    pub content: ChunkLine,
    pub replace_last: bool,
    pub history: bool,
    pub append: bool,
    pub id: Object,
    pub prompt: bool,
}

/// A complete history entry from `msg_history_show`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub kind: OxStr,
    pub content: ChunkLine,
    pub append: bool,
}

/// Paged history float state. `entries` always contains the complete record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryFloat {
    pub entries: Vec<HistoryEntry>,
    pub previous_command: bool,
    pub page: usize,
}

impl HistoryFloat {
    pub fn next_page(&mut self, page_size: usize) {
        if page_size == 0 {
            return;
        }
        let last_page = self.entries.len().saturating_sub(1) / page_size;
        self.page = self.page.saturating_add(1).min(last_page);
    }

    pub fn previous_page(&mut self) {
        self.page = self.page.saturating_sub(1);
    }

    #[must_use]
    pub fn page_entries(&self, page_size: usize) -> &[HistoryEntry] {
        if page_size == 0 {
            return &self.entries[0..0];
        }
        let start = self.page.saturating_mul(page_size).min(self.entries.len());
        let end = start.saturating_add(page_size).min(self.entries.len());
        &self.entries[start..end]
    }
}

/// Ordinary message visibility plus an overflow cue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleMessages<'a> {
    pub entries: Vec<&'a MessageEntry>,
    pub hidden_count: usize,
    pub overflow_badge: Option<String>,
}

/// Client chrome state, independent of transport and rendering.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chrome {
    pub cmdline: CmdlineState,
    pub wildmenu: Option<WildmenuState>,
    pub sticky_wildlist: Option<ChunkLine>,
    pub insert_popup: Option<InsertPopupState>,
    pub messages: Vec<MessageEntry>,
    pub search_count: Option<ChunkLine>,
    pub history: Vec<HistoryEntry>,
    pub history_float: Option<HistoryFloat>,
    sequence: u64,
    generation: u64,
}

impl Chrome {
    #[allow(clippy::too_many_arguments)]
    pub fn cmdline_show(
        &mut self,
        level: u32,
        content: ChunkLine,
        cursor_byte: usize,
        first_character: impl AsRef<[u8]>,
        prompt: Option<OxStr>,
        indent: usize,
        prompt_hl_id: i64,
    ) {
        self.cmdline.show(
            level,
            content,
            cursor_byte,
            first_character,
            prompt,
            indent,
            prompt_hl_id,
        );
    }

    pub fn cmdline_hide(&mut self, level: u32, aborted: bool) -> bool {
        let hidden = self.cmdline.hide(level, aborted);
        self.wildmenu = None;
        self.sticky_wildlist = None;
        hidden
    }

    pub fn cmdline_pos(&mut self, level: u32, cursor_byte: usize) -> bool {
        self.cmdline.set_cursor(level, cursor_byte)
    }

    pub fn cmdline_special_char(
        &mut self,
        level: u32,
        text: impl AsRef<[u8]>,
        shift: bool,
    ) -> bool {
        self.cmdline.set_special(level, text, shift)
    }

    pub fn cmdline_block_show(&mut self, lines: Vec<ChunkLine>) -> bool {
        self.cmdline.show_block(lines)
    }

    pub fn cmdline_block_append(&mut self, line: ChunkLine) -> bool {
        self.cmdline.append_block(line)
    }

    pub fn cmdline_block_hide(&mut self) -> bool {
        self.cmdline.hide_block()
    }

    pub fn popupmenu_show(
        &mut self,
        items: Vec<PopupItem>,
        selected: Option<usize>,
        row: usize,
        column: usize,
        grid: i64,
    ) {
        if grid == -1 {
            let anchor_column = self
                .cmdline
                .active()
                .map_or(0, |cmdline| cmdline.rendered_map.column_for_byte(column));
            self.wildmenu = Some(WildmenuState {
                items,
                selected: valid_selection(selected, None),
                anchor_byte: column,
                anchor_column,
            });
            self.insert_popup = None;
        } else {
            let len = items.len();
            self.insert_popup = Some(InsertPopupState {
                items,
                selected: valid_selection(selected, Some(len)),
                anchor: GridAnchor { grid, row, column },
            });
            self.wildmenu = None;
        }
    }

    pub fn popupmenu_select(&mut self, selected: Option<usize>) {
        if let Some(wildmenu) = self.wildmenu.as_mut() {
            wildmenu.selected = valid_selection(selected, Some(wildmenu.items.len()));
        }
        if let Some(popup) = self.insert_popup.as_mut() {
            popup.selected = valid_selection(selected, Some(popup.items.len()));
        }
    }

    pub fn popupmenu_hide(&mut self) {
        self.wildmenu = None;
        self.insert_popup = None;
    }

    pub fn message_show(&mut self, update: MessageUpdate) -> Result<(), ChromeError> {
        let id = MessageId::from_object(update.id)?;
        let class = classify_message(&update.kind, id.is_some());

        if class == MessageClass::SearchCount {
            self.search_count = Some(update.content);
            return Ok(());
        }
        if update.kind.as_bytes() == b"wildlist" {
            self.sticky_wildlist = Some(update.content.clone());
            if update.history {
                self.history.push(HistoryEntry {
                    kind: update.kind,
                    content: update.content,
                    append: update.append,
                });
            }
            return Ok(());
        }

        self.sequence = self.sequence.saturating_add(1);
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        let key = MessageKey { kind: update.kind.clone(), id };
        let force_sticky = update.prompt
            || self.cmdline.is_active()
            || is_sticky_kind(&update.kind);
        let lifetime = lifetime_for(class, generation, force_sticky);

        let target = if class == MessageClass::Progress {
            key.id.as_ref().and_then(|id| {
                self.messages.iter().position(|entry| {
                    entry.key.kind == key.kind && entry.key.id.as_ref() == Some(id)
                })
            })
        } else if update.append || update.replace_last {
            self.messages.len().checked_sub(1)
        } else {
            self.messages.iter().position(|entry| entry.key == key)
        };

        if let Some(index) = target {
            if let Some(entry) = self.messages.get_mut(index) {
                if update.append {
                    entry.content.extend(update.content.clone());
                } else {
                    entry.content = update.content.clone();
                }
                entry.key = key;
                entry.history |= update.history;
                entry.lifetime = lifetime;
                entry.sequence = self.sequence;
            }
        } else {
            self.messages.push(MessageEntry {
                key,
                content: update.content.clone(),
                history: update.history,
                lifetime,
                sequence: self.sequence,
            });
        }

        if update.history && update.kind.as_bytes() != b"search_count" {
            self.history.push(HistoryEntry {
                kind: update.kind,
                content: update.content,
                append: update.append,
            });
        }
        Ok(())
    }

    /// Mark a redraw batch stable and start/restart all pending ephemeral timers.
    pub fn finish_batch(&mut self, now: TimeMs) {
        for entry in &mut self.messages {
            if entry.lifetime == MessageLifetime::StreamingOpen
                && entry.key.kind.as_bytes() == b"shell_ret"
            {
                self.generation = self.generation.saturating_add(1);
                entry.lifetime = MessageLifetime::Expiring {
                    deadline: now.after(EPHEMERAL_LIFETIME_MS),
                    generation: self.generation,
                };
                continue;
            }
            if let MessageLifetime::PendingBatch { generation } = entry.lifetime {
                entry.lifetime = MessageLifetime::Expiring {
                    deadline: now.after(EPHEMERAL_LIFETIME_MS),
                    generation,
                };
            }
        }
    }

    /// Close one streaming entry and start its four-second expiry.
    pub fn close_stream(&mut self, key: &MessageKey, now: TimeMs) -> bool {
        if let Some(entry) = self.messages.iter_mut().find(|entry| &entry.key == key) {
            if entry.lifetime == MessageLifetime::StreamingOpen {
                self.generation = self.generation.saturating_add(1);
                entry.lifetime = MessageLifetime::Expiring {
                    deadline: now.after(EPHEMERAL_LIFETIME_MS),
                    generation: self.generation,
                };
                return true;
            }
        }
        false
    }

    /// Remove expired ephemerals. Equality with the deadline counts as expired.
    pub fn advance_time(&mut self, now: TimeMs) {
        self.messages.retain(|entry| match entry.lifetime {
            MessageLifetime::Expiring { deadline, .. } => deadline > now,
            _ => true,
        });
    }

    /// Sticky messages dismiss on a keypress; open streams remain visible.
    pub fn keypress(&mut self) {
        self.messages.retain(|entry| entry.lifetime != MessageLifetime::StickyUntilKeypress);
    }

    /// Honor `msg_clear` without violating sticky and streaming lifetimes.
    pub fn message_clear(&mut self) {
        self.messages.retain(|entry| {
            matches!(
                entry.lifetime,
                MessageLifetime::StickyUntilKeypress | MessageLifetime::StreamingOpen
            )
        });
    }

    pub fn history_show(&mut self, entries: Vec<HistoryEntry>, previous_command: bool) {
        self.history = entries.clone();
        self.history_float = Some(HistoryFloat { entries, previous_command, page: 0 });
    }

    pub fn history_hide(&mut self) {
        self.history_float = None;
    }

    pub fn history_clear(&mut self) {
        self.history.clear();
        self.history_float = None;
    }

    #[must_use]
    pub fn visible_messages(&self) -> VisibleMessages<'_> {
        let start = self.messages.len().saturating_sub(MAX_VISIBLE_MESSAGES);
        let hidden_count = start;
        VisibleMessages {
            entries: self.messages[start..].iter().collect(),
            hidden_count,
            overflow_badge: (hidden_count > 0).then(|| format!("+{hidden_count} more")),
        }
    }

    /// Describe client-surface rectangles for the current terminal dimensions.
    #[must_use]
    pub fn layout(&self, columns: usize, rows: usize, cursor_row: Option<usize>) -> ChromeLayout {
        let cmdline = self.cmdline.active().map(|active| cmdline_layout(active, columns, rows, cursor_row));
        let wildlist = cmdline.and_then(|rect| {
            self.sticky_wildlist.as_ref().map(|_| rect.above(1))
        });
        let wildmenu = cmdline.and_then(|rect| {
            self.wildmenu.as_ref().map(|_| rect.below(1, rows))
        });
        let messages = (!self.messages.is_empty() || self.search_count.is_some()).then(|| {
            message_layout(columns, rows, self.visible_messages().entries.len(), self.search_count.is_some())
        });
        let search_count = messages.and_then(|rect| {
            self.search_count.as_ref().map(|_| Rect {
                x: rect.x,
                y: rect.y.saturating_add(rect.height).saturating_sub(1),
                width: rect.width,
                height: usize::from(rect.height > 0),
            })
        });
        let (insert_popup, documentation) = popup_layout(self.insert_popup.as_ref(), columns, rows);
        let history = self.history_float.as_ref().map(|_| history_layout(columns, rows));

        ChromeLayout {
            cmdline,
            wildlist,
            wildmenu,
            messages,
            search_count,
            insert_popup,
            documentation,
            history,
        }
    }
}

fn valid_selection(selected: Option<usize>, len: Option<usize>) -> Option<usize> {
    match (selected, len) {
        (Some(index), Some(len)) if index < len => Some(index),
        (Some(index), None) => Some(index),
        _ => None,
    }
}

fn classify_message(kind: &OxStr, has_id: bool) -> MessageClass {
    if kind.as_bytes() == b"search_count" {
        return MessageClass::SearchCount;
    }
    if kind.as_bytes() == b"progress" && has_id {
        return MessageClass::Progress;
    }
    if is_streaming_kind(kind) {
        return MessageClass::Streaming;
    }
    if is_sticky_kind(kind) {
        return MessageClass::Sticky;
    }
    MessageClass::Ephemeral
}

fn is_sticky_kind(kind: &OxStr) -> bool {
    matches!(kind.as_bytes(), b"emsg" | b"echoerr" | b"lua_error" | b"rpc_error" | b"wmsg" | b"confirm")
}

fn is_streaming_kind(kind: &OxStr) -> bool {
    matches!(kind.as_bytes(), b"shell_out" | b"shell_err" | b"shell_cmd" | b"shell_ret")
}

fn lifetime_for(class: MessageClass, generation: u64, force_sticky: bool) -> MessageLifetime {
    if force_sticky {
        return MessageLifetime::StickyUntilKeypress;
    }
    match class {
        MessageClass::Sticky => MessageLifetime::StickyUntilKeypress,
        MessageClass::Streaming => MessageLifetime::StreamingOpen,
        MessageClass::Progress | MessageClass::Ephemeral => MessageLifetime::PendingBatch { generation },
        MessageClass::SearchCount => MessageLifetime::PendingBatch { generation },
    }
}

/// A saturated terminal rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    fn above(self, height: usize) -> Self {
        let actual = height.min(self.y);
        Self { x: self.x, y: self.y.saturating_sub(actual), width: self.width, height: actual }
    }

    fn below(self, height: usize, rows: usize) -> Self {
        let y = self.y.saturating_add(self.height).min(rows);
        Self { x: self.x, y, width: self.width, height: height.min(rows.saturating_sub(y)) }
    }

    #[must_use]
    pub fn contains_row(self, row: usize) -> bool {
        row >= self.y && row < self.y.saturating_add(self.height)
    }

    #[must_use]
    pub fn contains(self, column: usize, row: usize) -> bool {
        column >= self.x
            && column < self.x.saturating_add(self.width)
            && self.contains_row(row)
    }
}

/// Responsive layout descriptions for every client-owned surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChromeLayout {
    pub cmdline: Option<Rect>,
    pub wildlist: Option<Rect>,
    pub wildmenu: Option<Rect>,
    pub messages: Option<Rect>,
    pub search_count: Option<Rect>,
    pub insert_popup: Option<Rect>,
    pub documentation: Option<Rect>,
    pub history: Option<Rect>,
}

impl ChromeLayout {
    /// Whether a terminal-space `(column, row)` falls inside any client-owned
    /// chrome surface. Mouse events inside these rectangles are suppressed
    /// rather than forwarded to the server as grid events.
    #[must_use]
    pub fn contains(self, column: usize, row: usize) -> bool {
        [
            self.cmdline,
            self.wildlist,
            self.wildmenu,
            self.messages,
            self.search_count,
            self.insert_popup,
            self.documentation,
            self.history,
        ]
        .into_iter()
        .flatten()
        .any(|rect| rect.contains(column, row))
    }
}

fn cmdline_layout(
    active: &CmdlineLevel,
    columns: usize,
    rows: usize,
    cursor_row: Option<usize>,
) -> Rect {
    let padding = usize::from(columns >= 60);
    let border = 2usize.min(rows);
    let desired_height = 1usize
        .saturating_add(active.block.len())
        .saturating_add(padding.saturating_mul(2))
        .saturating_add(border);
    let height = desired_height.min(rows);
    let width = if columns < 60 { columns } else { columns.min(72).max(1) };
    let x = if columns < 60 { 0 } else { columns.saturating_sub(width) / 2 };
    let top_y = rows / 3;
    let top = Rect { x, y: top_y.min(rows.saturating_sub(height)), width, height };
    let collides = cursor_row.is_some_and(|row| top.contains_row(row));
    let y = if collides {
        (rows.saturating_mul(2) / 3).min(rows.saturating_sub(height))
    } else {
        top.y
    };
    Rect { x, y, width, height }
}

fn message_layout(columns: usize, rows: usize, visible: usize, search_count: bool) -> Rect {
    let content_height = visible.saturating_add(usize::from(search_count)).max(1);
    let height = content_height.saturating_add(4).min(rows);
    let width = if columns < 80 { columns } else { columns.min(44) };
    Rect {
        x: columns.saturating_sub(width),
        y: rows.saturating_sub(height),
        width,
        height,
    }
}

fn popup_layout(
    popup: Option<&InsertPopupState>,
    columns: usize,
    rows: usize,
) -> (Option<Rect>, Option<Rect>) {
    let Some(popup) = popup else {
        return (None, None);
    };
    let longest = popup.items.iter().map(|item| {
        item.word.as_bytes().len()
            .saturating_add(item.kind.as_bytes().len())
            .saturating_add(item.menu.as_bytes().len())
            .saturating_add(4)
    }).max().map_or(1, |value| value);
    let width = longest.saturating_add(4).min(columns);
    let height = popup.items.len().saturating_add(4).min(rows);
    let x = popup.anchor.column.min(columns.saturating_sub(width));
    let y = popup.anchor.row.saturating_add(1).min(rows.saturating_sub(height));
    let menu = Rect { x, y, width, height };

    let documentation = popup.documentation().and_then(|info| {
        let desired = String::from_utf8_lossy(info)
            .lines()
            .map(str::len)
            .max()
            .map_or(1, |value| value)
            .saturating_add(4);
        let available_right = columns.saturating_sub(menu.x.saturating_add(menu.width));
        let available_left = menu.x;
        // The preview takes the roomier side. Neither side wide enough is the
        // narrow-terminal case: the preview is dropped so the completion menu
        // keeps its columns, instead of clipping the preview to a strip of
        // border with no text in it.
        let (available, on_right) = if available_right >= available_left {
            (available_right, true)
        } else {
            (available_left, false)
        };
        if available < MIN_FLOAT_COLUMNS {
            return None;
        }
        let doc_width = desired.min(available);
        let doc_x = if on_right {
            menu.x.saturating_add(menu.width)
        } else {
            menu.x.saturating_sub(doc_width)
        };
        Some(Rect { x: doc_x, y: menu.y, width: doc_width, height: height.min(rows.saturating_sub(menu.y)) })
    });
    (Some(menu), documentation)
}

fn history_layout(columns: usize, rows: usize) -> Rect {
    let width = columns.saturating_mul(4) / 5;
    let height = rows.saturating_mul(2) / 3;
    Rect {
        x: columns.saturating_sub(width) / 2,
        y: rows.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// Explicit compositor layers for client chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChromeLayer {
    InsertPopup,
    Documentation,
    Cmdline,
    Wildlist,
    Wildmenu,
    Messages,
    History,
}

impl ChromeLayer {
    #[must_use]
    pub const fn z_index(self) -> u16 {
        match self {
            Self::InsertPopup => 160,
            Self::Documentation => 161,
            Self::Cmdline => 180,
            Self::Wildlist => 181,
            Self::Wildmenu => 182,
            Self::Messages => 200,
            Self::History => 210,
        }
    }

    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::InsertPopup => "Insert completion anchored to the server cursor",
            Self::Documentation => "Selected completion documentation beside the popup",
            Self::Cmdline => "Level-indexed external command-line overlay",
            Self::Wildlist => "Sticky wildlist above the command line",
            Self::Wildmenu => "Horizontal command-line completion strip",
            Self::Messages => "Message stack above every server float",
            Self::History => "Complete paged message history float",
        }
    }
}

/// Errors at the decoded protocol boundary.
#[derive(Debug, Error, PartialEq)]
pub enum ChromeError {
    #[error("message id must be nil, integer, or string, got {0:?}")]
    InvalidMessageId(Object),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_types::OxStr;

    fn chunk(text: &str) -> TextChunk {
        TextChunk::new(0, text, -1)
    }

    fn update(kind: &str, text: &str) -> MessageUpdate {
        MessageUpdate {
            kind: OxStr::from(kind),
            content: vec![chunk(text)],
            replace_last: false,
            history: true,
            append: false,
            id: Object::Nil,
            prompt: false,
        }
    }

    #[test]
    fn chunk_map_uses_protocol_bytes_and_rendered_columns() {
        let map = RenderedChunkMap::from_chunks(&[chunk("aé界\tb")]);
        assert_eq!(map.byte_len, 8);
        assert_eq!(map.column_for_byte(0), 0);
        assert_eq!(map.column_for_byte(1), 1);
        assert_eq!(map.column_for_byte(2), 1);
        assert_eq!(map.column_for_byte(3), 2);
        assert_eq!(map.column_for_byte(6), 4);
        assert_eq!(map.column_for_byte(7), 8);
        assert_eq!(map.column_for_byte(8), 9);
        assert_eq!(map.column_for_byte(99), 9);
    }

    #[test]
    fn cmdline_levels_restore_and_track_block_special_and_cursor() {
        let mut chrome = Chrome::default();
        chrome.cmdline_show(1, vec![chunk("one")], 1, ":", None, 2, -1);
        assert!(chrome.cmdline_special_char(1, "^V", true));
        assert!(chrome.cmdline_block_show(vec![vec![chunk("line one")]]));
        assert!(chrome.cmdline_block_append(vec![chunk("line two")]));
        chrome.cmdline_show(2, vec![chunk("nested")], 3, "=", Some("prompt".into()), 0, 9);
        assert_eq!(chrome.cmdline.len(), 2);
        assert!(chrome.cmdline_pos(2, 6));
        let active = chrome.cmdline.active();
        assert_eq!(active.map(|entry| entry.level), Some(2));
        assert_eq!(active.map(CmdlineLevel::cursor_column), Some(6));
        assert!(chrome.cmdline_hide(2, true));
        assert_eq!(chrome.cmdline.last_hide, Some(CmdlineHide { level: 2, aborted: true }));
        let restored = chrome.cmdline.active();
        assert_eq!(restored.map(|entry| entry.level), Some(1));
        assert_eq!(restored.map(|entry| entry.block.len()), Some(2));
        assert_eq!(restored.and_then(|entry| entry.special.as_ref()).map(|special| special.text.as_bytes()), Some(b"^V".as_slice()));
        assert!(chrome.cmdline_block_hide());
    }

    #[test]
    fn cmdline_show_clears_transient_special() {
        let mut state = CmdlineState::default();
        state.show(1, vec![chunk("a")], 0, ":", None, 0, -1);
        assert!(state.set_special(1, "x", false));
        state.show(1, vec![chunk("ab")], 1, ":", None, 0, -1);
        assert_eq!(state.active().and_then(|entry| entry.special.as_ref()), None);
    }

    #[test]
    fn wildmenu_maps_byte_anchor_and_wildlist_sticks_until_cmdline_hide() {
        let mut chrome = Chrome::default();
        chrome.cmdline_show(1, vec![chunk("a界b")], 0, ":", None, 0, -1);
        chrome.popupmenu_show(vec![PopupItem::new("one", "", "", "")], Some(0), 0, 4, -1);
        assert_eq!(chrome.wildmenu.as_ref().map(|menu| menu.anchor_column), Some(3));
        assert!(chrome.message_show(update("wildlist", "one  two")).is_ok());
        chrome.popupmenu_hide();
        assert!(chrome.sticky_wildlist.is_some());
        assert!(chrome.cmdline_hide(1, false));
        assert!(chrome.sticky_wildlist.is_none());
    }

    #[test]
    fn sticky_kinds_prompts_and_cmdline_messages_wait_for_keypress() {
        let mut chrome = Chrome::default();
        assert!(chrome.message_show(update("emsg", "bad")).is_ok());
        let mut prompt = update("echo", "continue?");
        prompt.prompt = true;
        assert!(chrome.message_show(prompt).is_ok());
        chrome.cmdline_show(1, vec![chunk("x")], 0, ":", None, 0, -1);
        assert!(chrome.message_show(update("quickfix", "during command")).is_ok());
        assert!(chrome.messages.iter().all(|entry| entry.lifetime == MessageLifetime::StickyUntilKeypress));
        chrome.keypress();
        assert!(chrome.messages.is_empty());
    }

    #[test]
    fn every_named_sticky_kind_waits_for_keypress() {
        let mut chrome = Chrome::default();
        for (index, kind) in ["emsg", "echoerr", "lua_error", "rpc_error", "wmsg", "confirm"]
            .into_iter()
            .enumerate()
        {
            let mut message = update(kind, kind);
            message.id = Object::Integer(index as i64);
            assert!(chrome.message_show(message).is_ok());
        }
        assert_eq!(chrome.messages.len(), 6);
        assert!(chrome.messages.iter().all(|entry| entry.lifetime == MessageLifetime::StickyUntilKeypress));
        chrome.keypress();
        assert!(chrome.messages.is_empty());
    }

    #[test]
    fn every_named_ephemeral_and_unknown_kind_expires() {
        let mut chrome = Chrome::default();
        for (index, kind) in ["echo", "echomsg", "lua_print", "quickfix", "undo", "empty", "future_kind"]
            .into_iter()
            .enumerate()
        {
            let mut message = update(kind, kind);
            message.id = Object::Integer(index as i64);
            assert!(chrome.message_show(message).is_ok());
        }
        chrome.finish_batch(TimeMs(0));
        chrome.advance_time(TimeMs(3_999));
        assert_eq!(chrome.messages.len(), 7);
        chrome.advance_time(TimeMs(4_000));
        assert!(chrome.messages.is_empty());
    }

    #[test]
    fn ephemeral_timer_starts_when_stable_and_restarts_on_update() {
        let mut chrome = Chrome::default();
        assert!(chrome.message_show(update("echo", "first")).is_ok());
        assert!(matches!(chrome.messages[0].lifetime, MessageLifetime::PendingBatch { .. }));
        chrome.finish_batch(TimeMs(10));
        chrome.advance_time(TimeMs(4_009));
        assert_eq!(chrome.messages.len(), 1);

        let mut replacement = update("echo", "second");
        replacement.replace_last = true;
        assert!(chrome.message_show(replacement).is_ok());
        assert!(matches!(chrome.messages[0].lifetime, MessageLifetime::PendingBatch { .. }));
        chrome.finish_batch(TimeMs(4_009));
        chrome.advance_time(TimeMs(8_008));
        assert_eq!(chrome.messages.len(), 1);
        chrome.advance_time(TimeMs(8_009));
        assert!(chrome.messages.is_empty());
    }

    #[test]
    fn streaming_appends_then_expires_four_seconds_after_close() {
        let mut chrome = Chrome::default();
        let mut first = update("shell_out", "a");
        first.history = false;
        assert!(chrome.message_show(first).is_ok());
        let mut second = update("shell_out", "b");
        second.append = true;
        second.history = false;
        assert!(chrome.message_show(second).is_ok());
        assert_eq!(chrome.messages[0].content, vec![chunk("a"), chunk("b")]);
        let mut replacement = update("shell_out", "replacement");
        replacement.replace_last = true;
        replacement.history = false;
        assert!(chrome.message_show(replacement).is_ok());
        assert_eq!(chrome.messages[0].content, vec![chunk("replacement")]);
        let key = chrome.messages[0].key.clone();
        assert!(chrome.close_stream(&key, TimeMs(20)));
        chrome.advance_time(TimeMs(4_019));
        assert_eq!(chrome.messages.len(), 1);
        chrome.advance_time(TimeMs(4_020));
        assert!(chrome.messages.is_empty());
    }

    #[test]
    fn progress_replaces_by_id_without_stacking() {
        let mut chrome = Chrome::default();
        let mut first = update("progress", "10%");
        first.id = Object::Integer(7);
        assert!(chrome.message_show(first).is_ok());
        let mut second = update("progress", "90%");
        second.id = Object::Integer(7);
        assert!(chrome.message_show(second).is_ok());
        assert_eq!(chrome.messages.len(), 1);
        assert_eq!(chrome.messages[0].content, vec![chunk("90%")]);
    }

    #[test]
    fn ordinary_messages_replace_only_on_the_full_kind_id_key() {
        let mut chrome = Chrome::default();
        let mut echo = update("echo", "first");
        echo.id = Object::Integer(9);
        assert!(chrome.message_show(echo).is_ok());
        let mut other_kind = update("quickfix", "second");
        other_kind.id = Object::Integer(9);
        assert!(chrome.message_show(other_kind).is_ok());
        assert_eq!(chrome.messages.len(), 2);
        let mut same_key = update("echo", "replacement");
        same_key.id = Object::Integer(9);
        assert!(chrome.message_show(same_key).is_ok());
        assert_eq!(chrome.messages.len(), 2);
        assert_eq!(chrome.messages[0].content, vec![chunk("replacement")]);
    }

    #[test]
    fn progress_keeps_id_replacement_while_cmdline_forces_sticky_lifetime() {
        let mut chrome = Chrome::default();
        chrome.cmdline_show(1, vec![chunk("x")], 0, ":", None, 0, -1);
        let mut first = update("progress", "10%");
        first.id = Object::Integer(7);
        assert!(chrome.message_show(first).is_ok());
        let mut second = update("progress", "90%");
        second.id = Object::Integer(7);
        assert!(chrome.message_show(second).is_ok());
        assert_eq!(chrome.messages.len(), 1);
        assert_eq!(chrome.messages[0].content, vec![chunk("90%")]);
        assert_eq!(chrome.messages[0].lifetime, MessageLifetime::StickyUntilKeypress);
    }

    #[test]
    fn search_count_has_dedicated_non_history_zone() {
        let mut chrome = Chrome::default();
        assert!(chrome.message_show(update("search_count", "[1/3]")).is_ok());
        assert!(chrome.message_show(update("search_count", "[2/3]")).is_ok());
        assert_eq!(chrome.search_count, Some(vec![chunk("[2/3]")]));
        assert!(chrome.messages.is_empty());
        assert!(chrome.history.is_empty());
        let layout = chrome.layout(80, 24, None);
        assert_eq!(layout.search_count.map(|rect| rect.height), Some(1));
        assert_eq!(layout.search_count.map(|rect| rect.y), layout.messages.map(|rect| rect.y + rect.height - 1));
    }

    #[test]
    fn replacement_by_string_id_accepts_byte_exact_identifier() {
        let mut chrome = Chrome::default();
        let mut first = update("echo", "old");
        first.id = Object::String(OxStr(vec![0xff, b'a']));
        assert!(chrome.message_show(first).is_ok());
        let mut second = update("echo", "new");
        second.id = Object::String(OxStr(vec![0xff, b'a']));
        assert!(chrome.message_show(second).is_ok());
        assert_eq!(chrome.messages.len(), 1);
        assert_eq!(chrome.messages[0].content[0].text.as_bytes(), b"new");
    }

    #[test]
    fn invalid_message_id_is_typed_error() {
        let mut chrome = Chrome::default();
        let mut invalid = update("echo", "text");
        invalid.id = Object::Boolean(true);
        assert!(matches!(chrome.message_show(invalid), Err(ChromeError::InvalidMessageId(Object::Boolean(true)))));
    }

    #[test]
    fn five_visible_messages_have_exact_overflow_badge() {
        let mut chrome = Chrome::default();
        for index in 0..7 {
            let mut message = update("echo", &index.to_string());
            message.id = Object::Integer(index);
            assert!(chrome.message_show(message).is_ok());
        }
        let visible = chrome.visible_messages();
        assert_eq!(visible.entries.len(), 5);
        assert_eq!(visible.hidden_count, 2);
        assert_eq!(visible.overflow_badge.as_deref(), Some("+2 more"));
        assert_eq!(visible.entries[0].content, vec![chunk("2")]);
    }

    #[test]
    fn history_float_keeps_complete_record_and_pages() {
        let entries: Vec<_> = (0..7).map(|index| HistoryEntry {
            kind: OxStr::from("echo"),
            content: vec![chunk(&index.to_string())],
            append: false,
        }).collect();
        let mut chrome = Chrome::default();
        chrome.history_show(entries.clone(), true);
        let history = chrome.history_float.as_mut();
        assert_eq!(history.as_ref().map(|state| state.entries.len()), Some(7));
        assert_eq!(history.as_ref().map(|state| state.previous_command), Some(true));
        if let Some(history) = history {
            history.next_page(3);
            assert_eq!(history.page_entries(3), &entries[3..6]);
            history.next_page(3);
            assert_eq!(history.page_entries(3), &entries[6..7]);
            history.previous_page();
            assert_eq!(history.page_entries(3), &entries[3..6]);
        }
        chrome.history_clear();
        assert!(chrome.history.is_empty());
        assert!(chrome.history_float.is_none());
    }

    #[test]
    fn insert_popup_exposes_selected_documentation_float() {
        let mut chrome = Chrome::default();
        chrome.popupmenu_show(
            vec![PopupItem::new("print", "f", "builtin", "Print a value")],
            Some(0),
            4,
            10,
            2,
        );
        assert_eq!(chrome.insert_popup.as_ref().and_then(InsertPopupState::documentation), Some(b"Print a value".as_slice()));
        let layout = chrome.layout(80, 24, None);
        assert!(layout.insert_popup.is_some());
        assert!(layout.documentation.is_some());
        chrome.popupmenu_select(None);
        assert_eq!(chrome.insert_popup.as_ref().and_then(InsertPopupState::documentation), None);
    }

    #[test]
    fn layouts_follow_breakpoints_and_cmdline_collision_rule() {
        let mut chrome = Chrome::default();
        chrome.cmdline_show(1, vec![chunk("command")], 0, ":", None, 0, -1);
        assert!(chrome.message_show(update("echo", "message")).is_ok());

        let wide = chrome.layout(100, 30, None);
        assert_eq!(wide.cmdline.map(|rect| rect.width), Some(72));
        assert_eq!(wide.cmdline.map(|rect| rect.y), Some(10));
        assert_eq!(wide.messages.map(|rect| rect.width), Some(44));
        assert_eq!(wide.messages.map(|rect| rect.x), Some(56));

        let collision = chrome.layout(100, 30, Some(10));
        assert_eq!(collision.cmdline.map(|rect| rect.y), Some(20));

        let medium = chrome.layout(70, 30, None);
        assert_eq!(medium.messages.map(|rect| rect.width), Some(70));
        let at_message_breakpoint = chrome.layout(80, 30, None);
        assert_eq!(at_message_breakpoint.messages.map(|rect| rect.width), Some(44));
        let at_cmdline_breakpoint = chrome.layout(60, 30, None);
        assert_eq!(at_cmdline_breakpoint.cmdline.map(|rect| rect.width), Some(60));
        assert_eq!(at_cmdline_breakpoint.cmdline.map(|rect| rect.height), Some(5));
        let narrow = chrome.layout(59, 30, None);
        assert_eq!(narrow.cmdline.map(|rect| rect.width), Some(59));
        assert_eq!(narrow.cmdline.map(|rect| rect.height), Some(3));
    }

    #[test]
    fn narrow_layout_saturates_without_underflow() {
        let mut chrome = Chrome::default();
        chrome.cmdline_show(1, vec![chunk("x")], 0, ":", None, 0, -1);
        chrome.popupmenu_show(vec![PopupItem::new("long", "k", "m", "docs")], Some(0), 0, 0, 1);
        let layout = chrome.layout(1, 1, Some(0));
        assert_eq!(layout.cmdline, Some(Rect { x: 0, y: 0, width: 1, height: 1 }));
        assert_eq!(layout.insert_popup.map(|rect| (rect.width, rect.height)), Some((1, 1)));
    }

    #[test]
    fn chrome_layers_have_explicit_stable_order_and_descriptions() {
        let layers = [
            ChromeLayer::InsertPopup,
            ChromeLayer::Documentation,
            ChromeLayer::Cmdline,
            ChromeLayer::Wildlist,
            ChromeLayer::Wildmenu,
            ChromeLayer::Messages,
            ChromeLayer::History,
        ];
        assert!(layers.windows(2).all(|pair| pair[0].z_index() < pair[1].z_index()));
        assert_eq!(ChromeLayer::Messages.z_index(), 200);
        assert!(layers.iter().all(|layer| !layer.description().is_empty()));
    }

    #[test]
    fn chrome_layout_hit_tests_own_surface_coordinates() {
        let layout = ChromeLayout {
            cmdline: Some(Rect { x: 14, y: 10, width: 72, height: 3 }),
            wildmenu: Some(Rect { x: 14, y: 14, width: 72, height: 1 }),
            messages: Some(Rect { x: 56, y: 24, width: 44, height: 6 }),
            ..ChromeLayout::default()
        };
        assert!(layout.contains(15, 11));
        assert!(layout.contains(20, 14));
        assert!(layout.contains(99, 29));
        assert!(!layout.contains(0, 0));
        assert!(!layout.contains(14, 9));
        assert!(!layout.contains(14, 13));
        assert!(!layout.contains(55, 26));
        assert!(ChromeLayout::default().contains(3, 3) == false);
    }

    #[test]
    fn server_text_remains_byte_exact_across_append_and_history() {
        let source = "  alpha\nβ — 'quoted'  ";
        let mut chrome = Chrome::default();
        assert!(chrome.message_show(update("echomsg", source)).is_ok());
        assert_eq!(chrome.messages[0].content[0].text.as_bytes(), source.as_bytes());
        assert_eq!(chrome.history[0].content[0].text.as_bytes(), source.as_bytes());
    }

    #[test]
    fn invalid_utf8_text_is_preserved_and_mapped_without_loss() {
        let raw = [b'a', 0xff, b'b'];
        let chunk = TextChunk::new(0, raw, -1);
        let map = RenderedChunkMap::from_chunks(std::slice::from_ref(&chunk));
        assert_eq!(chunk.text.as_bytes(), raw.as_slice());
        assert_eq!(map.byte_len, 3);
        assert_eq!(map.rendered_columns, 3);
        assert_eq!(map.column_for_byte(2), 2);
    }

    #[test]
    fn invalid_utf8_popup_documentation_stays_byte_exact() {
        let info = [b'd', 0xff, b'c'];
        let mut chrome = Chrome::default();
        chrome.popupmenu_show(
            vec![PopupItem::new(b"word", b"k", b"menu", info)],
            Some(0),
            2,
            3,
            1,
        );
        assert_eq!(chrome.insert_popup.as_ref().and_then(InsertPopupState::documentation), Some(info.as_slice()));
        assert!(chrome.layout(40, 12, None).documentation.is_some());
    }

    #[test]
    fn message_clear_keeps_only_sticky_and_open_stream_entries() {
        let mut chrome = Chrome::default();
        assert!(chrome.message_show(update("echo", "temporary")).is_ok());
        assert!(chrome.message_show(update("emsg", "sticky")).is_ok());
        assert!(chrome.message_show(update("shell_out", "open")).is_ok());
        chrome.message_clear();
        assert_eq!(chrome.messages.len(), 2);
        assert!(chrome.messages.iter().all(|entry| matches!(entry.lifetime, MessageLifetime::StickyUntilKeypress | MessageLifetime::StreamingOpen)));
    }

    #[test]
    fn wildcard_list_does_not_enter_normal_message_stack() {
        let mut chrome = Chrome::default();
        chrome.cmdline_show(1, vec![chunk("x")], 0, ":", None, 0, -1);
        assert!(chrome.message_show(update("wildlist", "one  two")).is_ok());
        assert!(chrome.messages.is_empty());
        assert!(chrome.sticky_wildlist.is_some());
        assert!(chrome.cmdline_hide(1, false));
        assert!(chrome.sticky_wildlist.is_none());
    }

    #[test]
    fn shell_return_closes_after_a_stable_batch() {
        let mut chrome = Chrome::default();
        chrome.message_show(MessageUpdate {
            kind: OxStr::from("shell_ret"),
            content: vec![chunk("done")],
            replace_last: false,
            history: false,
            append: false,
            id: Object::Nil,
            prompt: false,
        }).unwrap();
        chrome.finish_batch(TimeMs(10));
        chrome.advance_time(TimeMs(4_009));
        assert_eq!(chrome.messages.len(), 1);
        chrome.advance_time(TimeMs(4_010));
        assert!(chrome.messages.is_empty());
    }

    /// A completion menu anchored at `column` with an item wide enough to make
    /// the menu `word.len() + 8` columns, and a one-line documentation body.
    fn popup_with_documentation(word: &str, column: usize) -> Chrome {
        let mut chrome = Chrome::default();
        chrome.popupmenu_show(
            vec![PopupItem::new(word, "", "", "doc body")],
            Some(0),
            0,
            column,
            1,
        );
        chrome
    }

    // Two promises: the preview takes the roomier side, and it is dropped when
    // that side cannot hold `MIN_FLOAT_COLUMNS`. Each case is arranged so the
    // other promise gives the wrong answer on its own — a placement-only rule
    // would still return a rect at 20 columns, and a suppression-only rule
    // would put every preview on the same side.
    #[test]
    fn the_documentation_preview_picks_the_roomier_side_or_is_dropped() {
        // Room on the right: the preview starts where the menu ends. A
        // left-preferring rule would answer x < menu.x here.
        let chrome = popup_with_documentation("word", 0);
        let layout = chrome.layout(80, 24, None);
        let menu = layout.insert_popup.expect("menu");
        let documentation = layout.documentation.expect("preview beside a wide menu");
        assert_eq!(documentation.x, menu.x + menu.width);
        assert!(documentation.width >= MIN_FLOAT_COLUMNS);

        // Menu pushed to the right edge: the left side is roomier, so the
        // preview ends exactly where the menu starts. A right-preferring rule
        // would run off the screen.
        let chrome = popup_with_documentation("word", 60);
        let layout = chrome.layout(80, 24, None);
        let menu = layout.insert_popup.expect("menu");
        let documentation = layout.documentation.expect("preview left of the menu");
        assert_eq!(documentation.x + documentation.width, menu.x);

        // Twenty columns: a menu that fills the width leaves neither side
        // anything, so the preview is dropped and the menu keeps its columns.
        // A clipping rule would answer Some with width 0.
        let chrome = popup_with_documentation("a-long-completion", 0);
        let layout = chrome.layout(20, 24, None);
        let menu = layout.insert_popup.expect("menu survives a narrow terminal");
        assert_eq!(menu.width, 20);
        assert_eq!(layout.documentation, None);
    }

    // The threshold itself, one case on each side of it: a rule that used `<=`
    // or a different constant answers the other way on one of these.
    #[test]
    fn the_preview_threshold_admits_exactly_min_float_columns() {
        // A menu of width `columns - MIN_FLOAT_COLUMNS` leaves exactly the
        // minimum on the right.
        let chrome = popup_with_documentation("wordwordwo", 0);
        let layout = chrome.layout(18 + MIN_FLOAT_COLUMNS, 24, None);
        assert_eq!(layout.insert_popup.map(|menu| menu.width), Some(18));
        assert_eq!(
            layout.documentation.map(|rect| rect.width),
            Some(MIN_FLOAT_COLUMNS),
            "exactly the minimum is admitted"
        );

        let layout = chrome.layout(18 + MIN_FLOAT_COLUMNS - 1, 24, None);
        assert_eq!(layout.insert_popup.map(|menu| menu.width), Some(18));
        assert_eq!(layout.documentation, None, "one column short is dropped");
    }
}
