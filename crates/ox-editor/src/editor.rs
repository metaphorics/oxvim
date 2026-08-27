//! The single-writer root for all editor state.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use ox_text::{Buffer, Position, UndoTree};
use ox_types::{BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};
use thiserror::Error;

use crate::arglist::ArgList;
use crate::autocmd::Autocmds;
use crate::buffer::{BufferState, BufferStateError, BufferTextEditRequest};
use crate::decoration::Decorations;
use crate::extmark::{ExtmarkPosition, NamespaceId, SignGroup, TextExtent, TextSplice};
use crate::fold::{FoldError, Position as FoldPosition};
use crate::layout::{Geometry, Layout, LayoutError, TabpageState, WinConfig, WindowState};
use crate::mapping::Mappings;
use crate::marks::{Changelists, GlobalMarks, Jumplist, MarkError};
use crate::options::{OptionStore, OptionValue};
use crate::put::{plan_put, put_origin, PutDirection, PutEdit, PutPlan};
use crate::register::{RegisterError, Registers};
use crate::typeahead::Typeahead;

static NEXT_API_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Cloneable allocator for the process-wide dynamic channel key space.
#[derive(Clone, Debug)]
pub struct ChannelIds(Rc<Cell<u64>>);

impl Default for ChannelIds {
    fn default() -> Self { Self::new() }
}

impl ChannelIds {
    /// Start after the reserved stdio and stderr channel ids.
    #[must_use]
    pub fn new() -> Self { Self(Rc::new(Cell::new(3))) }

    /// Allocate one monotonically increasing dynamic channel id.
    #[must_use]
    pub fn allocate(&self) -> u64 {
        let id = self.0.get();
        self.0.set(id.checked_add(1).expect("channel id space exhausted"));
        id
    }
}

/// Classification retained with a message submitted to the editor sink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
    /// Error output such as `nvim_err_writeln`.
    Error,
    /// General echo output.
    Echo,
}

/// Where the message sink sends one message's text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageDestination {
    /// Written to standard error (`message.c` `msg_puts_printf`, line 3049).
    Stderr,
    /// Written to standard output, upstream's `info_message` stream
    /// (`message.c` line 3047).
    Stdout,
    /// Handed to an attached UI (`message.c` `msg_puts_display`, line 2448).
    Ui,
    /// Dropped: batch mode with `'verbose'` zero (`message.c` line 3038).
    Suppressed,
}

/// Process-level state that decides where message output goes.
///
/// `message.c` `msg_use_printf` (line 3013) prints to stdout/stderr whenever
/// nothing else can display the text: no `--embed` peer, no attached UI and
/// no `ext_messages` UI. `main.c` starts a UI only when a terminal is
/// available and none of `--headless`, `--embed`, `-es`/`-Es` was requested
/// (line 332), so those modes reach the printf branch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessageRouting {
    /// `--embed`: an RPC peer owns the message stream (`embedded_mode`).
    pub embedded: bool,
    /// `-es`, `-Es`, `-e -` batch mode (`silent_mode`); it both suppresses
    /// output while `'verbose'` is zero and keeps `main.c` from starting a UI
    /// (line 332, together with `--headless` and `--embed`).
    pub silent: bool,
    /// A UI has attached over RPC (`ui_active()`).
    pub ui_attached: bool,
}

/// Attributes retained for one `:highlight` group.
///
/// Values remain source spellings (`guifg=#rrggbb`, `bold`, `NONE`) so the
/// UI layer can apply terminal- or GUI-specific interpretation later.
pub type HighlightDefinition = BTreeMap<String, String>;
/// Attributes registered by the legacy `:sign define` command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SignDefinition {
    pub(crate) text: Option<String>,
    pub(crate) text_highlight: Option<String>,
    pub(crate) number_highlight: Option<String>,
    pub(crate) line_highlight: Option<String>,
    pub(crate) cursorline_highlight: Option<String>,
}

/// A message retained until a UI or server consumes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Message category.
    pub kind: MessageKind,
    /// API-compatible message payload.
    pub content: Object,
    /// Whether the message should enter message history.
    pub history: bool,
}

/// Failures while mutating [`Editor`] state.
#[derive(Debug, Error)]
pub enum EditorError {
    /// A buffer handle is not live.
    #[error("unknown buffer {0:?}")]
    UnknownBuffer(BufHandle),
    /// A window handle is not live.
    #[error("unknown window {0:?}")]
    UnknownWindow(WinHandle),
    /// A tabpage handle is not live.
    #[error("unknown tabpage {0:?}")]
    UnknownTabpage(TabHandle),
    /// An operation requiring a current tabpage was requested in an empty editor.
    #[error("no current tabpage")]
    NoCurrentTabpage,
    /// The only remaining tabpage cannot be closed.
    #[error("cannot close last tab page")]
    LastTabpage,
    /// A displayed buffer cannot be wiped.
    #[error("cannot wipe buffer {buffer:?} attached to {windows} window(s)")]
    BufferInUse {
        /// Buffer requested for wiping.
        buffer: BufHandle,
        /// Number of windows displaying it.
        windows: usize,
    },
    /// A 32-bit editor handle space was exhausted.
    #[error("{0} handle space exhausted")]
    HandleExhausted(&'static str),
    /// A buffer operation failed.
    #[error(transparent)]
    Buffer(#[from] BufferStateError),
    /// A frame-tree operation failed.
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// A register operation failed.
    #[error(transparent)]
    Register(#[from] RegisterError),
    /// A named-mark operation failed.
    #[error(transparent)]
    Mark(#[from] MarkError),
    /// A fold operation failed.
    #[error(transparent)]
    Fold(#[from] FoldError),
}

/// What to do with an old buffer after its last window switches away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferRelease {
    /// Retain resident text as a hidden buffer.
    KeepLoaded,
    /// Release resident text and undo history.
    Unload,
}

/// Editor input mode visible to the buffer API for cursor-adjustment policy.
///
/// The buffer API needs to know whether the current window is in INSERT mode
/// to decide whether `nvim_buf_set_text` moves the current window's cursor
/// when text is added at the cursor position (`mark_col_adjust` in
/// `mark.c` skips the current cursor when `restart_edit` is set). The host
/// sets this before dispatching API calls that mutate buffer text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BufferEditMode {
    /// Normal command mode (the default).
    #[default]
    Normal,
    /// Insert or replace mode.
    Insert,
}

/// Metadata for one editor-owned terminal channel.
///
/// A terminal channel is backed by a pseudoterminal and displayed through a
/// buffer; the pty slave name is reported by `nvim_get_chan_info` and used by
/// plugins such as termdebug to communicate with the child.
#[derive(Clone, Debug)]
pub struct TerminalChannelInfo {
    pub pty: Option<String>,
    pub buffer: BufHandle,
    /// Bytes of the last line if it has not yet ended with a newline; this
    /// mirrors the text visible in `buffer` so the next chunk can complete it.
    pub pending: Option<Vec<u8>>,
}

/// All mutable editor state under a single `&mut self` discipline.
///
/// No state is process-global. Event-loop and RPC layers can serialize their
/// requests into calls on this root without introducing locks into the model.
pub struct Editor {
    /// Stable identity for host-layer state associated with this editor.
    api_instance_id: u64,
    /// Live buffers in monotonically allocated handle order.
    buffers: BTreeMap<BufHandle, BufferState>,
    /// Tabpage owning each live window handle.
    windows: BTreeMap<WinHandle, TabHandle>,
    /// Live tabpages and their tiled/floating layouts, keyed for lookup.
    tabpages: BTreeMap<TabHandle, TabpageState>,
    /// Tabpage order of record.
    ///
    /// Upstream keeps tabpages in an explicitly ordered linked list
    /// (`tp_next`), walks it in `tabpage_index`, and reorders it with
    /// `:tabmove`, so position is independent of when a tabpage was created.
    /// `tabpages` is keyed by handle and cannot express that, so this is the
    /// order every caller sees and `tabpages` is storage only.
    tab_order: Vec<TabHandle>,
    /// Global and scoped option values.
    options: OptionStore,
    /// Named, numbered, special, and provider-backed registers.
    registers: Registers,
    /// Global `A-Z` and numbered file marks.
    global_marks: GlobalMarks,
    /// Editor jump history.
    jumplist: Jumplist,
    /// Buffer-separated change histories.
    changelists: Changelists,
    /// Global argument list and its current entry (`arglist.c global_alist`).
    arglist: ArgList,
    /// Registered autocmds and augroups.
    autocmds: Autocmds,
    /// Registered decoration providers and active redraw-scoped output.
    decorations: Decorations,
    /// Mode-aware mappings and insert abbreviations.
    mappings: Mappings,
    /// Encoded pending input stack.
    typeahead: Typeahead,
    /// Editor-wide `g:` variables.
    gvars: Dict,
    /// Editor-wide `v:` variables.
    vvars: Dict,
    /// Named highlight groups defined by `:highlight`.
    highlights: BTreeMap<String, HighlightDefinition>,
    /// Named definitions registered by the legacy `:sign define` command.
    sign_definitions: BTreeMap<String, SignDefinition>,
    /// Namespace per named legacy sign group, editor-wide so one group name
    /// resolves to the same namespace in every buffer.
    sign_groups: BTreeMap<String, crate::extmark::SignGroup>,
    /// Messages waiting for a UI or server consumer.
    ///
    /// `message.c` `msg_puts_len` (line 2406) writes to the capture and
    /// redirection sinks before it decides where the text is displayed, so a
    /// message stays retained for `execute()`, `:redir` and `:silent` even
    /// when its destination is [`MessageDestination::Suppressed`].
    messages: Vec<Message>,
    /// Sink decision recorded for each entry of `messages`, index for index.
    ///
    /// Both vectors are only ever pushed by [`Editor::push_message`] and
    /// [`Editor::push_info_message`] and truncated by
    /// [`Editor::truncate_messages`], so they stay the same length.
    message_destinations: Vec<MessageDestination>,
    /// Process modes deciding where message output goes.
    pub message_routing: MessageRouting,
    current_tab: Option<TabHandle>,
    next_buffer: i64,
    next_window: i64,
    next_tabpage: i64,
    /// Current edit mode for API-level cursor adjustment policy.
    edit_mode: BufferEditMode,
    /// Buffer whose current insert/replace session has recorded text.
    active_text_edit: Option<BufHandle>,
    channel_ids: ChannelIds,
    /// Channel id → editor-owned buffer used as a terminal surface.
    ///
    /// `jobstart(..., {'pty': v:true})` and `:terminal` allocate a terminal
    /// channel and bind it to a live buffer so `nvim_get_chan_info` can report
    /// the `buffer` field and so UI can show the child.
    terminal_buffers: BTreeMap<u64, TerminalChannelInfo>,
}
impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// Creates an empty editor. The first allocated handle of each kind is one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            api_instance_id: NEXT_API_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            buffers: BTreeMap::new(),
            windows: BTreeMap::new(),
            tabpages: BTreeMap::new(),
            tab_order: Vec::new(),
            options: OptionStore::new(),
            registers: Registers::new(),
            global_marks: GlobalMarks::new(),
            jumplist: Jumplist::new(),
            changelists: Changelists::new(),
            arglist: ArgList::new(),
            autocmds: Autocmds::new(),
            decorations: Decorations::new(),
            mappings: Mappings::new(),
            typeahead: Typeahead::new(),
            gvars: Dict(Vec::new()),
            vvars: Dict(vec![
                ("errors".into(), Object::Array(Vec::new())),
                (
                    "progpath".into(),
                    Object::String(OxStr(
                        std::env::current_exe().map_or_else(
                            |_| Vec::new(),
                            |path| path.to_string_lossy().into_owned().into_bytes(),
                        ),
                    )),
                ),
                ("_null_string".into(), Object::String(OxStr::from(""))),
                ("_null_list".into(), Object::Array(Vec::new())),
                ("_null_dict".into(), Object::Dict(Dict(Vec::new()))),
            ]),
            highlights: BTreeMap::new(),
            sign_definitions: BTreeMap::new(),
            sign_groups: BTreeMap::new(),
            messages: Vec::new(),
            message_destinations: Vec::new(),
            message_routing: MessageRouting::default(),
            current_tab: None,
            next_buffer: 1,
            next_window: 1,
            next_tabpage: 1,
            edit_mode: BufferEditMode::Normal,
            active_text_edit: None,
            channel_ids: ChannelIds::new(),
            terminal_buffers: BTreeMap::new(),
        }
    }

    /// Allocate a dynamic channel id. Values 1 and 2 are reserved for stdio and stderr.
    #[must_use]
    pub fn allocate_channel_id(&self) -> u64 { self.channel_ids.allocate() }

    /// Return the allocator shared by every dynamic channel owner.
    #[must_use]
    pub fn channel_ids(&self) -> ChannelIds { self.channel_ids.clone() }

    /// Allocate a buffer and bind it to a terminal channel.
    ///
    /// The buffer is created unlisted and empty; the caller is responsible for
    /// recording the pty slave name with `set_terminal_channel`.
    pub fn allocate_terminal_buffer(&mut self, channel: u64) -> Result<BufHandle, EditorError> {
        let buffer = self.create_buffer(false)?;
        self.terminal_buffers.insert(channel, TerminalChannelInfo { pty: None, buffer, pending: Some(Vec::new()) });
        Ok(buffer)
    }

    /// Record or update the pty slave path for an existing terminal channel.
    pub fn set_terminal_channel_pty(&mut self, channel: u64, pty: Option<String>) {
        if let Some(info) = self.terminal_buffers.get_mut(&channel) {
            info.pty = pty;
        }
    }

    /// Append raw PTY output to the terminal channel's buffer.
    ///
    /// Chunks ending mid-line extend the current last line; chunks ending
    /// with a newline are split into complete lines and a trailing empty
    /// segment is discarded so an isolated `\n` does not add a blank line.
    pub fn append_terminal_buffer(&mut self, channel: u64, bytes: &[u8]) -> Result<(), EditorError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let (buffer, had_partial, data) = match self.terminal_buffers.get_mut(&channel) {
            Some(info) => {
                let buffer = info.buffer;
                let had_partial = info.pending.is_some();
                let mut data = info.pending.take().unwrap_or_default();
                data.extend_from_slice(bytes);
                (buffer, had_partial, data)
            }
            None => return Ok(()),
        };

        let mut segments: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
        let ends_newline = data.last() == Some(&b'\n');
        if ends_newline {
            let _ = segments.pop();
        }
        let partial = if ends_newline { None } else { segments.pop().map(|s| s.to_vec()) };

        let strip_cr = |line: &[u8]| line.strip_suffix(b"\r").unwrap_or(line).to_vec();
        let state = self.buffer_mut(buffer)?;
        let count = state.text()?.line_count();
        let cursor = ox_text::Position { lnum: count, col: 0 };

        let (result, new_pending) = if segments.is_empty() {
            // No complete line yet: update the visible partial line.
            let result = if had_partial {
                state.replace_lines(count, count, &[data.clone()], cursor, cursor, 0)
                    .map(|_| ()).map_err(EditorError::Buffer)
            } else {
                state.append_lines(count, &[data.clone()], cursor, 0)
                    .map(|_| ()).map_err(EditorError::Buffer)
            };
            drop(segments);
            (result, Some(data))
        } else {
            let first = segments.remove(0);
            let first_line = strip_cr(first);

            let mut lines: Vec<Vec<u8>> = segments.iter().map(|&line| strip_cr(line)).collect();
            if let Some(tail) = partial.as_ref() {
                lines.push(tail.clone());
            }

            let result = if had_partial {
                state.replace_lines(count, count, &[first_line], cursor, cursor, 0)
                    .map(|_| ()).map_err(EditorError::Buffer)?;
                state.append_lines(count, &lines, cursor, 0)
                    .map(|_| ()).map_err(EditorError::Buffer)
            } else {
                lines.insert(0, first_line);
                state.append_lines(count, &lines, cursor, 0)
                    .map(|_| ()).map_err(EditorError::Buffer)
            };
            (result, partial)
        };

        if result.is_ok() {
            if let Some(info) = self.terminal_buffers.get_mut(&channel) {
                info.pending = new_pending;
            }
        }
        result
    }

    /// Look up the editor-owned terminal channel metadata.
    #[must_use]
    pub fn terminal_channel(&self, channel: u64) -> Option<&TerminalChannelInfo> {
        self.terminal_buffers.get(&channel)
    }

    /// Stable identity for state owned by API and UI host layers.
    #[must_use]
    pub const fn api_instance_id(&self) -> u64 {
        self.api_instance_id
    }

    /// Returns the current tabpage, if one has been created.
    #[must_use]
    pub const fn current_tabpage(&self) -> Option<TabHandle> {
        self.current_tab
    }

    /// Returns the current window, if a tabpage has been created.
    #[must_use]
    pub fn current_window(&self) -> Option<WinHandle> {
        self.current_tab
            .and_then(|tab| self.tabpages.get(&tab))
            .map(TabpageState::current_window)
    }

    /// Returns the buffer displayed by the current window.
    #[must_use]
    pub fn current_buffer(&self) -> Option<BufHandle> {
        self.current_window()
            .and_then(|window| self.window(window).ok())
            .map(|window| window.buffer)
    }

    /// Returns live buffer handles in allocation order.
    #[must_use]
    pub fn buffers(&self) -> Vec<BufHandle> {
        self.buffers.keys().copied().collect()
    }

    /// Returns the highest buffer number ever allocated.
    #[must_use]
    pub fn last_buffer_nr(&self) -> i64 {
        self.next_buffer.saturating_sub(1)
    }

    /// Returns live window handles in allocation order.
    #[must_use]
    pub fn windows(&self) -> Vec<WinHandle> {
        self.windows.keys().copied().collect()
    }

    /// Returns live tabpage handles in tab order.
    #[must_use]
    pub fn tabpages(&self) -> Vec<TabHandle> {
        self.tab_order.clone()
    }

    /// Returns a live tabpage's one-based position, upstream's
    /// `tabpage_index` (`window.c`).
    #[must_use]
    pub fn tabpage_index(&self, tab: TabHandle) -> Option<usize> {
        self.tab_order.iter().position(|entry| *entry == tab).map(|index| index + 1)
    }

    /// Returns the tabpage that owns a live window.
    pub fn window_tabpage(&self, window: WinHandle) -> Result<TabHandle, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        self.windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))
    }

    /// Returns an immutable live buffer state.
    pub fn buffer(&self, buffer: BufHandle) -> Result<&BufferState, EditorError> {
        let resolved = if buffer.is_current() {
            self.current_buffer().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            buffer
        };
        self.buffers
            .get(&resolved)
            .ok_or(EditorError::UnknownBuffer(resolved))
    }

    /// Returns mutable state for a live buffer.
    pub fn buffer_mut(&mut self, buffer: BufHandle) -> Result<&mut BufferState, EditorError> {
        let resolved = if buffer.is_current() {
            self.current_buffer().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            buffer
        };
        self.buffers
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownBuffer(resolved))
    }

    /// Returns an immutable live tabpage state.
    pub fn tabpage(&self, tab: TabHandle) -> Result<&TabpageState, EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        self.tabpages
            .get(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))
    }

    /// Returns immutable viewport state for a live window.
    pub fn window(&self, window: WinHandle) -> Result<&WindowState, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self
            .windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))?;
        Ok(self.tabpage(tab)?.window(resolved)?)
    }

    /// Returns mutable viewport state for a live window.
    pub fn window_mut(&mut self, window: WinHandle) -> Result<&mut WindowState, EditorError> {
        let resolved = self.resolve_window_handle(window)?;
        let tab = self
            .windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))?;
        Ok(self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window_mut(resolved)?)
    }

    /// Makes a live window and its owning tabpage current.
    pub fn set_current_window(&mut self, window: WinHandle) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self
            .windows
            .get(&resolved)
            .copied()
            .ok_or(EditorError::UnknownWindow(resolved))?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_current(resolved)?;
        self.current_tab = Some(tab);
        Ok(())
    }

    /// Displays a live buffer in the current window.
    pub fn set_current_buffer(
        &mut self,
        buffer: BufHandle,
        release: BufferRelease,
    ) -> Result<(), EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        let window = self.current_window().ok_or(EditorError::NoCurrentTabpage)?;
        self.set_window_buffer(window, buffer, release)
    }

    /// Returns the current edit mode for API cursor-adjustment policy.
    #[must_use]
    pub const fn edit_mode(&self) -> BufferEditMode {
        self.edit_mode
    }

    /// Sets the edit mode the host reports before dispatching buffer-text
    /// mutations. The host sets [`BufferEditMode::Insert`] when the current
    /// window is in insert/replace mode so `nvim_buf_set_text` preserves the
    /// current cursor when text is added at the cursor position.
    pub fn set_edit_mode(&mut self, mode: BufferEditMode) {
        if self.edit_mode != mode {
            self.active_text_edit = None;
        }
        self.edit_mode = mode;
    }

    /// Whether this buffer has recorded text in the current insert/replace session.
    #[must_use]
    pub fn has_active_text_edit(&self, buffer: BufHandle) -> bool {
        self.edit_mode == BufferEditMode::Insert
            && self.current_buffer() == Some(buffer)
            && self.active_text_edit == Some(buffer)
    }

    /// Replaces one validated byte range and adjusts every position-bearing
    /// subsystem with column-aware cursor adjustment, matching
    /// `mark_col_adjust` (`mark.c`).
    pub fn replace_buffer_text(
        &mut self,
        buffer: BufHandle,
        request: &BufferTextEditRequest,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        let opens_active_edit = self.edit_mode == BufferEditMode::Insert
            && self.current_buffer() == Some(buffer);
        let prepared = self.buffer(buffer)?.prepare_buffer_text_edit(request)?;
        let splice = prepared.splice;
        let seq = self
            .buffer_mut(buffer)?
            .commit_buffer_text_edit(prepared, cursor_before, cursor_after, timestamp);
        self.splice_text_positions(buffer, splice);
        self.changelists.push(buffer, cursor_after);
        if opens_active_edit {
            self.active_text_edit = Some(buffer);
        }
        Ok(seq)
    }

    /// Replaces the live prompt line without adding the replacement to normal
    /// undo history, while preserving text-edit position geometry
    /// (`f_prompt_setprompt`, `eval/buffer.c`).
    pub fn replace_prompt_line(
        &mut self,
        buffer: BufHandle,
        lnum: usize,
        line: Vec<u8>,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), EditorError> {
        let splice = TextSplice {
            start: ExtmarkPosition::new(lnum.saturating_sub(1), 0),
            old_extent: TextExtent::new(0, old_len),
            new_extent: TextExtent::new(0, new_len),
        };
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        state.replace_prompt_line(lnum, line, splice)?;
        self.splice_text_positions(buffer, splice);
        Ok(())
    }

    /// Replaces several byte ranges as one planning-atomic batch
    /// (`op_reindent`, indent.c:947): every request is prepared against the
    /// pre-edit buffer and the cursor window is resolved before the first
    /// commit, so a validation failure leaves text, cursor, undo, and ticks
    /// untouched. Requests must be row-disjoint, same-row, and strictly
    /// ascending. Line-count-preserving batches commit as one text/derived
    /// tick; structural batches commit bottom-up with per-splice ticks.
    /// Commits join the open undo block as one transaction and the changelist
    /// gains one entry.
    pub(crate) fn replace_buffer_texts(
        &mut self,
        buffer: BufHandle,
        window: WinHandle,
        requests: &[BufferTextEditRequest],
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        debug_assert!(requests.iter().all(|r| r.start.row == r.end.row));
        debug_assert!(requests.windows(2).all(|pair| pair[0].start.row < pair[1].start.row));
        let opens_active_edit = self.edit_mode == BufferEditMode::Insert
            && self.current_buffer() == Some(buffer);
        let any = !requests.is_empty();
        // Validate: every fallible step runs before the first commit.
        let buffer = self.resolve_buffer_handle(buffer)?;
        let window = self.resolve_window_handle(window)?;
        self.window(window)?;
        let mut prepared = Vec::with_capacity(requests.len());
        for request in requests {
            prepared.push(self.buffer(buffer)?.prepare_buffer_text_edit(request)?);
        }
        // Commit: infallible by construction (buffer.rs prepare/commit split).
        let line_preserving = prepared.iter().all(|edit| edit.preserves_line_count());
        let splices: Vec<TextSplice> = if line_preserving {
            prepared.iter().map(|edit| edit.splice).collect()
        } else {
            prepared.iter().rev().map(|edit| edit.splice).collect()
        };
        let mut seq = 0;
        {
            let state = self
                .buffers
                .get_mut(&buffer)
                .expect("buffer resolved during validation");
            if line_preserving {
                seq = state.commit_prepared_line_preserving_batch(
                    prepared,
                    cursor_before,
                    cursor_after,
                    timestamp,
                );
            } else {
                for edit in prepared.into_iter().rev() {
                    seq = state.commit_buffer_text_edit(
                        edit,
                        cursor_before,
                        cursor_after,
                        timestamp,
                    );
                }
            }
        }
        // Splices are applied in commit order. Line-preserving disjoint
        // splices commute; row-count-changing ones must be applied bottom-up
        // so each pre-edit-coordinate transform only row-shifts positions
        // below its already-processed span.
        for splice in splices {
            self.splice_text_positions(buffer, splice);
        }
        if any {
            self.changelists.push(buffer, cursor_after);
            if opens_active_edit {
                self.active_text_edit = Some(buffer);
            }
        }
        // Cursor last; the window was resolved above and evaluation is
        // read-only, so nothing between validation and here can remove it.
        let tab = self
            .windows
            .get(&window)
            .copied()
            .expect("window resolved during validation");
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .expect("tabpage resolved during validation");
        tabpage
            .window_mut(window)
            .expect("window state resolved during validation")
            .cursor = cursor_after;
        Ok(seq)
    }


    /// Changes a live window's cursor position.
    pub fn set_window_cursor(
        &mut self,
        window: WinHandle,
        position: Position,
    ) -> Result<(), EditorError> {
        let window = self.resolve_window_handle(window)?;
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        tabpage.window_mut(window)?.cursor = position;
        Ok(())
    }

    /// Changes the first displayed line of a live window.
    pub fn set_window_topline(
        &mut self,
        window: WinHandle,
        topline: usize,
    ) -> Result<(), EditorError> {
        let window = self.resolve_window_handle(window)?;
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        tabpage.window_mut(window)?.topline = topline.max(1);
        Ok(())
    }

    /// Switches the buffer displayed by a window and updates both attachment counts.
    pub fn set_window_buffer(
        &mut self,
        window: WinHandle,
        buffer: BufHandle,
        release: BufferRelease,
    ) -> Result<(), EditorError> {
        let window = self.resolve_window_handle(window)?;
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        let old_buffer = self.window(window)?.buffer;
        if old_buffer == buffer {
            return Ok(());
        }
        // `win_enter_ext` syncs undo before leaving the current buffer so the
        // block cannot be joined by a later edit made after coming back
        // (`window.c:5275-5279`, `buffer.c:1743-1750`).
        self.sync_buffer_undo(old_buffer);
        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.attach()?;
        }
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        if let Err(error) = tabpage.window_mut(window).map(|state| state.buffer = buffer) {
            if let Some(state) = self.buffers.get_mut(&buffer) {
                state.detach(true);
            }
            return Err(error.into());
        }
        if let Some(state) = self.buffers.get_mut(&old_buffer) {
            state.detach(release == BufferRelease::KeepLoaded);
        }
        Ok(())
    }

    /// Returns option state.
    #[must_use]
    pub const fn options(&self) -> &OptionStore {
        &self.options
    }

    /// Returns the decoration provider registry.
    #[must_use]
    pub const fn decorations(&self) -> &Decorations {
        &self.decorations
    }

    /// Returns mutable decoration provider and redraw state.
    pub const fn decorations_mut(&mut self) -> &mut Decorations {
        &mut self.decorations
    }

    /// Returns mutable option state.
    pub const fn options_mut(&mut self) -> &mut OptionStore {
        &mut self.options
    }

    /// Returns register state.
    #[must_use]
    pub const fn registers(&self) -> &Registers {
        &self.registers
    }

    /// Returns mutable register state.
    pub const fn registers_mut(&mut self) -> &mut Registers {
        &mut self.registers
    }

    /// Returns the global argument list.
    #[must_use]
    pub const fn arglist(&self) -> &ArgList {
        &self.arglist
    }

    /// Returns mutable global argument list state.
    pub const fn arglist_mut(&mut self) -> &mut ArgList {
        &mut self.arglist
    }

    /// Sets a buffer-local named or special mark.
    pub fn set_local_mark(
        &mut self,
        buffer: BufHandle,
        name: char,
        position: Position,
    ) -> Result<Option<Position>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        Ok(state.marks.set(name, position)?)
    }

    /// Reads a buffer-local named or special mark.
    pub fn local_mark(
        &self,
        buffer: BufHandle,
        name: char,
    ) -> Result<Option<Position>, EditorError> {
        Ok(self.buffer(buffer)?.marks.get(name)?)
    }
    /// Returns global marks.
    #[must_use]
    pub const fn global_marks(&self) -> &GlobalMarks {
        &self.global_marks
    }

    /// Returns mutable global marks.
    pub const fn global_marks_mut(&mut self) -> &mut GlobalMarks {
        &mut self.global_marks
    }

    /// Returns named highlight definitions.
    #[must_use]
    pub const fn highlights(&self) -> &BTreeMap<String, HighlightDefinition> {
        &self.highlights
    }

    /// Returns mutable named highlight definitions.
    pub const fn highlights_mut(&mut self) -> &mut BTreeMap<String, HighlightDefinition> {
        &mut self.highlights
    }

    pub(crate) fn sign_definitions(&self) -> &BTreeMap<String, SignDefinition> {
        &self.sign_definitions
    }

    pub(crate) fn sign_definitions_mut(&mut self) -> &mut BTreeMap<String, SignDefinition> {
        &mut self.sign_definitions
    }

    /// Resolves the sign group for `name`, allocating its namespace on first
    /// use the way upstream's `buf_set_sign` creates one per group.
    pub(crate) fn sign_group(&mut self, name: &str) -> SignGroup {
        if let Some(group) = self.sign_groups.get(name) {
            return *group;
        }
        let offset = u32::try_from(self.sign_groups.len()).unwrap_or(u32::MAX);
        let raw = SignGroup::NAMED_BASE.saturating_add(offset);
        let group = SignGroup::from_namespace(NamespaceId::new(raw).expect("named sign namespaces stay positive"));
        self.sign_groups.insert(name.to_owned(), group);
        group
    }

    /// Returns the sign group for `name` when `:sign place` already created it.
    pub(crate) fn sign_group_if_placed(&self, name: &str) -> Option<SignGroup> {
        self.sign_groups.get(name).copied()
    }

    /// Every named sign group allocated so far, in name order.
    pub(crate) fn sign_groups(&self) -> impl Iterator<Item = SignGroup> + '_ {
        self.sign_groups.values().copied()
    }

    /// Returns buffer-separated change history.
    #[must_use]
    pub const fn changelists(&self) -> &Changelists {
        &self.changelists
    }

    /// Returns editor jump history.
    #[must_use]
    pub const fn jumplist(&self) -> &Jumplist {
        &self.jumplist
    }

    /// Returns mutable editor jump history.
    pub const fn jumplist_mut(&mut self) -> &mut Jumplist {
        &mut self.jumplist
    }

    /// Returns registered autocmd and augroup state.
    #[must_use]
    pub const fn autocmds(&self) -> &Autocmds {
        &self.autocmds
    }

    /// Returns mutable autocmd and augroup state.
    pub const fn autocmds_mut(&mut self) -> &mut Autocmds {
        &mut self.autocmds
    }

    /// Returns mapping and abbreviation state.
    #[must_use]
    pub const fn mappings(&self) -> &Mappings {
        &self.mappings
    }

    /// Returns mutable mapping and abbreviation state.
    pub const fn mappings_mut(&mut self) -> &mut Mappings {
        &mut self.mappings
    }

    /// Returns queued encoded input.
    #[must_use]
    pub const fn typeahead(&self) -> &Typeahead {
        &self.typeahead
    }

    /// Returns mutable queued encoded input.
    pub const fn typeahead_mut(&mut self) -> &mut Typeahead {
        &mut self.typeahead
    }

    /// Returns editor-wide `g:` variables.
    #[must_use]
    pub const fn gvars(&self) -> &Dict {
        &self.gvars
    }

    /// Returns mutable editor-wide `g:` variables.
    pub const fn gvars_mut(&mut self) -> &mut Dict {
        &mut self.gvars
    }

    /// Returns editor-wide `v:` variables.
    #[must_use]
    pub const fn vvars(&self) -> &Dict {
        &self.vvars
    }

    /// Returns mutable editor-wide `v:` variables.
    pub const fn vvars_mut(&mut self) -> &mut Dict {
        &mut self.vvars
    }

    /// Returns messages retained by the editor sink.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Returns the sink decision recorded for each retained message.
    ///
    /// Index for index with [`Editor::messages`].
    #[must_use]
    pub fn message_destinations(&self) -> &[MessageDestination] {
        &self.message_destinations
    }

    /// Where a message produced now is sent.
    ///
    /// `message.c` `msg_use_printf` (line 3013) sends output to stdout or
    /// stderr unless an `--embed` peer or an attached UI can display it;
    /// `msg_puts_printf` then drops the text while `silent_mode` is set and
    /// `'verbose'` is zero (line 3038), and otherwise writes it to stderr
    /// (line 3049).
    #[must_use]
    pub fn message_destination(&self) -> MessageDestination {
        if self.message_routing.embedded || self.message_routing.ui_attached {
            return MessageDestination::Ui;
        }
        if self.message_routing.silent && self.verbose_level() == 0 {
            return MessageDestination::Suppressed;
        }
        MessageDestination::Stderr
    }

    /// `'verbose'` (`p_verbose`), zero when the option holds no number.
    #[must_use]
    fn verbose_level(&self) -> i64 {
        match self.options.get_global("verbose") {
            Ok(OptionValue::Number(level)) => *level,
            _ => 0,
        }
    }

    /// Stores a message without claiming that a UI has rendered it, together
    /// with the sink decision that applies to it.
    pub fn push_message(&mut self, message: Message) {
        let destination = self.message_destination();
        self.messages.push(message);
        self.message_destinations.push(destination);
    }

    /// Stores output produced by an informative listing command.
    ///
    /// `print_line` (`ex_cmds.c` line 1701, `:print`/`:number`/`:list`) and
    /// `showoneopt` (`option.c` line 4851, `:set` display) clear
    /// `silent_mode` and set `info_message` around their own output, so that
    /// output survives `-es` and goes to stdout rather than stderr. Only the
    /// printf branch differs: with a UI attached it is an ordinary message.
    pub fn push_info_message(&mut self, message: Message) {
        let destination = match self.message_destination() {
            MessageDestination::Ui => MessageDestination::Ui,
            _ => MessageDestination::Stdout,
        };
        self.messages.push(message);
        self.message_destinations.push(destination);
    }

    /// Discards messages appended at or after `len`.
    ///
    /// Command-output capture uses this after copying newly emitted messages,
    /// matching Neovim's behavior where captured output is not also displayed.
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        self.message_destinations.truncate(len);
    }

    /// Returns tabpage-local variables.
    pub fn tabpage_variables(&self, tab: TabHandle) -> Result<&Dict, EditorError> {
        Ok(self.tabpage(tab)?.variables())
    }

    /// Returns mutable tabpage-local variables.
    pub fn tabpage_variables_mut(&mut self, tab: TabHandle) -> Result<&mut Dict, EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        Ok(self
            .tabpages
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))?
            .variables_mut())
    }

    /// Returns one tabpage's tiled and floating windows in display order.
    pub fn tabpage_windows(&self, tab: TabHandle) -> Result<Vec<WinHandle>, EditorError> {
        Ok(self.tabpage(tab)?.windows())
    }

    /// Resizes and equalizes a tabpage's tiled layout.
    pub fn resize_tabpage(
        &mut self,
        tab: TabHandle,
        geometry: Geometry,
    ) -> Result<(), EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        self.tabpages
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))?
            .resize(geometry)?;
        Ok(())
    }

    /// Equalizes a tabpage's tiled layout within its current rectangle.
    pub fn equalize_tabpage(&mut self, tab: TabHandle) -> Result<(), EditorError> {
        let resolved = if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)?
        } else {
            tab
        };
        self.tabpages
            .get_mut(&resolved)
            .ok_or(EditorError::UnknownTabpage(resolved))?
            .equalize()?;
        Ok(())
    }

    /// Returns window-local variables.
    pub fn window_variables(&self, window: WinHandle) -> Result<&Dict, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self.tabpage(tab)?.window_api_state(window)?.variables())
    }

    /// Returns mutable window-local variables.
    pub fn window_variables_mut(&mut self, window: WinHandle) -> Result<&mut Dict, EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        Ok(self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window_api_state_mut(resolved)?
            .variables_mut())
    }

    /// Returns the highlight namespace selected for a window.
    pub fn window_highlight_namespace(&self, window: WinHandle) -> Result<i64, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self
            .tabpage(tab)?
            .window_api_state(window)?
            .highlight_namespace())
    }

    /// Selects a window highlight namespace without attempting to render it.
    pub fn set_window_highlight_namespace(
        &mut self,
        window: WinHandle,
        namespace: i64,
    ) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window_api_state_mut(resolved)?
            .set_highlight_namespace(namespace);
        Ok(())
    }

    /// Returns assigned screen geometry for a tiled window.
    pub fn window_geometry(&self, window: WinHandle) -> Result<Geometry, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self.tabpage(tab)?.window_geometry(window)?)
    }

    /// Changes a tiled or floating window's width.
    pub fn set_window_width(
        &mut self,
        window: WinHandle,
        width: usize,
    ) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_window_width(resolved, width)?;
        Ok(())
    }

    /// Changes a tiled or floating window's height.
    pub fn set_window_height(
        &mut self,
        window: WinHandle,
        height: usize,
    ) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_window_height(resolved, height)?;
        Ok(())
    }

    /// Returns floating configuration, or `None` for a tiled window.
    pub fn window_config(&self, window: WinHandle) -> Result<Option<&WinConfig>, EditorError> {
        let tab = self.window_tabpage(window)?;
        Ok(self.tabpage(tab)?.window_config(window)?)
    }

    /// Updates an existing floating window configuration.
    pub fn set_window_config(
        &mut self,
        window: WinHandle,
        config: WinConfig,
    ) -> Result<(), EditorError> {
        let resolved = if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)?
        } else {
            window
        };
        let tab = self.window_tabpage(resolved)?;
        self.tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .set_window_config(resolved, config)?;
        Ok(())
    }

    /// Allocates a listed, loaded empty buffer.
    pub fn create_buffer(&mut self, listed: bool) -> Result<BufHandle, EditorError> {
        self.create_buffer_with(Buffer::new(), listed)
    }

    /// Allocates a listed or unlisted buffer around existing text.
    pub fn create_buffer_with(
        &mut self,
        text: Buffer,
        listed: bool,
    ) -> Result<BufHandle, EditorError> {
        let handle = allocate_buffer_handle(&mut self.next_buffer)?;
        self.buffers.insert(handle, BufferState::new(text, listed));
        Ok(handle)
    }

    /// Permanently removes an unattached buffer; its handle is never reused.
    pub fn wipe_buffer(&mut self, buffer: BufHandle) -> Result<BufferState, EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        let state = self
            .buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        if state.attachments != 0 {
            return Err(EditorError::BufferInUse {
                buffer,
                windows: state.attachments,
            });
        }
        self.changelists.remove_buffer(buffer);
        self.options.remove_buffer(buffer);
        self.autocmds.remove_buffer(buffer);
        self.mappings.remove_buffer(buffer);
        self.buffers
            .remove(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))
    }

    /// Releases resident text and undo state for an unattached buffer.
    pub fn unload_buffer(&mut self, buffer: BufHandle) -> Result<(), EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        state.unload()?;
        Ok(())
    }

    /// Creates a tabpage with one tiled window displaying `buffer`, appended
    /// after every existing tabpage.
    pub fn create_tabpage(
        &mut self,
        buffer: BufHandle,
        geometry: Geometry,
    ) -> Result<TabHandle, EditorError> {
        let index = self.tab_order.len();
        self.insert_tabpage(buffer, geometry, index)
    }

    /// Creates a tabpage at upstream's `win_new_tabpage(after)` position
    /// (`window.c:4484-4539`).
    ///
    /// `after` is one-based: `1` makes the new tabpage the first, a larger
    /// value inserts it *before* tabpage `after`, and a value past the end
    /// appends. `0` inserts directly after the current tabpage, which is what
    /// an addressless `:tabnew` passes.
    pub fn create_tabpage_at(
        &mut self,
        buffer: BufHandle,
        geometry: Geometry,
        after: usize,
    ) -> Result<TabHandle, EditorError> {
        let index = if after == 0 {
            self.current_tab
                .and_then(|tab| self.tabpage_index(tab))
                .unwrap_or(self.tab_order.len())
        } else {
            (after - 1).min(self.tab_order.len())
        };
        self.insert_tabpage(buffer, geometry, index)
    }

    /// Closes a tabpage and every window it owns.
    ///
    /// This is the sole owner of tabpage removal. The window path cannot take
    /// that job: `Layout::close` refuses `LastWindow`, so `close_window` is
    /// structurally unable to empty a tabpage and never removes one. Upstream
    /// puts removal in `win_close` instead (`window.c`), which it can because
    /// its layout permits closing a tabpage's last window.
    ///
    /// Refuses the last remaining tabpage, upstream's `E784`.
    pub fn close_tabpage(&mut self, tab: TabHandle) -> Result<(), EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        self.require_tabpage(tab)?;
        if self.tab_order.len() <= 1 {
            return Err(EditorError::LastTabpage);
        }
        // Close every window but the last through the normal path so buffers
        // detach and window options are dropped; the last one goes with the
        // tabpage below, since the layout will not close it.
        let windows = self.tabpage(tab)?.windows();
        for window in windows.iter().skip(1) {
            self.close_window(tab, *window, true)?;
        }
        let removed = self
            .tabpages
            .remove(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        for window in removed.windows() {
            self.windows.remove(&window);
            self.options.remove_window(window);
            if let Ok(state) = removed.window(window) {
                if let Some(buffer_state) = self.buffers.get_mut(&state.buffer) {
                    buffer_state.detach(true);
                }
            }
        }
        self.tab_order.retain(|entry| *entry != tab);
        if self.current_tab == Some(tab) {
            self.current_tab = self.tab_order.first().copied();
        }
        Ok(())
    }

    fn insert_tabpage(
        &mut self,
        buffer: BufHandle,
        geometry: Geometry,
        index: usize,
    ) -> Result<TabHandle, EditorError> {
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        let window = allocate_window_handle(&mut self.next_window)?;
        let tab = allocate_tab_handle(&mut self.next_tabpage)?;
        let state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        let layout = Layout::new(window, state, geometry)?;
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        self.windows.insert(window, tab);
        self.tabpages.insert(tab, TabpageState::new(layout));
        self.tab_order.insert(index.min(self.tab_order.len()), tab);
        self.current_tab = Some(tab);
        Ok(tab)
    }

    /// Makes a live tabpage current.
    pub fn set_current_tabpage(&mut self, tab: TabHandle) -> Result<(), EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        self.require_tabpage(tab)?;
        self.current_tab = Some(tab);
        Ok(())
    }

    /// Splits a tiled window vertically and displays `buffer` on the right.
    pub fn split_vertical(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Right)
    }

    /// Splits a tiled window horizontally and displays `buffer` below.
    pub fn split_horizontal(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Below)
    }

    /// Splits a tiled window vertically and displays `buffer` to the left.
    pub fn split_left(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Left)
    }

    /// Splits a tiled window horizontally and displays `buffer` above.
    pub fn split_above(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Above)
    }

    /// Opens a floating window in `tab`.
    pub fn open_float(
        &mut self,
        tab: TabHandle,
        buffer: BufHandle,
        config: WinConfig,
    ) -> Result<WinHandle, EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        self.require_tabpage(tab)?;
        let window = allocate_window_handle(&mut self.next_window)?;
        let state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        if let Err(error) = tabpage.add_float(window, state, config) {
            if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
                buffer_state.detach(true);
            }
            return Err(error.into());
        }
        self.windows.insert(window, tab);
        Ok(window)
    }

    /// Closes a tiled or floating window, applying the effective hidden policy.
    pub fn close_window(
        &mut self,
        tab: TabHandle,
        window: WinHandle,
        keep_buffer_loaded: bool,
    ) -> Result<WindowState, EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        let window = self.resolve_window_handle(window)?;
        self.require_tabpage(tab)?;
        if self.windows.get(&window) != Some(&tab) {
            return Err(EditorError::UnknownWindow(window));
        }
        let canonical = self
            .tabpages
            .get(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window(window)?
            .clone();
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        let removed = if tabpage
            .floating_windows()
            .any(|candidate| candidate.window == window)
        {
            tabpage.remove_float(window)?.state
        } else {
            tabpage.close_tiled(window)?
        };
        self.windows.remove(&window);
        self.options.remove_window(window);
        if let Some(buffer_state) = self.buffers.get_mut(&canonical.buffer) {
            buffer_state.detach(keep_buffer_loaded);
        }
        Ok(removed)
    }

    /// Replaces buffer lines and adjusts every position-bearing subsystem.
    pub fn replace_buffer_lines(
        &mut self,
        buffer: BufHandle,
        start: usize,
        end: usize,
        lines: &[Vec<u8>],
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        let old_count = end.saturating_sub(start).saturating_add(1);
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let seq = state.replace_lines(
            start,
            end,
            lines,
            cursor_before,
            cursor_after,
            timestamp,
        )?;
        self.splice_positions(buffer, start, old_count, lines.len());
        self.changelists.push(buffer, cursor_after);
        Ok(seq)
    }

    /// Appends buffer lines and adjusts every position-bearing subsystem.
    pub fn append_buffer_lines(
        &mut self,
        buffer: BufHandle,
        after: usize,
        lines: &[Vec<u8>],
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let seq = state.append_lines(after, lines, cursor, timestamp)?;
        let start = after.saturating_add(1);
        self.splice_positions(buffer, start, 0, lines.len());
        self.changelists.push(
            buffer,
            Position {
                lnum: cursor.lnum.saturating_add(lines.len()),
                col: cursor.col,
            },
        );
        Ok(seq)
    }

    /// Undoes a buffer's most recent change, replaying its inverse through
    /// every position-bearing subsystem (marks, jump/change history, window
    /// cursors), matching the direct-mutation pipeline. Returns the undone
    /// header's sequence, or `None` at the oldest change.
    pub fn buffer_undo(&mut self, buffer: BufHandle) -> Result<Option<u64>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let Some(replayed) = state.undo()? else {
            return Ok(None);
        };
        let seq = replayed.first().map(|edit| edit.seq);
        self.finish_replay(buffer, &replayed);
        Ok(seq)
    }

    /// Redoes a buffer's next change, replaying its stored edit through every
    /// position-bearing subsystem. Returns the redone header's sequence, or
    /// `None` at the newest change.
    pub fn buffer_redo(&mut self, buffer: BufHandle) -> Result<Option<u64>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let Some(replayed) = state.redo()? else {
            return Ok(None);
        };
        let seq = replayed.first().map(|edit| edit.seq);
        self.finish_replay(buffer, &replayed);
        Ok(seq)
    }

    /// Navigates a buffer's undo tree to sequence `seq`, replaying every step
    /// through the position-bearing subsystems.
    ///
    /// The target may be behind or ahead of the current state, and on another
    /// branch, which is what `:undo {N}` needs (`undo_time`, `undo.c:1975`).
    /// An unknown sequence is reported, not silently clamped.
    pub fn buffer_undo_to_seq(
        &mut self,
        buffer: BufHandle,
        seq: u64,
    ) -> Result<usize, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let replayed = state.undo_to_seq(seq)?;
        let count = replayed.len();
        for block in replayed {
            self.finish_replay(buffer, &block);
        }
        Ok(count)
    }

    /// Returns a buffer's current undo sequence, upstream's `b_u_seq_cur`.
    pub fn buffer_undo_seq(&self, buffer: BufHandle) -> Result<u64, EditorError> {
        Ok(self
            .buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?
            .undo
            .current_seq())
    }

    /// Closes a buffer's open undo block, so the next edit starts a new one.
    ///
    /// This is upstream's `u_sync` (`undo.c:2704-2717`) and the only way to
    /// move an undo-block boundary from outside `BufferState`. An unknown or
    /// unloaded buffer has no block to close, so it is a no-op rather than an
    /// error: upstream's `u_sync` likewise has nothing to do when the buffer
    /// carries no entries.
    pub fn sync_buffer_undo(&mut self, buffer: BufHandle) {
        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.sync_undo();
        }
    }

    /// Closes the current buffer's open undo block.
    pub fn sync_current_undo(&mut self) {
        if let Some(buffer) = self.current_buffer() {
            self.sync_buffer_undo(buffer);
        }
    }

    /// Reopens a buffer's newest undo block so the next edit joins it
    /// (`:undojoin`, `undo.c:2800-2816`).
    pub fn buffer_undojoin(&mut self, buffer: BufHandle) -> Result<(), EditorError> {
        self.buffer_mut(buffer)?.undojoin()?;
        Ok(())
    }

    /// Returns a buffer's undo tree for reads that need the whole shape,
    /// which is what `undotree()` reports.
    pub fn buffer_undo_tree(&self, buffer: BufHandle) -> Result<&UndoTree, EditorError> {
        Ok(&self
            .buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?
            .undo)
    }

    /// Opens one containing fold, corresponding to `zo`.
    pub fn fold_open(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<bool, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.open(position)?)
    }

    /// Closes one visible containing fold, corresponding to `zc`.
    pub fn fold_close(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<bool, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.close(position)?)
    }

    /// Toggles one containing fold, corresponding to `za`.
    pub fn fold_toggle(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<bool, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.toggle(position)?)
    }

    /// Opens a containing fold and descendants, corresponding to `zO`.
    pub fn fold_open_recursive(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.open_recursive(position)?)
    }

    /// Closes the outer containing fold, corresponding to `zC`.
    pub fn fold_close_recursive(
        &mut self,
        buffer: BufHandle,
        position: FoldPosition,
    ) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.close_recursive(position)?)
    }

    /// Opens every fold in a buffer, corresponding to `zR`.
    pub fn fold_open_all(&mut self, buffer: BufHandle) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.open_all())
    }

    /// Closes every fold in a buffer, corresponding to `zM`.
    pub fn fold_close_all(&mut self, buffer: BufHandle) -> Result<usize, EditorError> {
        Ok(self.buffer_mut(buffer)?.folds.close_all())
    }

    /// Adjusts the editor-wide position-bearing subsystems for one replayed
    /// undo block: every edit splices, and the block leaves one changelist
    /// entry, matching the one entry a recorded block leaves.
    fn finish_replay(&mut self, buffer: BufHandle, replayed: &[crate::buffer::ReplayedEdit]) {
        for edit in replayed {
            self.splice_positions(buffer, edit.start, edit.old_count, edit.new_count);
        }
        if let Some(last) = replayed.last() {
            self.changelists.push(buffer, last.cursor);
        }
    }

    /// Puts a stored register through the buffer mutation pipeline.
    ///
    /// Returns false when the selected register has no retained content.
    pub fn put_register(
        &mut self,
        window: WinHandle,
        name: char,
        direction: PutDirection,
        count: usize,
        timestamp: i64,
    ) -> Result<bool, EditorError> {
        let window = self.resolve_window_handle(window)?;
        let state = self.window(window)?;
        let buffer = state.buffer;
        let mut cursor = state.cursor;
        let Some(content) = self.registers.get(name)?.cloned() else {
            return Ok(false);
        };
        let lines = buffer_lines(self.buffer(buffer)?.text()?)?;
        cursor.lnum = cursor.lnum.clamp(1, lines.len().max(1));
        let origin = put_origin(&lines, cursor, content.kind(), direction);
        let plan = plan_put(&lines, origin, &content, count.max(1), cursor)?;
        self.commit_put_plan(buffer, Some(window), plan, timestamp)?;
        Ok(true)
    }

    /// Inserts explicit content through the same undo-aware pipeline as a register put.
    pub fn put_content(
        &mut self,
        buffer: BufHandle,
        position: Position,
        content: &crate::register::RegisterContent,
        timestamp: i64,
    ) -> Result<(), EditorError> {
        let lines = buffer_lines(self.buffer(buffer)?.text()?)?;
        let plan = plan_put(&lines, position, content, 1, position)?;
        self.commit_put_plan(buffer, None, plan, timestamp)?;
        Ok(())
    }

    fn commit_put_plan(
        &mut self,
        buffer: BufHandle,
        window: Option<WinHandle>,
        plan: PutPlan,
        timestamp: i64,
    ) -> Result<bool, EditorError> {
        let PutPlan { edits, cursor_before, cursor_after } = plan;
        if edits.is_empty() {
            return Ok(false);
        }

        let buffer = self.resolve_buffer_handle(buffer)?;
        let opens_active_edit = self.edit_mode == BufferEditMode::Insert
            && self.current_buffer() == Some(buffer);
        self.buffer(buffer)?.text()?;
        let window = if let Some(window) = window {
            let window = self.resolve_window_handle(window)?;
            self.window(window)?;
            Some(window)
        } else {
            None
        };
        let mut prepared = Vec::with_capacity(edits.len());
        let mut trailing_insert = None;
        for edit in edits {
            match edit {
                PutEdit::Splice(request) => {
                    prepared.push(self.buffer(buffer)?.prepare_buffer_text_edit(&request)?);
                }
                PutEdit::InsertLines { after_lnum, lines } => {
                    debug_assert!(trailing_insert.is_none());
                    trailing_insert = Some((after_lnum, lines));
                }
            }
        }

        let line_preserving = prepared.iter().all(|edit| edit.preserves_line_count());
        let splices: Vec<TextSplice> = prepared.iter().map(|edit| edit.splice).collect();
        let inserted_lines = trailing_insert
            .as_ref()
            .map(|(after_lnum, lines)| (*after_lnum, lines.len()));
        {
            let state = self
                .buffers
                .get_mut(&buffer)
                .expect("buffer resolved during validation");
            if line_preserving {
                if !prepared.is_empty() {
                    state.commit_prepared_line_preserving_batch(
                        prepared,
                        cursor_before,
                        cursor_after,
                        timestamp,
                    );
                }
            } else {
                for edit in prepared {
                    state.commit_buffer_text_edit(
                        edit,
                        cursor_before,
                        cursor_after,
                        timestamp,
                    );
                }
            }
            if let Some((after_lnum, lines)) = trailing_insert {
                state.insert_lines(
                    after_lnum,
                    &lines,
                    cursor_before,
                    cursor_after,
                    timestamp,
                );
            }
        }

        for splice in splices {
            self.splice_text_positions(buffer, splice);
        }
        if let Some((after_lnum, line_count)) = inserted_lines {
            self.splice_positions(buffer, after_lnum + 1, 0, line_count);
        }
        self.changelists.push(buffer, cursor_after);
        if opens_active_edit {
            self.active_text_edit = Some(buffer);
        }
        if let Some(window) = window {
            let tab = self
                .windows
                .get(&window)
                .copied()
                .expect("window resolved during validation");
            let tabpage = self
                .tabpages
                .get_mut(&tab)
                .expect("tabpage resolved during validation");
            tabpage
                .window_mut(window)
                .expect("window state resolved during validation")
                .cursor = cursor_after;
        }
        Ok(true)
    }


    fn split_window(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
        direction: SplitDirection,
    ) -> Result<WinHandle, EditorError> {
        let tab = self.resolve_tabpage_handle(tab)?;
        let target = self.resolve_window_handle(target)?;
        let buffer = self.resolve_buffer_handle(buffer)?;
        self.require_buffer(buffer)?;
        self.require_tabpage(tab)?;
        let window = allocate_window_handle(&mut self.next_window)?;
        let state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        let inserted = match direction {
            SplitDirection::Right => tabpage.split_vertical(target, window, state),
            SplitDirection::Below => tabpage.split_horizontal(target, window, state),
            SplitDirection::Left => tabpage.split_left(target, window, state),
            SplitDirection::Above => tabpage.split_above(target, window, state),
        };
        if let Err(error) = inserted {
            if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
                buffer_state.detach(true);
            }
            return Err(error.into());
        }
        self.windows.insert(window, tab);
        Ok(window)
    }

    fn splice_positions(
        &mut self,
        buffer: BufHandle,
        start: usize,
        old_count: usize,
        new_count: usize,
    ) {
        self.global_marks
            .splice_buffer(buffer, start, old_count, new_count);
        self.jumplist
            .splice_buffer(buffer, start, old_count, new_count);
        self.changelists
            .splice_buffer(buffer, start, old_count, new_count);
        let windows = &self.windows;
        let tabpages = &mut self.tabpages;
        for (&window, &tab) in windows {
            let Some(tabpage) = tabpages.get_mut(&tab) else {
                continue;
            };
            let Ok(state) = tabpage.window_mut(window) else {
                continue;
            };
            if state.buffer != buffer {
                continue;
            }
            splice_position(&mut state.cursor, start, old_count, new_count);
            let mut top = Position {
                lnum: state.topline,
                col: 0,
            };
            splice_position(&mut top, start, old_count, new_count);
            state.topline = top.lnum;
        }
    }

    /// Column-aware position splice for byte-level text edits.
    ///
    /// Like [`Self::splice_positions`] for marks, jumplist, changelists, and
    /// toplines, but also adjusts cursor columns for windows showing the
    /// edited buffer, matching `mark_col_adjust` (`mark.c`).
    fn splice_text_positions(&mut self, buffer: BufHandle, splice: TextSplice) {
        let start = splice.start.row + 1;
        let old_count = splice.old_extent.rows + 1;
        let new_count = splice.new_extent.rows + 1;
        self.global_marks
            .splice_buffer(buffer, start, old_count, new_count);
        self.jumplist
            .splice_buffer(buffer, start, old_count, new_count);
        self.changelists
            .splice_buffer(buffer, start, old_count, new_count);

        let old_end = splice.old_end();
        let anchor = splice.new_end();
        let current_window = self.current_window();
        let insert_current = self.edit_mode == BufferEditMode::Insert
            && self.current_buffer() == Some(buffer);

        let windows = &self.windows;
        let tabpages = &mut self.tabpages;
        for (&window, &tab) in windows {
            let Some(tabpage) = tabpages.get_mut(&tab) else {
                continue;
            };
            let Ok(state) = tabpage.window_mut(window) else {
                continue;
            };
            if state.buffer != buffer {
                continue;
            }

            let is_current = current_window == Some(window);
            let (new_row, new_col) = adjust_text_cursor(
                state.cursor.lnum.saturating_sub(1),
                state.cursor.col,
                splice.start.row,
                splice.start.column,
                old_end.row,
                old_end.column,
                anchor.row,
                anchor.column,
                new_count,
                insert_current && is_current,
            );
            state.cursor.lnum = new_row + 1;
            state.cursor.col = new_col;

            let mut top = Position {
                lnum: state.topline,
                col: 0,
            };
            splice_position(&mut top, start, old_count, new_count);
            state.topline = top.lnum;
        }
    }

    fn require_buffer(&self, buffer: BufHandle) -> Result<(), EditorError> {
        if self.buffers.contains_key(&buffer) {
            Ok(())
        } else {
            Err(EditorError::UnknownBuffer(buffer))
        }
    }

    fn resolve_buffer_handle(&self, buffer: BufHandle) -> Result<BufHandle, EditorError> {
        if buffer.is_current() {
            self.current_buffer().ok_or(EditorError::NoCurrentTabpage)
        } else {
            Ok(buffer)
        }
    }

    fn resolve_window_handle(&self, window: WinHandle) -> Result<WinHandle, EditorError> {
        if window.is_current() {
            self.current_window().ok_or(EditorError::NoCurrentTabpage)
        } else {
            Ok(window)
        }
    }

    fn resolve_tabpage_handle(&self, tab: TabHandle) -> Result<TabHandle, EditorError> {
        if tab.is_current() {
            self.current_tab.ok_or(EditorError::NoCurrentTabpage)
        } else {
            Ok(tab)
        }
    }

    fn require_tabpage(&self, tab: TabHandle) -> Result<(), EditorError> {
        if self.tabpages.contains_key(&tab) {
            Ok(())
        } else {
            Err(EditorError::UnknownTabpage(tab))
        }
    }
}

fn splice_position(position: &mut Position, start: usize, old_count: usize, new_count: usize) {
    let old_end = start.saturating_add(old_count);
    if position.lnum < start {
        return;
    }
    if position.lnum >= old_end {
        position.lnum = if new_count >= old_count {
            position.lnum.saturating_add(new_count - old_count)
        } else {
            position.lnum.saturating_sub(old_count - new_count).max(1)
        };
        return;
    }
    let relative = position.lnum - start;
    if relative < new_count {
        position.lnum = start + relative;
    } else {
        position.lnum = start.max(1);
        position.col = 0;
    }
}

/// Adjusts a 0-based `(row, col)` cursor for a byte-level text replacement
/// from `(start_row, start_col)` to `(end_row, end_col)`, producing
/// `new_count` lines whose last line has the byte length encoded in
/// `anchor_col` for multi-line replacements.
///
/// `anchor_row`/`anchor_col` is where old `(end_row, end_col)` maps to in
/// the new text. When `insert_current` is true (INSERT mode in the current
/// window), a cursor at the start of an insertion stays put, matching
/// `mark_col_adjust` skipping `restart_edit` cursors.
fn adjust_text_cursor(
    crow: usize,
    ccol: usize,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
    anchor_row: usize,
    anchor_col: usize,
    new_count: usize,
    insert_current: bool,
) -> (usize, usize) {
    // Before the edit: no change.
    if crow < start_row {
        return (crow, ccol);
    }
    if crow == start_row && ccol < start_col {
        return (crow, ccol);
    }

    // At the exact start position.
    if crow == start_row && ccol == start_col {
        let range_empty = start_row == end_row && start_col == end_col;
        let has_replacement = new_count > 1 || anchor_col > start_col;
        if !has_replacement {
            // Pure deletion at cursor: stays at start (anchor == start).
            return (crow, ccol);
        }
        if range_empty || (new_count == 1 && anchor_col > start_col) {
            // Insertion or single-line replacement at cursor.
            if insert_current {
                return (crow, ccol);
            }
        }
        return (anchor_row, anchor_col);
    }

    // Within the replaced range (strictly inside).
    if crow < end_row || (crow == end_row && ccol < end_col) {
        return (anchor_row, anchor_col);
    }

    // At or after the end position on the end row.
    if crow == end_row && ccol >= end_col {
        return (anchor_row, anchor_col + (ccol - end_col));
    }

    // Beyond the replaced range: shift row by delta, keep column.
    let delta = new_count as i64 - 1 - (end_row - start_row) as i64;
    let new_row = if delta >= 0 {
        crow.saturating_add(delta as usize)
    } else {
        (crow as i64 + delta).max(0) as usize
    };
    (new_row, ccol)
}

fn buffer_lines(buffer: &Buffer) -> Result<Vec<Vec<u8>>, BufferStateError> {
    (1..=buffer.line_count())
        .map(|line| buffer.line(line))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BufferStateError::from)
}

#[derive(Clone, Copy)]
enum SplitDirection {
    Left,
    Right,
    Above,
    Below,
}

fn allocate_buffer_handle(next: &mut i64) -> Result<BufHandle, EditorError> {
    let value = *next;
    let handle = BufHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("buffer"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("buffer"))?;
    Ok(handle)
}

fn allocate_window_handle(next: &mut i64) -> Result<WinHandle, EditorError> {
    let value = *next;
    let handle = WinHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("window"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("window"))?;
    Ok(handle)
}

fn allocate_tab_handle(next: &mut i64) -> Result<TabHandle, EditorError> {
    let value = *next;
    let handle = TabHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("tabpage"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("tabpage"))?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_lines(editor: &Editor, channel: u64) -> Vec<Vec<u8>> {
        let info = editor.terminal_channel(channel).unwrap();
        let state = editor.buffer(info.buffer).unwrap();
        buffer_lines(&state.text().unwrap()).unwrap()
    }

    #[test]
    fn terminal_buffer_merges_partial_line_chunks() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor.allocate_terminal_buffer(channel).unwrap();

        editor.append_terminal_buffer(channel, b"hel").unwrap();
        editor.append_terminal_buffer(channel, b"lo\n").unwrap();
        editor.append_terminal_buffer(channel, b"wor").unwrap();
        editor.append_terminal_buffer(channel, b"ld\n").unwrap();

        let lines = terminal_lines(&editor, channel);
        assert_eq!(lines, vec![b"hello".to_vec(), b"world".to_vec()]);
    }

    #[test]
    fn terminal_buffer_appends_trailing_newline_without_blank_line() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor.allocate_terminal_buffer(channel).unwrap();

        editor.append_terminal_buffer(channel, b"first\n").unwrap();
        editor.append_terminal_buffer(channel, b"second\n").unwrap();

        let lines = terminal_lines(&editor, channel);
        assert_eq!(lines, vec![b"first".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn terminal_buffer_strips_carriage_returns_from_complete_lines() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor.allocate_terminal_buffer(channel).unwrap();

        editor.append_terminal_buffer(channel, b"one\r\ntwo\r\n").unwrap();

        let lines = terminal_lines(&editor, channel);
        assert_eq!(lines, vec![b"one".to_vec(), b"two".to_vec()]);
    }

    #[test]
    fn terminal_buffer_keeps_partial_line_visible_without_newline() {
        let mut editor = Editor::new();
        let channel = editor.allocate_channel_id();
        editor.allocate_terminal_buffer(channel).unwrap();

        editor.append_terminal_buffer(channel, b"partial").unwrap();

        let lines = terminal_lines(&editor, channel);
        assert_eq!(lines, vec![b"partial".to_vec()]);
    }
}
