//! Buffer ownership, text mutation, and buflist lifecycle.

use std::collections::BTreeMap;

use ox_text::buffer::LineSplice;
use ox_text::{Buffer, BufferError, Cursor, LineEdit, Position, UndoError, UndoStep, UndoTree};
use ox_types::{BufHandle, Dict, OxStr};
use thiserror::Error;

use crate::NamespaceId;
use crate::extmark::{
    ExtmarkError, ExtmarkId, ExtmarkPlacement, ExtmarkPosition, ExtmarkSpliceUndo, TextSplice,
    extent_end,
};
use crate::fold::FoldError;
use crate::marks::LocalMarks;
use crate::{Extmarks, Folds};

/// Neovim reports `b:changedtick` as 2 for a newly created buffer: its
/// bootstrap counts the initial empty line and the buffer-local dict setup
/// (`buf_init_changedtick`, `buffer.c:1941`). The editor-owned counter stays a
/// pure zero-based mutation count, so this offset applies only where the tick
/// becomes script- or API-visible.
const INITIAL_CHANGEDTICK: u64 = 2;

/// A channel's intent to receive buffer update events.
#[derive(Clone, Debug, PartialEq)]
pub struct BufferAttachSubscription {
    /// RPC channel that requested the subscription.
    pub channel_id: u64,
    /// Whether initial buffer contents were requested.
    pub send_buffer: bool,
    /// Event and callback options supplied by the caller.
    pub options: Dict,
}

/// A removed attachment together with the buffer whose lifecycle ended.
///
/// The buffer handle is retained after a wipe so the Lua `on_detach`
/// callback can still receive the same `(event, buffer)` arguments that
/// Neovim sends before it frees the attachment.
#[derive(Clone, Debug, PartialEq)]
pub struct BufferSubscriptionRelease {
    /// Buffer whose attachment was removed.
    pub buffer: BufHandle,
    /// Removed attachment and its callback references.
    pub subscription: BufferAttachSubscription,
}

/// The projection of one committed buffer update used by line callbacks and
/// RPC notifications.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BufferUpdateKind {
    /// A text splice, with the complete post-edit lines for the linewise
    /// callback and RPC notification.
    Mutation {
        /// Number of old lines replaced by the splice.
        old_line_count: usize,
        /// Byte size of the old lines, including one newline per line.
        old_byte_size: usize,
        /// UTF-32 size of the old lines, including one newline per line.
        deleted_codepoints: usize,
        /// UTF-16 size of the old lines, including one newline per line.
        deleted_codeunits: usize,
        /// Complete lines in the post-edit range.
        new_lines: Vec<Vec<u8>>,
    },
    /// The initial contents sent to an RPC attachment.
    Initial {
        /// Complete contents of the attached buffer.
        new_lines: Vec<Vec<u8>>,
    },
    /// The initial changedtick-only notification for an RPC attachment that
    /// did not request the buffer contents.
    Changedtick,
}

/// One committed text mutation projected onto the upstream
/// `nvim_buf_attach` `on_bytes` argument shape: the change start as a
/// zero-based row, byte column, and byte offset, plus the replaced and
/// inserted spans as row/column extents and byte lengths. Positions are
/// buffer-text coordinates: the old span addresses the pre-edit text, the
/// new span the post-edit text, and both share the same start. The tick is the
/// script-visible changedtick the callback observes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BufferBytesEvent {
    /// Subscription identities present when this mutation committed.
    pub subscribers: Vec<u128>,
    /// Script-visible changedtick after the mutation.
    pub tick: u64,
    /// Zero-based start row.
    pub start_row: usize,
    /// Start byte column.
    pub start_col: usize,
    /// Start byte offset into the buffer text.
    pub start_byte: usize,
    /// Rows spanned by the replaced text.
    pub old_row: usize,
    /// Replaced byte columns on the end row.
    pub old_col: usize,
    /// Replaced byte length.
    pub old_byte: usize,
    /// Rows spanned by the inserted text.
    pub new_row: usize,
    /// Inserted byte columns on the end row.
    pub new_col: usize,
    /// Inserted byte length.
    pub new_byte: usize,
    /// Linewise and channel-facing projection of this update.
    pub update: BufferUpdateKind,
}

impl BufferBytesEvent {
    fn initial(subscriber: u128, tick: u64, new_lines: Vec<Vec<u8>>) -> Self {
        Self {
            subscribers: vec![subscriber],
            tick,
            start_row: 0,
            start_col: 0,
            start_byte: 0,
            old_row: 0,
            old_col: 0,
            old_byte: 0,
            new_row: 0,
            new_col: 0,
            new_byte: 0,
            update: BufferUpdateKind::Initial { new_lines },
        }
    }

    fn changedtick(subscriber: u128, tick: u64) -> Self {
        Self::changedtick_for(vec![subscriber], tick)
    }

    fn changedtick_for(subscribers: Vec<u128>, tick: u64) -> Self {
        Self {
            subscribers,
            tick,
            start_row: 0,
            start_col: 0,
            start_byte: 0,
            old_row: 0,
            old_col: 0,
            old_byte: 0,
            new_row: 0,
            new_col: 0,
            new_byte: 0,
            update: BufferUpdateKind::Changedtick,
        }
    }
}

/// Byte length of the span from `start` (inclusive) to `end` (exclusive)
/// across full line bodies: `lines[0]` is the start row's whole line
/// without its terminator. Rows past the vector (an insertion end landing
/// on the following line) contribute nothing; their newline was already
/// counted by the previous row.
fn span_bytes(lines: &[Vec<u8>], start: ExtmarkPosition, end: ExtmarkPosition) -> usize {
    let mut bytes = 0;
    for row in start.row..=end.row {
        let line_len = lines.get(row - start.row).map_or(0, Vec::len);
        if row == start.row && row == end.row {
            bytes += end.column.saturating_sub(start.column);
        } else if row == start.row {
            bytes += line_len.saturating_sub(start.column) + 1;
        } else if row == end.row {
            bytes += end.column;
        } else {
            bytes += line_len + 1;
        }
    }
    bytes
}

/// Returns the linewise byte, UTF-32, and UTF-16 sizes of deleted lines.
///
/// Buffer text is UTF-8 by construction. `from_utf8_lossy` keeps this helper
/// total for the editor's internal invariant while matching the replacement
/// character accounting if an invalid byte sequence ever crosses this seam.
fn linewise_deleted_sizes(lines: &[Vec<u8>]) -> (usize, usize, usize) {
    lines.iter().fold((0, 0, 0), |(bytes, codepoints, codeunits), line| {
        let text = String::from_utf8_lossy(line);
        (
            bytes.saturating_add(line.len().saturating_add(1)),
            codepoints.saturating_add(text.chars().count().saturating_add(1)),
            codeunits.saturating_add(text.encode_utf16().count().saturating_add(1)),
        )
    })
}


/// End column relative to the change start: absolute when the span covers
/// several rows, start-relative within a single row, matching upstream
/// `on_bytes` (`buf_updates_send_tick` reports the same two shapes).
fn end_column(end: ExtmarkPosition, start: ExtmarkPosition) -> usize {
    if end.row == start.row {
        end.column.saturating_sub(start.column)
    } else {
        end.column
    }
}

/// Failures while changing a buffer or its lifecycle.
#[derive(Debug, Error)]
pub enum BufferStateError {
    /// The underlying text operation failed.
    #[error(transparent)]
    Text(#[from] BufferError),
    /// A displayed buffer cannot be unloaded.
    #[error("cannot unload a buffer attached to {0} window(s)")]
    Attached(usize),
    /// Text is unavailable until the buffer is loaded again.
    #[error("buffer text is not loaded")]
    Unloaded,
    /// An extmark could not be adjusted through the text mutation.
    #[error(transparent)]
    Extmark(#[from] ExtmarkError),
    /// A manual fold could not be normalized after the text mutation.
    #[error(transparent)]
    Fold(#[from] FoldError),
    /// An undo-tree navigation failed.
    #[error(transparent)]
    Undo(#[from] UndoError),
    /// A byte-precise text edit request was invalid.
    #[error(transparent)]
    TextEdit(#[from] BufferTextEditError),
}

/// Failures while validating a byte-precise buffer text edit.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BufferTextEditError {
    /// The requested byte range was reversed.
    #[error("text edit range is reversed")]
    ReversedRange,
    /// A row or column was outside the resident buffer.
    #[error("text edit position is out of range")]
    OutOfRange,
    /// A byte column split a UTF-8 code point.
    #[error("byte column {0} is not a UTF-8 boundary")]
    NotCharBoundary(usize),
    /// A logical replacement line contained a newline byte.
    #[error("text edit replacement element contains a newline")]
    EmbeddedNewline,
    /// A logical replacement line was not valid UTF-8.
    #[error("text edit replacement element is not valid UTF-8")]
    InvalidUtf8,
}

/// One validated byte-precise text replacement against a pre-edit snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferTextEditRequest {
    /// Zero-based inclusive start of the spliced byte range.
    pub start: ExtmarkPosition,
    /// Zero-based exclusive end of the spliced byte range.
    pub end: ExtmarkPosition,
    /// Raw replacement lines under `nvim_buf_set_text` semantics.
    ///
    /// Row count is arbitrary. The kernel composes the start-line prefix and
    /// end-line suffix around these lines. An empty vector means deletion and
    /// is normalized to one empty line.
    pub replacement: Vec<Vec<u8>>,
}

pub(crate) struct PreparedBufferTextEdit {
    start_line: usize,
    before: Vec<Vec<u8>>,
    after: Vec<Vec<u8>>,
    pub(crate) splice: TextSplice,
}

impl PreparedBufferTextEdit {
    pub(crate) const fn preserves_line_count(&self) -> bool {
        self.before.len() == self.after.len()
    }
}

/// Independently combinable buffer status flags.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BufferFlags(u8);

impl BufferFlags {
    /// Resident text differs from the last saved undo state.
    pub const MODIFIED: Self = Self(1 << 0);
    /// Read-only policy data; command layers decide whether to raise
    /// E37/E89-class errors.
    pub const READONLY: Self = Self(1 << 1);
    /// The buffer appears in the buffer list.
    pub const LISTED: Self = Self(1 << 2);
    /// The buffer name changed without reading or writing that path.
    pub const NOTEDITED: Self = Self(1 << 3);

    /// Whether every flag in `other` is enabled.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Enables or disables `flag` without touching the other bits.
    pub const fn set(&mut self, flag: Self, enabled: bool) {
        if enabled {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

/// Whether a buffer's text and undo state are resident, and whether a
/// resident buffer currently has a window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferResidency {
    /// Text and undo state are resident and at least one window displays
    /// the buffer.
    Displayed,
    /// Text and undo state are resident and no window displays the buffer.
    Hidden,
    /// Text and undo state are released.
    Unloaded,
}

impl BufferResidency {
    /// Whether text and undo state are resident.
    #[must_use]
    pub const fn is_loaded(self) -> bool {
        !matches!(self, Self::Unloaded)
    }

    /// Whether resident text currently has no window.
    #[must_use]
    pub const fn is_hidden(self) -> bool {
        matches!(self, Self::Hidden)
    }
}

/// Text and buffer-local state owned by [`crate::Editor`].
#[derive(Clone, Debug)]
pub struct BufferState {
    /// Rope-backed text.
    text: Buffer,
    /// API-visible buffer name as uninterpreted bytes.
    name: OxStr,
    /// Buffer-local API variables in insertion order.
    variables: Dict,
    /// Names of buffer-local variables locked by `:lockvar`, persisted from
    /// the eval scope so the API (`nvim_buf_set_var`/`nvim_buf_del_var`) can
    /// reject mutations with "Key is locked: {name}" (upstream
    /// `dict_check_writable` checks `DI_FLAGS_LOCK`).
    locked_vars: Vec<OxStr>,
    /// Bumped by every variable writer; the differential Ex-variable sync
    /// skips re-reading an unchanged map.
    variables_version: u64,
    /// RPC subscriptions use channel keys; Lua keys occupy the wider namespace.
    subscriptions: BTreeMap<u128, BufferAttachSubscription>,
    /// Subscriptions removed while the buffer is still owned by the editor.
    /// The Lua host drains this queue after the editor borrow ends.
    pending_subscription_releases: Vec<BufferAttachSubscription>,
    next_lua_subscription: u128,
    /// Committed mutations and their original recipients, in commit order.
    pending_bytes: Vec<BufferBytesEvent>,
    /// Branch-preserving undo history.
    pub undo: UndoTree,
    /// Named and special buffer-local marks.
    pub marks: LocalMarks,
    /// Buffer-relative extmarks and their decoration attributes.
    pub extmarks: Extmarks,
    /// Compact extmark position deltas per undo header, one per grouped edit
    /// in application order.
    extmark_undo: BTreeMap<u64, Vec<ExtmarkSpliceUndo>>,
    /// Lazily computed and manual buffer folds.
    pub folds: Folds,
    /// Modified/read-only policy and buffer-list membership.
    pub flags: BufferFlags,
    /// Prefix currently owned by prompt-buffer input (`b_prompt_text`).
    prompt: Vec<u8>,
    /// One-based line containing the live prompt (`b_prompt_start.mark.lnum`).
    prompt_start: usize,
    /// Monotonic Neovim-compatible text change counter for this buffer lifetime.
    changedtick: u64,
    /// Text changedtick observed when the buffer was last marked saved.
    saved_changedtick: u64,
    /// Final-EOL state at the last save.
    saved_has_eol: bool,
    /// Undo-tree state corresponding to the last saved contents, as the
    /// header sequence and how many edits that header held. The edit count is
    /// part of it because a header keeps growing while its block is open, so
    /// the sequence alone cannot tell a saved state from a later edit that
    /// joined the same block.
    saved_undo_state: (u64, usize),
    /// Explicit `'modified'` setting, retained until the buffer is marked saved.
    forced_modified: bool,
    /// Whether text and undo state are resident, and whether a resident
    /// buffer currently has no window.
    pub residency: BufferResidency,
    /// Number of windows displaying the buffer.
    pub attachments: usize,
    /// Text generation last consumed by diagnostics.
    pub changedtick_diag: u64,
    /// Text generation last consumed by fold computation.
    pub changedtick_fold: u64,
}

impl Default for BufferState {
    fn default() -> Self {
        Self::new(Buffer::new(), true)
    }
}

/// Metadata recovered by replaying one undo/redo step, for adjusting the
/// editor-wide position-bearing subsystems (jump/change history, windows).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayedEdit {
    /// The undone or redone header's sequence.
    pub seq: u64,
    /// First affected line (one-based).
    pub start: usize,
    /// Lines present before the replayed step.
    pub old_count: usize,
    /// Lines present after the replayed step.
    pub new_count: usize,
    /// Cursor the replayed step leaves the edit at.
    pub cursor: Position,
}

impl BufferState {
    /// Creates a loaded buffer with no window attachments.
    #[must_use]
    pub fn new(text: Buffer, listed: bool) -> Self {
        let mut flags = BufferFlags::default();
        flags.set(BufferFlags::LISTED, listed);
        let prompt_start = text.line_count().max(1);
        let saved_has_eol = text.has_eol();
        Self {
            text,
            name: OxStr::from(""),
            variables: Dict(Vec::new()),
            locked_vars: Vec::new(),
            variables_version: 1,
            subscriptions: BTreeMap::new(),
            pending_subscription_releases: Vec::new(),
            next_lua_subscription: u128::from(u64::MAX) + 1,
            pending_bytes: Vec::new(),
            undo: UndoTree::new(),
            marks: LocalMarks::new(),
            extmarks: Extmarks::new(),
            extmark_undo: BTreeMap::new(),
            folds: Folds::new(),
            flags,
            residency: BufferResidency::Hidden,
            prompt: Vec::new(),
            prompt_start,
            changedtick: 0,
            saved_changedtick: 0,
            saved_has_eol,
            saved_undo_state: (0, 0),
            forced_modified: false,
            attachments: 0,
            changedtick_diag: 0,
            changedtick_fold: 0,
        }
    }

    /// Returns the API-visible buffer name.
    #[must_use]
    pub const fn name(&self) -> &OxStr {
        &self.name
    }

    /// Returns the stored prompt prefix.
    #[must_use]
    pub fn prompt(&self) -> &[u8] {
        &self.prompt
    }

    /// Returns the effective prompt prefix, defaulting to Neovim's `% `.
    #[must_use]
    pub fn effective_prompt(&self) -> &[u8] {
        if self.prompt.is_empty() {
            b"% "
        } else {
            &self.prompt
        }
    }

    /// Returns the authoritative one-based live prompt row.
    #[must_use]
    pub const fn prompt_start(&self) -> usize {
        self.prompt_start
    }

    /// Stores a prompt prefix and its authoritative live row.
    pub fn set_prompt(&mut self, prompt: Vec<u8>, lnum: usize) {
        self.prompt = prompt;
        self.prompt_start = lnum.clamp(1, self.text.line_count());
    }

    /// Replaces the prompt prefix without changing its live row.
    pub fn set_prompt_text(&mut self, prompt: Vec<u8>) {
        self.prompt = prompt;
    }

    /// Replaces the API-visible buffer name.
    pub fn set_name(&mut self, name: OxStr) {
        self.name = name;
    }

    /// Returns buffer-local API variables.
    #[must_use]
    pub const fn variables(&self) -> &Dict {
        &self.variables
    }

    /// Returns mutable buffer-local API variables.
    pub fn variables_mut(&mut self) -> &mut Dict {
        self.variables_version = self.variables_version.wrapping_add(1);
        &mut self.variables
    }

    /// Counts queued mutation events without taking them, for the dispatch
    /// gate that drains only when a call queued new events.
    #[must_use]
    pub fn pending_bytes_len(&self) -> usize {
        self.pending_bytes.len()
    }

    /// Counts queued update and detach work without taking it, for the Lua
    /// dispatch gate that must deliver lifecycle callbacks after editor code.
    #[must_use]
    pub fn pending_callback_work_len(&self) -> usize {
        self.pending_bytes.len() + self.pending_subscription_releases.len()
    }

    /// Returns whether a buffer-local variable is locked by `:lockvar`.
    #[must_use]
    pub fn is_var_locked(&self, name: &OxStr) -> bool {
        self.locked_vars
            .iter()
            .any(|locked| locked.as_bytes() == name.as_bytes())
    }

    /// Replaces the set of locked buffer-local variable names, called by the
    /// eval scope sync to persist `:lockvar` state into editor-owned storage.
    pub fn set_locked_vars(&mut self, names: Vec<OxStr>) {
        self.locked_vars = names;
    }

    /// Returns the variable-map version used by the differential sync.
    #[must_use]
    pub const fn variables_version(&self) -> u64 {
        self.variables_version
    }

    /// Returns requested buffer event subscriptions.
    #[must_use]
    pub const fn subscriptions(&self) -> &BTreeMap<u128, BufferAttachSubscription> {
        &self.subscriptions
    }

    /// Takes the queued mutation events in commit order, leaving the queue
    /// empty. The Lua-side drain calls this before invoking callbacks, so
    /// no editor borrow is held while user code runs.
    pub fn take_bytes_events(&mut self) -> Vec<BufferBytesEvent> {
        std::mem::take(&mut self.pending_bytes)
    }

    /// Puts undelivered events back at the front of the queue.
    ///
    /// A channel transport can fail after the editor has committed a change.
    /// Keeping those events queued lets the owning server retry or report the
    /// failure without silently losing the mutation notification.
    pub fn prepend_bytes_events(&mut self, mut events: Vec<BufferBytesEvent>) {
        if events.is_empty() {
            return;
        }
        events.append(&mut self.pending_bytes);
        self.pending_bytes = events;
    }

    /// Adds a subscription, queuing the previous value when its identity is reused.
    pub fn insert_subscription(
        &mut self,
        id: u128,
        subscription: BufferAttachSubscription,
    ) {
        let is_rpc = subscription.channel_id != 0;
        let send_buffer = subscription.send_buffer;
        if let Some(previous) = self.subscriptions.insert(id, subscription) {
            self.pending_subscription_releases.push(previous);
        }
        if is_rpc {
            let event = if send_buffer {
                BufferBytesEvent::initial(id, self.script_changedtick(), self.snapshot_lines())
            } else {
                BufferBytesEvent::changedtick(id, self.script_changedtick())
            };
            self.pending_bytes.push(event);
        }
    }
    fn snapshot_lines(&self) -> Vec<Vec<u8>> {
        (1..=self.text.line_count())
            .map(|lnum| match self.text.line(lnum) {
                Ok(line) => line,
                Err(error) => unreachable!("resident buffer line must exist: {error}"),
            })
            .collect()
    }
    /// Adds a distinct Lua attachment without sharing RPC channel identities.
    /// Returns the unique subscription id assigned to this attachment.
    pub fn attach_lua(&mut self, subscription: BufferAttachSubscription) -> u128 {
        let id = self.next_lua_subscription;
        self.next_lua_subscription += 1;
        self.insert_subscription(id, subscription);
        id
    }
    /// Removes one attachment and its pending deliveries.
    pub fn remove_subscription(&mut self, id: u128) {
        if let Some(subscription) = self.subscriptions.remove(&id) {
            self.pending_subscription_releases.push(subscription);
        }
        for event in &mut self.pending_bytes {
            event.subscribers.retain(|recipient| *recipient != id);
        }
        self.pending_bytes
            .retain(|event| !event.subscribers.is_empty());
    }

    /// Removes every attachment owned by `channel_id` and its pending
    /// deliveries. Used by `nvim_buf_detach` for both RPC channels and
    /// in-process Lua calls.
    pub fn remove_subscriptions_by_channel(&mut self, channel_id: u64) {
        let removed: Vec<u128> = self
            .subscriptions
            .iter()
            .filter_map(|(id, sub)| (sub.channel_id == channel_id).then_some(*id))
            .collect();
        if removed.is_empty() {
            return;
        }
        for id in &removed {
            if let Some(subscription) = self.subscriptions.remove(id) {
                self.pending_subscription_releases.push(subscription);
            }
        }
        for event in &mut self.pending_bytes {
            event.subscribers.retain(|id| !removed.contains(id));
        }
        self.pending_bytes
            .retain(|event| !event.subscribers.is_empty());
    }

    /// Takes removed subscriptions while preserving this buffer's handle.
    ///
    /// Editor-level drains use this form so lifecycle callbacks can run after
    /// the editor borrow ends without having to recover a wiped buffer.
    pub fn take_pending_subscription_releases_for(
        &mut self,
        buffer: BufHandle,
    ) -> Vec<BufferSubscriptionRelease> {
        std::mem::take(&mut self.pending_subscription_releases)
            .into_iter()
            .map(|subscription| BufferSubscriptionRelease {
                buffer,
                subscription,
            })
            .collect()
    }

    /// Consumes a buffer while retaining the handle on every release record.
    pub fn into_released_subscriptions_for(
        mut self,
        buffer: BufHandle,
    ) -> Vec<BufferSubscriptionRelease> {
        let mut released = std::mem::take(&mut self.pending_subscription_releases)
            .into_iter()
            .map(|subscription| BufferSubscriptionRelease {
                buffer,
                subscription,
            })
            .collect::<Vec<_>>();
        released.extend(
            std::mem::take(&mut self.subscriptions)
                .into_values()
                .map(|subscription| BufferSubscriptionRelease {
                    buffer,
                    subscription,
                }),
        );
        released
    }

    /// Takes subscriptions removed while the buffer remains owned by the editor.
    pub fn take_pending_subscription_releases(&mut self) -> Vec<BufferAttachSubscription> {
        std::mem::take(&mut self.pending_subscription_releases)
    }

    /// Consumes a buffer and returns every subscription that still owns refs.
    ///
    /// Callers that remove a buffer must pass the returned subscriptions to the
    /// Lua host before dropping them; no editor-owned code knows how to free
    /// the registry values.
    pub fn into_released_subscriptions(mut self) -> Vec<BufferAttachSubscription> {
        let mut released = std::mem::take(&mut self.pending_subscription_releases);
        released.extend(std::mem::take(&mut self.subscriptions).into_values());
        released
    }

    /// Returns mutable requested buffer event subscriptions.
    pub const fn subscriptions_mut(&mut self) -> &mut BTreeMap<u128, BufferAttachSubscription> {
        &mut self.subscriptions
    }

    /// Returns the editor-owned text change counter.
    #[must_use]
    pub const fn changedtick(&self) -> u64 {
        self.changedtick
    }

    /// Returns the tick as Neovim exposes it through `b:changedtick` and
    /// `nvim_buf_get_changedtick`, offset by the bootstrap ticks upstream
    /// spends creating the buffer. Cache keys and delta contracts inside the
    /// editor use [`changedtick`](Self::changedtick) instead.
    #[must_use]
    pub const fn script_changedtick(&self) -> u64 {
        self.changedtick.wrapping_add(INITIAL_CHANGEDTICK)
    }

    /// Returns the text generation recorded at the last successful save.
    #[must_use]
    pub const fn saved_changedtick(&self) -> u64 {
        self.saved_changedtick
    }

    /// Forces `'modified'` until the buffer is marked saved.
    pub fn mark_modified(&mut self) {
        self.forced_modified = true;
        self.flags.set(BufferFlags::MODIFIED, true);
    }

    /// Records the current undo state as saved and clears `'modified'`.
    ///
    /// Neovim marks the active undo branch unchanged after writing so that
    /// undoing away from, or returning to, that point restores the flag
    /// (`src/nvim/undo.c:2818-2824`, `src/nvim/bufwrite.c:1727-1738`).
    pub fn mark_saved(&mut self) {
        self.forced_modified = false;
        self.saved_changedtick = self.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_state = self.undo_state();
        self.flags.set(BufferFlags::MODIFIED, false);
    }

    /// Returns resident text, or an unloaded-state error.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not resident.
    pub fn text(&self) -> Result<&Buffer, BufferStateError> {
        if self.residency.is_loaded() {
            Ok(&self.text)
        } else {
            Err(BufferStateError::Unloaded)
        }
    }

    /// Replaces unloaded resident text before a window attaches.
    pub fn load(&mut self, text: Buffer) {
        self.text = text;
        self.bump_changedtick();
        self.prompt_start = self.prompt_start.clamp(1, self.text.line_count().max(1));
        self.undo = UndoTree::new();
        self.extmarks = Extmarks::new();
        self.extmark_undo.clear();
        self.folds = Folds::new();
        self.flags.set(BufferFlags::MODIFIED, false);
        self.saved_changedtick = self.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_state = (0, 0);
        self.residency = if self.attachments == 0 {
            BufferResidency::Hidden
        } else {
            BufferResidency::Displayed
        };
    }

    /// Attaches one window to resident text.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not resident.
    pub fn attach(&mut self) -> Result<(), BufferStateError> {
        self.require_loaded()?;
        self.attachments = self.attachments.saturating_add(1);
        self.residency = BufferResidency::Displayed;
        Ok(())
    }

    /// Detaches one window.
    ///
    /// When the last window leaves, `keep_loaded` models the effective
    /// `'hidden'` policy: true retains text as a hidden buffer, while false
    /// unloads it. Listedness is independent and survives unloading.
    pub fn detach(&mut self, keep_loaded: bool) {
        self.attachments = self.attachments.saturating_sub(1);
        if self.attachments != 0 {
            return;
        }
        self.residency = if keep_loaded {
            BufferResidency::Hidden
        } else {
            BufferResidency::Unloaded
        };
        if !keep_loaded {
            self.release_resident_state();
        }
    }

    /// Unloads text state when no window displays the buffer.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Attached`] with the current attachment count
    /// when one or more windows still display the buffer.
    pub fn unload(&mut self) -> Result<(), BufferStateError> {
        if self.attachments != 0 {
            return Err(BufferStateError::Attached(self.attachments));
        }
        self.residency = BufferResidency::Unloaded;
        self.release_resident_state();
        Ok(())
    }

    /// Replaces the live prompt line without recording undo, using exact
    /// byte-splice geometry for extmarks (`f_prompt_setprompt`).
    pub(crate) fn replace_prompt_line(
        &mut self,
        lnum: usize,
        line: Vec<u8>,
        splice: TextSplice,
    ) -> Result<(), BufferStateError> {
        self.require_loaded()?;
        let before = self.text.line(lnum)?;
        self.text.replace_lines(lnum, lnum, &[line])?;
        self.bump_changedtick();
        // Prompt edits bypass undo but not attach callbacks: project the
        // same byte event from a synthetic single-line splice.
        let after = self.text.line(lnum)?;
        let edit = PreparedBufferTextEdit {
            start_line: lnum,
            before: vec![before],
            after: vec![after],
            splice,
        };
        let event = self.bytes_event(&edit)?;
        self.pending_bytes.push(event);
        self.marks.splice(lnum, 1, 1);
        let _ = self.extmarks.splice_recording(splice);
        self.splice_folds(lnum, 1, 1)?;
        self.splice_prompt_start(lnum, 1, 1);
        self.bump_derived_ticks();
        Ok(())
    }

    /// Replaces an inclusive line range, joining the open undo block or
    /// starting a new one.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not resident.
    /// Returns [`BufferStateError::Text`] when the range is invalid or a
    /// replacement line contains a newline or invalid UTF-8.
    pub fn replace_lines(
        &mut self,
        start: usize,
        end: usize,
        lines: &[Vec<u8>],
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.require_loaded()?;
        let before = (start..=end)
            .map(|line| self.text.line(line))
            .collect::<Result<Vec<_>, _>>()?;
        let after = lines.to_vec();
        let splice = TextSplice::line_anchored(start.saturating_sub(1), before.len(), after.len());
        let edit = PreparedBufferTextEdit {
            start_line: start,
            before,
            after,
            splice,
        };
        self.commit_recorded_splice(edit, cursor_before, cursor_after, timestamp)
    }

    /// Inserts logical lines after `after_lnum` with explicit undo cursors.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Text`] when the insertion point is outside
    /// the resident text or a supplied line is rejected by the rope.
    pub(crate) fn insert_lines(
        &mut self,
        after_lnum: usize,
        lines: &[Vec<u8>],
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        let after = lines.to_vec();
        let splice = TextSplice::line_anchored(after_lnum, 0, after.len());
        let edit = PreparedBufferTextEdit {
            start_line: after_lnum.saturating_add(1),
            before: Vec::new(),
            after,
            splice,
        };
        self.commit_validated_splice(edit, cursor_before, cursor_after, timestamp)
    }

    /// Inserts logical lines after `lnum`, joining the open undo block or
    /// starting a new one.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not resident.
    /// Returns [`BufferStateError::Text`] when `lnum` is outside the buffer or a
    /// supplied line contains a newline or invalid UTF-8.
    pub fn append_lines(
        &mut self,
        lnum: usize,
        lines: &[Vec<u8>],
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.require_loaded()?;
        self.insert_lines(
            lnum,
            lines,
            cursor,
            Position {
                lnum: cursor.lnum.saturating_add(lines.len()),
                col: cursor.col,
            },
            timestamp,
        )
    }

    /// Deletes an inclusive logical-line range, joining the open undo block
    /// or starting a new one.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not resident,
    /// or [`BufferStateError::Text`] when the range is invalid.
    pub fn delete_lines(
        &mut self,
        start: usize,
        end: usize,
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.replace_lines(start, end, &[], cursor, cursor, timestamp)
    }

    /// Creates or moves an extmark, binding complete point/range geometry to
    /// the last recorded splice when `bind_to_open_edit` is set.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtmarkError`] when the namespace is unknown, the range end
    /// precedes its start, or the render-order or namespace-local id space is
    /// exhausted.
    pub fn set_extmark_recorded(
        &mut self,
        namespace: NamespaceId,
        requested: Option<ExtmarkId>,
        placement: ExtmarkPlacement,
        bind_to_open_edit: bool,
    ) -> Result<ExtmarkId, ExtmarkError> {
        let previous = requested.and_then(|id| {
            self.extmarks.get(namespace, id).ok().flatten().map(|mark| {
                (
                    mark.position(),
                    mark.placement.end.map(|end| end.position),
                    mark.invalid,
                )
            })
        });
        let after_position = placement.position;
        let after_end = placement.end.map(|end| end.position);
        let id = self.extmarks.set(namespace, requested, placement)?;
        if bind_to_open_edit {
            self.retarget_recorded_set(namespace, id, after_position, after_end, previous);
        }
        Ok(id)
    }

    fn retarget_recorded_set(
        &mut self,
        namespace: NamespaceId,
        id: ExtmarkId,
        after_position: ExtmarkPosition,
        after_end: Option<ExtmarkPosition>,
        previous: Option<(ExtmarkPosition, Option<ExtmarkPosition>, bool)>,
    ) {
        let seq = self.undo.current_seq();
        let Some(undos) = self.extmark_undo.get_mut(&seq) else {
            return;
        };
        let Some(last) = undos.last_mut() else {
            return;
        };
        last.retarget_set(namespace, id, after_position, after_end, false, previous);
    }

    /// Replaces one validated byte range using `nvim_buf_set_text` semantics.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not resident.
    /// Returns [`BufferStateError::TextEdit`] when the requested range, byte
    /// boundaries, UTF-8, or replacement lines are invalid.
    pub fn replace_buffer_text(
        &mut self,
        request: &BufferTextEditRequest,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        let prepared = self.prepare_buffer_text_edit(request)?;
        self.commit_buffer_text_edit(prepared, cursor_before, cursor_after, timestamp)
    }

    pub(crate) fn prepare_buffer_text_edit(
        &self,
        request: &BufferTextEditRequest,
    ) -> Result<PreparedBufferTextEdit, BufferStateError> {
        self.require_loaded()?;
        let start = request.start;
        let end = request.end;
        let mut replacement = request.replacement.clone();
        if replacement.is_empty() {
            replacement.push(Vec::new());
        }

        if replacement.iter().any(|line| line.contains(&b'\n')) {
            return Err(BufferTextEditError::EmbeddedNewline.into());
        }
        if replacement
            .iter()
            .any(|line| std::str::from_utf8(line).is_err())
        {
            return Err(BufferTextEditError::InvalidUtf8.into());
        }

        if start.row > end.row {
            return Err(BufferTextEditError::ReversedRange.into());
        }

        let line_count = self.text.line_count();
        if start.row >= line_count || end.row >= line_count {
            return Err(BufferTextEditError::OutOfRange.into());
        }

        let start_line_bytes = self.text.line(start.row + 1)?;
        let end_line_bytes = if end.row == start.row {
            start_line_bytes.clone()
        } else {
            self.text.line(end.row + 1)?
        };
        if start.column > start_line_bytes.len() || end.column > end_line_bytes.len() {
            return Err(BufferTextEditError::OutOfRange.into());
        }

        if start.row == end.row && start.column > end.column {
            return Err(BufferTextEditError::ReversedRange.into());
        }

        if !is_utf8_boundary(&start_line_bytes, start.column) {
            return Err(BufferTextEditError::NotCharBoundary(start.column).into());
        }
        if !is_utf8_boundary(&end_line_bytes, end.column) {
            return Err(BufferTextEditError::NotCharBoundary(end.column).into());
        }

        let before = (start.row + 1..=end.row + 1)
            .map(|line| self.text.line(line))
            .collect::<Result<Vec<_>, _>>()?;
        let after = compose_replacement_lines(&before, start.column, end.column, &replacement);
        let splice = TextSplice::from_byte_edit(start, end, &replacement);
        Ok(PreparedBufferTextEdit {
            start_line: start.row + 1,
            before,
            after,
            splice,
        })
    }

    pub(crate) fn commit_buffer_text_edit(
        &mut self,
        prepared: PreparedBufferTextEdit,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.commit_validated_splice(prepared, cursor_before, cursor_after, timestamp)
    }

    /// Commits a splice whose inputs a prepare phase already validated, so
    /// the text layer cannot reject them; a rejection would mean the prepare
    /// and commit snapshots diverged, so the error propagates instead of any
    /// mutation or undo recording happening silently.
    fn commit_validated_splice(
        &mut self,
        edit: PreparedBufferTextEdit,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.commit_recorded_splice(edit, cursor_before, cursor_after, timestamp)
    }

    /// Undoes the most recent undo block, replaying the inverse of every edit
    /// it grouped through the text and mark pipeline with the changedtick
    /// advanced. Returns one entry per replayed edit, or `None` when already
    /// at the oldest change.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Text`] if a recorded line range cannot be
    /// replayed against the resident text.
    pub fn undo(&mut self) -> Result<Option<Vec<ReplayedEdit>>, BufferStateError> {
        let Ok(step) = self.undo.undo() else {
            return Ok(None);
        };
        self.apply_undo_step(&step).map(Some)
    }

    /// Redoes the next undo block, replaying each of its stored edits through
    /// the text and mark pipeline with the changedtick advanced. Returns one
    /// entry per replayed edit, or `None` when already at the newest change.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Text`] if a recorded line range cannot be
    /// replayed against the resident text.
    pub fn redo(&mut self) -> Result<Option<Vec<ReplayedEdit>>, BufferStateError> {
        let Ok(step) = self.undo.redo() else {
            return Ok(None);
        };
        self.apply_undo_step(&step).map(Some)
    }

    /// Navigates the undo tree to sequence `seq`, replaying every step the
    /// route needs, and returns them in application order, one inner vector
    /// per undo block.
    ///
    /// This is what `:undo {N}` needs (`undo_time` with `absolute`,
    /// `undo.c:1975`): the target may be behind *or* ahead of the current
    /// state, and may be on another branch, so it is not a run of one-step
    /// undos. `UndoTree::undo_to_seq` picks the route; this applies it.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Undo`] when the target sequence is unavailable,
    /// or [`BufferStateError::Text`] if a recorded line range cannot be replayed.
    pub fn undo_to_seq(&mut self, seq: u64) -> Result<Vec<Vec<ReplayedEdit>>, BufferStateError> {
        let steps = self.undo.undo_to_seq(seq)?;
        let mut replayed = Vec::with_capacity(steps.len());
        for step in steps {
            replayed.push(self.apply_undo_step(&step)?);
        }
        Ok(replayed)
    }

    /// Reopens the newest undo block so the next edit joins it (`:undojoin`).
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not resident,
    /// or [`BufferStateError::Undo`] when no prior undo block can be joined.
    pub fn undojoin(&mut self) -> Result<(), BufferStateError> {
        self.require_loaded()?;
        self.undo.undojoin().map_err(Into::into)
    }

    /// Closes the open undo block so the next edit starts a new one.
    ///
    /// This is `u_sync` (`undo.c:2704`). It is deliberately the only way for
    /// anything outside this module to move the boundary.
    pub fn sync_undo(&mut self) {
        self.undo.sync();
    }

    /// The buffer's undo position: the current header and how many edits it
    /// has collected so far.
    fn undo_state(&self) -> (u64, usize) {
        (self.undo.current_seq(), self.undo.current_block_len())
    }

    /// Replays one undo-tree step through text, marks, folds and extmarks.
    ///
    /// One owner for the direction-dependent parts so `undo`, `redo` and
    /// `undo_to_seq` cannot drift: an undo swaps `after` for `before`,
    /// walks the block's edits backwards and lands on the block's *pre*
    /// cursor, a redo does the reverse in recording order.
    fn apply_undo_step(&mut self, step: &UndoStep) -> Result<Vec<ReplayedEdit>, BufferStateError> {
        let (entry, undoing) = match step {
            UndoStep::Undo(entry) => (entry, true),
            UndoStep::Redo(entry) => (entry, false),
        };
        let count = entry.edits.len();
        let mut replayed = Vec::with_capacity(count);
        // One replayed block is one text change: the tick advances once for the
        // whole step, before any fold invalidation keys off it, exactly as the
        // forward batch path does.
        self.bump_changedtick();
        for offset in 0..count {
            // Undoing walks the block backwards, so the inverse of the last
            // edit applied is the first one undone.
            let index = if undoing { count - 1 - offset } else { offset };
            let edit = &entry.edits[index];
            let (remove, apply) = if undoing {
                (&edit.after, &edit.before)
            } else {
                (&edit.before, &edit.after)
            };
            let start_row = edit.start.saturating_sub(1);
            let (old_byte_size, deleted_codepoints, deleted_codeunits) =
                linewise_deleted_sizes(remove);
            let event = BufferBytesEvent {
                subscribers: self.subscriptions.keys().copied().collect(),
                tick: self.script_changedtick(),
                start_row,
                start_col: 0,
                start_byte: self.text.byte_of_line(edit.start)?,
                old_row: remove.len(),
                old_col: 0,
                old_byte: old_byte_size,
                new_row: apply.len(),
                new_col: 0,
                new_byte: apply.iter().map(|line| line.len() + 1).sum(),
                update: BufferUpdateKind::Mutation {
                    old_line_count: remove.len(),
                    old_byte_size,
                    deleted_codepoints,
                    deleted_codeunits,
                    new_lines: apply.clone(),
                },
            };
            self.replay_text(edit.start, remove, apply)?;
            self.pending_bytes.push(event);
            self.marks.splice(edit.start, remove.len(), apply.len());
            let recorded = self
                .extmark_undo
                .get(&entry.seq)
                .and_then(|undos| undos.get(index));
            if let Some(extmark_undo) = recorded {
                if undoing {
                    self.extmarks.undo_splice(extmark_undo);
                } else {
                    self.extmarks.redo_splice(extmark_undo);
                }
            } else {
                debug_assert!(
                    false,
                    "missing extmark undo record for seq {} member {}",
                    entry.seq, index
                );
            }
            self.splice_folds(edit.start, remove.len(), apply.len())?;
            let cursor = if undoing {
                edit.cursor_before
            } else {
                edit.cursor_after
            };
            replayed.push(ReplayedEdit {
                seq: entry.seq,
                start: edit.start,
                old_count: remove.len(),
                new_count: apply.len(),
                cursor: Position {
                    lnum: cursor.lnum,
                    col: cursor.col,
                },
            });
        }
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(replayed)
    }

    /// Applies `apply` in place of the `remove` lines currently at `start`,
    /// mirroring the buffer mutation used by direct edits without recording a
    /// new undo header (the tree already navigated).
    fn replay_text(
        &mut self,
        start: usize,
        remove: &[Vec<u8>],
        apply: &[Vec<u8>],
    ) -> Result<(), BufferStateError> {
        if remove.is_empty() {
            if !apply.is_empty() {
                self.text.append_lines(start.saturating_sub(1), apply)?;
            }
        } else {
            let end = start
                .checked_add(remove.len())
                .and_then(|line| line.checked_sub(1))
                .unwrap_or(start);
            self.text.replace_lines(start, end, apply)?;
        }
        Ok(())
    }

    /// Changes final-EOL state and advances every text-derived generation.
    ///
    /// `'modified'` is recomputed from the undo point and saved EOL state, so
    /// restoring the saved EOL (with no other pending edits) clears the flag
    /// again instead of latching it once changed.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Unloaded`] when the buffer text is not
    /// resident.
    pub fn set_eol(&mut self, has_eol: bool) -> Result<(), BufferStateError> {
        self.require_loaded()?;
        let changed = self.text.has_eol() != has_eol;
        self.text.set_eol(has_eol);
        if changed {
            self.bump_changedtick();
            self.pending_bytes.push(BufferBytesEvent::changedtick_for(
                self.subscriptions.keys().copied().collect(),
                self.script_changedtick(),
            ));
            self.folds.invalidate(self.changedtick());
        }
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(())
    }

    fn require_loaded(&self) -> Result<(), BufferStateError> {
        if self.residency.is_loaded() {
            Ok(())
        } else {
            Err(BufferStateError::Unloaded)
        }
    }

    fn release_resident_state(&mut self) {
        self.text = Buffer::new();
        self.bump_changedtick();
        self.undo = UndoTree::new();
        self.extmarks.invalidate_for_unload();
        self.extmark_undo.clear();
        self.folds = Folds::new();
        self.flags.set(BufferFlags::MODIFIED, false);
        self.saved_changedtick = self.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_state = (0, 0);
        self.pending_subscription_releases
            .extend(std::mem::take(&mut self.subscriptions).into_values());
        self.pending_bytes.clear();
    }

    /// Writes a prepared splice's lines into the resident text.
    ///
    /// # Errors
    ///
    /// Returns [`BufferStateError::Text`] when the rope rejects the line
    /// range or a replacement line.
    fn write_prepared_lines(
        &mut self,
        edit: &PreparedBufferTextEdit,
    ) -> Result<(), BufferStateError> {
        if edit.before.is_empty() {
            self.text
                .append_lines(edit.start_line.saturating_sub(1), &edit.after)?;
        } else {
            // Both operands are bounded by the in-memory line table, so the
            // inclusive end line cannot overflow.
            let end = edit.start_line + edit.before.len() - 1;
            self.text.replace_lines(edit.start_line, end, &edit.after)?;
        }
        Ok(())
    }

    fn commit_recorded_splice(
        &mut self,
        edit: PreparedBufferTextEdit,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.write_prepared_lines(&edit)?;
        self.bump_changedtick();
        // The byte offsets below address the pre-write text, whose line
        // prefix is unchanged by this edit; the tick is post-bump, which is
        // what the callback observes through `b:changedtick`.
        let event = self.bytes_event(&edit)?;
        self.pending_bytes.push(event);
        let seq = self.record_committed_splice(edit, cursor_before, cursor_after, timestamp)?;
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(seq)
    }

    /// Projects one committed splice onto the `on_bytes` argument shape.
    /// Must run after a successful write: the changedtick is already
    /// bumped, and the pre-write line prefix still addresses the change
    /// start. Fails only when the start line has no byte offset, which
    /// cannot happen for a splice the text layer just accepted.
    fn bytes_event(
        &self,
        edit: &PreparedBufferTextEdit,
    ) -> Result<BufferBytesEvent, BufferStateError> {
        let start = edit.splice.start;
        let old_end = edit.splice.old_end();
        let new_end = extent_end(start, edit.splice.new_extent);
        // One-based line of the change start in either text generation:
        // lines before it are untouched by this edit.
        let start_byte = self.text.byte_of_line(start.row + 1)? + start.column;
        let old_byte = span_bytes(&edit.before, start, old_end);
        let new_byte = span_bytes(&edit.after, start, new_end);
        let (old_byte_size, deleted_codepoints, deleted_codeunits) =
            linewise_deleted_sizes(&edit.before);
        Ok(BufferBytesEvent {
            subscribers: self.subscriptions.keys().copied().collect(),
            tick: self.script_changedtick(),
            start_row: start.row,
            start_col: start.column,
            start_byte,
            old_row: old_end.row.saturating_sub(start.row),
            old_col: end_column(old_end, start),
            old_byte,
            new_row: new_end.row.saturating_sub(start.row),
            new_col: end_column(new_end, start),
            new_byte,
            update: BufferUpdateKind::Mutation {
                old_line_count: edit.before.len(),
                old_byte_size,
                deleted_codepoints,
                deleted_codeunits,
                new_lines: edit.after.clone(),
            },
        })
    }

    fn record_committed_splice(
        &mut self,
        edit: PreparedBufferTextEdit,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.marks
            .splice(edit.start_line, edit.before.len(), edit.after.len());
        let (_, extmark_undo) = self.extmarks.splice_recording(edit.splice);
        self.splice_folds(edit.start_line, edit.before.len(), edit.after.len())?;
        self.splice_prompt_start(edit.start_line, edit.before.len(), edit.after.len());
        let seq = self.undo.record(
            LineEdit {
                start: edit.start_line,
                before: edit.before,
                after: edit.after,
                cursor_before: Cursor {
                    lnum: cursor_before.lnum,
                    col: cursor_before.col,
                },
                cursor_after: Cursor {
                    lnum: cursor_after.lnum,
                    col: cursor_after.col,
                },
            },
            timestamp,
        );
        self.extmark_undo.entry(seq).or_default().push(extmark_undo);
        Ok(seq)
    }

    pub(crate) fn commit_prepared_line_preserving_batch(
        &mut self,
        prepared: Vec<PreparedBufferTextEdit>,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        if prepared.is_empty() {
            return Ok(0);
        }
        let events = prepared
            .iter()
            .map(|edit| self.bytes_event(edit))
            .collect::<Result<Vec<_>, _>>()?;
        let splices: Vec<LineSplice<'_>> = prepared
            .iter()
            // The end line cannot overflow: both operands are bounded by the
            // in-memory line table.
            .map(|edit| LineSplice {
                start: edit.start_line,
                end: edit.start_line + edit.before.len() - 1,
                lines: &edit.after,
            })
            .collect();
        self.text
            .replace_lines_disjoint(&splices)
            .map_err(BufferStateError::from)?;
        self.bump_changedtick();
        for mut event in events {
            event.tick = self.script_changedtick();
            self.pending_bytes.push(event);
        }
        let mut seq = 0;
        for edit in prepared {
            debug_assert!(edit.preserves_line_count());
            seq = self.record_committed_splice(edit, cursor_before, cursor_after, timestamp)?;
        }
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(seq)
    }

    fn splice_folds(
        &mut self,
        start: usize,
        old_rows: usize,
        new_rows: usize,
    ) -> Result<(), BufferStateError> {
        self.folds
            .splice_rows(start.saturating_sub(1), old_rows, new_rows)?;
        self.folds.invalidate(self.changedtick());
        Ok(())
    }

    /// Adjusts the prompt row through the same line splice that moves marks.
    fn splice_prompt_start(&mut self, start: usize, old_count: usize, new_count: usize) {
        let old_end = start.saturating_add(old_count);
        self.prompt_start = if old_count == 0 && self.prompt_start >= start {
            self.prompt_start.saturating_add(new_count)
        } else if self.prompt_start >= old_end {
            self.prompt_start
                .saturating_sub(old_count)
                .saturating_add(new_count)
        } else if self.prompt_start >= start {
            start.saturating_add(
                self.prompt_start
                    .saturating_sub(start)
                    .min(new_count.saturating_sub(1)),
            )
        } else {
            self.prompt_start
        };
        self.prompt_start = self.prompt_start.clamp(1, self.text.line_count().max(1));
    }

    fn refresh_modified(&mut self) {
        self.flags.set(
            BufferFlags::MODIFIED,
            self.forced_modified
                || self.undo_state() != self.saved_undo_state
                || self.text.has_eol() != self.saved_has_eol,
        );
    }

    fn bump_derived_ticks(&mut self) {
        self.changedtick_diag = self.changedtick_diag.wrapping_add(1);
        self.changedtick_fold = self.changedtick_fold.wrapping_add(1);
    }

    fn bump_changedtick(&mut self) {
        self.changedtick = self.changedtick.wrapping_add(1);
    }
}

fn is_utf8_boundary(line: &[u8], col: usize) -> bool {
    col >= line.len() || line[col] & 0xC0 != 0x80
}

fn compose_replacement_lines(
    before: &[Vec<u8>],
    start_column: usize,
    end_column: usize,
    replacement: &[Vec<u8>],
) -> Vec<Vec<u8>> {
    debug_assert!(!before.is_empty());
    debug_assert!(!replacement.is_empty());
    if replacement.len() == 1 {
        let mut line = before[0][..start_column].to_vec();
        line.extend_from_slice(&replacement[0]);
        line.extend_from_slice(&before[before.len() - 1][end_column..]);
        return vec![line];
    }

    let mut after = Vec::with_capacity(replacement.len());
    let mut first = before[0][..start_column].to_vec();
    first.extend_from_slice(&replacement[0]);
    after.push(first);
    after.extend(replacement[1..replacement.len() - 1].iter().cloned());
    let mut last = replacement[replacement.len() - 1].clone();
    last.extend_from_slice(&before[before.len() - 1][end_column..]);
    after.push(last);
    after
}

#[cfg(test)]
mod tests {
    use ox_text::Buffer;

    use super::*;
    use crate::{Editor, EditorError, ExtmarkPosition, Geometry};

    fn position(lnum: usize, col: usize) -> Position {
        Position { lnum, col }
    }

    fn editor_with(text: &[u8]) -> (Editor, ox_types::BufHandle, ox_types::WinHandle) {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(text).unwrap(), true)
            .unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        (editor, buffer, window)
    }

    fn snapshot(editor: &Editor, buffer: ox_types::BufHandle) -> (Vec<u8>, u64, u64, bool, usize) {
        let state = editor.buffer(buffer).unwrap();
        (
            state.text().unwrap().to_bytes(),
            state.changedtick(),
            state.undo.current_seq(),
            state.flags.contains(crate::BufferFlags::MODIFIED),
            editor.changelists().len(buffer),
        )
    }

    #[test]
    fn prepare_rejects_lf_in_replacement_element() {
        let (mut editor, buffer, _) = editor_with(b"abc");
        let before = snapshot(&editor, buffer);
        let error = editor
            .replace_buffer_text(
                buffer,
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(0, 0),
                    end: ExtmarkPosition::new(0, 1),
                    replacement: vec![b"a\nb".to_vec()],
                },
                position(1, 0),
                position(1, 0),
                1,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            EditorError::Buffer(BufferStateError::TextEdit(
                BufferTextEditError::EmbeddedNewline
            ))
        ));
        assert_eq!(snapshot(&editor, buffer), before);
    }

    #[test]
    fn prepare_rejects_invalid_utf8_replacement() {
        let (mut editor, buffer, _) = editor_with(b"abc");
        let before = snapshot(&editor, buffer);
        let error = editor
            .replace_buffer_text(
                buffer,
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(0, 0),
                    end: ExtmarkPosition::new(0, 1),
                    replacement: vec![vec![0xff, 0xfe]],
                },
                position(1, 0),
                position(1, 0),
                1,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            EditorError::Buffer(BufferStateError::TextEdit(BufferTextEditError::InvalidUtf8))
        ));
        assert_eq!(snapshot(&editor, buffer), before);
    }

    #[test]
    fn prepare_accepts_embedded_cr() {
        let (mut editor, buffer, _) = editor_with(b"abc");
        editor
            .replace_buffer_text(
                buffer,
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(0, 1),
                    end: ExtmarkPosition::new(0, 2),
                    replacement: vec![vec![b'\r']],
                },
                position(1, 0),
                position(1, 0),
                1,
            )
            .unwrap();
        assert_eq!(
            editor.buffer(buffer).unwrap().text().unwrap().to_bytes(),
            b"a\rc"
        );
    }

    #[test]
    fn batch_prepare_failure_is_atomic() {
        let (mut editor, buffer, window) = editor_with(b"abc\ndef");
        let before = snapshot(&editor, buffer);
        let error = editor
            .replace_buffer_texts(
                buffer,
                window,
                &[
                    BufferTextEditRequest {
                        start: ExtmarkPosition::new(0, 0),
                        end: ExtmarkPosition::new(0, 1),
                        replacement: vec![b"X".to_vec()],
                    },
                    BufferTextEditRequest {
                        start: ExtmarkPosition::new(1, 0),
                        end: ExtmarkPosition::new(1, 1),
                        replacement: vec![b"Y\nZ".to_vec()],
                    },
                ],
                position(1, 0),
                position(1, 0),
                1,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            EditorError::Buffer(BufferStateError::TextEdit(
                BufferTextEditError::EmbeddedNewline
            ))
        ));
        assert_eq!(snapshot(&editor, buffer), before);
    }

    fn cursor(lnum: usize, col: usize) -> Position {
        Position { lnum, col }
    }

    fn range_tuple(
        state: &BufferState,
        namespace: crate::NamespaceId,
        id: crate::ExtmarkId,
    ) -> (usize, usize, Option<(usize, usize)>, bool) {
        let mark = state.extmarks.get(namespace, id).unwrap().unwrap();
        (
            mark.position().row,
            mark.position().column,
            mark.placement
                .end
                .map(|end| (end.position.row, end.position.column)),
            mark.invalid,
        )
    }

    #[test]
    fn undo_redo_extmark_created_during_edit_restores_range() {
        let mut state = BufferState::new(Buffer::from_bytes(b"").unwrap(), true);
        state
            .replace_buffer_text(
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(0, 0),
                    end: ExtmarkPosition::new(0, 0),
                    replacement: vec![b"foobar".to_vec()],
                },
                cursor(1, 0),
                cursor(1, 6),
                1,
            )
            .unwrap();
        let namespace = state.extmarks.create_namespace("during-edit").unwrap();
        let id = crate::ExtmarkId::new(1).unwrap();
        let mut invalidate = crate::ExtmarkPlacement::new(ExtmarkPosition::new(0, 3))
            .with_end(ExtmarkPosition::new(0, 6));
        invalidate
            .attributes
            .flags
            .set(crate::ExtmarkFlags::INVALIDATE, true);
        state
            .set_extmark_recorded(
                namespace,
                Some(id),
                crate::ExtmarkPlacement::new(ExtmarkPosition::new(0, 3))
                    .with_end(ExtmarkPosition::new(0, 6)),
                true,
            )
            .unwrap();
        let invalidated = crate::ExtmarkId::new(2).unwrap();
        state
            .set_extmark_recorded(namespace, Some(invalidated), invalidate, true)
            .unwrap();
        state.sync_undo();
        assert_eq!(
            range_tuple(&state, namespace, id),
            (0, 3, Some((0, 6)), false)
        );
        state.undo().unwrap();
        assert_eq!(
            range_tuple(&state, namespace, id),
            (0, 0, Some((0, 0)), false)
        );
        assert_eq!(
            range_tuple(&state, namespace, invalidated),
            (0, 0, Some((0, 0)), true)
        );
        state.redo().unwrap();
        assert_eq!(
            range_tuple(&state, namespace, id),
            (0, 3, Some((0, 6)), false)
        );
        assert_eq!(
            range_tuple(&state, namespace, invalidated),
            (0, 3, Some((0, 6)), false)
        );
    }

    #[test]
    fn undo_redo_extmark_moved_during_edit_restores_explicit_point() {
        let mut state = BufferState::new(Buffer::from_bytes(b"abcdef").unwrap(), true);
        let namespace = state.extmarks.create_namespace("during-edit-move").unwrap();
        let id = crate::ExtmarkId::new(1).unwrap();
        state
            .set_extmark_recorded(
                namespace,
                Some(id),
                crate::ExtmarkPlacement::new(ExtmarkPosition::new(0, 4)),
                false,
            )
            .unwrap();
        state
            .replace_buffer_text(
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(0, 0),
                    end: ExtmarkPosition::new(0, 0),
                    replacement: vec![b"!!".to_vec()],
                },
                cursor(1, 0),
                cursor(1, 2),
                1,
            )
            .unwrap();
        state
            .set_extmark_recorded(
                namespace,
                Some(id),
                crate::ExtmarkPlacement::new(ExtmarkPosition::new(0, 1)),
                true,
            )
            .unwrap();
        state.sync_undo();
        assert_eq!(range_tuple(&state, namespace, id), (0, 1, None, false));
        state.undo().unwrap();
        assert_eq!(range_tuple(&state, namespace, id), (0, 4, None, false));
        state.redo().unwrap();
        assert_eq!(range_tuple(&state, namespace, id), (0, 1, None, false));
    }

    #[test]
    fn commit_records_bytes_event_for_line_replace() {
        let (mut editor, buffer, _) = editor_with(b"a\nb\nc\n");
        let state = editor.buffer_mut(buffer).unwrap();
        state
            .replace_lines(2, 2, &[b"XY".to_vec()], position(1, 0), position(2, 2), 0)
            .unwrap();
        let events = state.take_bytes_events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(
            (event.start_row, event.start_col, event.start_byte),
            (1, 0, 2)
        );
        // Full-line spans use the extent shape (terminator included):
        // replacing "b" with "XY" reports old=(1,0,2), new=(1,0,3),
        // matching the reference `on_bytes` for the same edit.
        assert_eq!((event.old_row, event.old_col, event.old_byte), (1, 0, 2));
        assert_eq!((event.new_row, event.new_col, event.new_byte), (1, 0, 3));
        assert_eq!(event.tick, state.script_changedtick());
        assert!(state.take_bytes_events().is_empty());
    }

    #[test]
    fn commit_records_bytes_event_for_line_insert() {
        let (mut editor, buffer, _) = editor_with(b"a\nb\n");
        let state = editor.buffer_mut(buffer).unwrap();
        state
            .insert_lines(0, &[b"Z".to_vec()], position(1, 0), position(2, 0), 0)
            .unwrap();
        let events = state.take_bytes_events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(
            (event.start_row, event.start_col, event.start_byte),
            (0, 0, 0)
        );
        assert_eq!((event.old_row, event.old_col, event.old_byte), (0, 0, 0));
        // Inserting one line reports the added line plus its newline.
        assert_eq!((event.new_row, event.new_col, event.new_byte), (1, 0, 2));
    }

    #[test]
    fn undo_and_redo_emit_bytes_events() {
        let (mut editor, buffer, _) = editor_with(b"a\nb\nc\n");
        let state = editor.buffer_mut(buffer).unwrap();
        state
            .replace_lines(2, 2, &[b"XY".to_vec()], position(1, 0), position(2, 2), 0)
            .unwrap();

        let forward = state.take_bytes_events();
        assert_eq!(forward.len(), 1);
        assert_eq!(
            (
                forward[0].start_row,
                forward[0].start_col,
                forward[0].start_byte
            ),
            (1, 0, 2)
        );

        state.undo().unwrap();
        let undo = state.take_bytes_events();
        assert_eq!(undo.len(), 1);
        let event = &undo[0];
        assert_eq!(
            (event.start_row, event.start_col, event.start_byte),
            (1, 0, 2)
        );
        // Undo removes "XY" (2 bytes + newline = 3) and inserts "b" (1 byte + newline = 2).
        assert_eq!((event.old_row, event.old_col, event.old_byte), (1, 0, 3));
        assert_eq!((event.new_row, event.new_col, event.new_byte), (1, 0, 2));

        state.redo().unwrap();
        let redo = state.take_bytes_events();
        assert_eq!(redo.len(), 1);
        let event = &redo[0];
        assert_eq!(
            (event.start_row, event.start_col, event.start_byte),
            (1, 0, 2)
        );
        // Redo does the inverse: removes "b" and inserts "XY".
        assert_eq!((event.old_row, event.old_col, event.old_byte), (1, 0, 2));
        assert_eq!((event.new_row, event.new_col, event.new_byte), (1, 0, 3));
    }

    #[test]
    fn line_preserving_batch_reports_pre_edit_offsets() {
        let (mut editor, buffer, window) = editor_with(b"a\nb\nc\n");
        let requests = &[
            BufferTextEditRequest {
                start: ExtmarkPosition::new(0, 0),
                end: ExtmarkPosition::new(0, 1),
                replacement: vec![b"longer".to_vec()],
            },
            BufferTextEditRequest {
                start: ExtmarkPosition::new(1, 0),
                end: ExtmarkPosition::new(1, 1),
                replacement: vec![b"also longer".to_vec()],
            },
            BufferTextEditRequest {
                start: ExtmarkPosition::new(2, 0),
                end: ExtmarkPosition::new(2, 1),
                replacement: vec![b"c2".to_vec()],
            },
        ];
        editor
            .replace_buffer_texts(buffer, window, requests, position(1, 0), position(1, 0), 0)
            .unwrap();

        let state = editor.buffer_mut(buffer).unwrap();
        let events = state.take_bytes_events();
        assert_eq!(events.len(), 3);
        // Offsets must be pre-edit: 0 for line 1, 2 for line 2, 4 for line 3.
        assert_eq!((events[0].start_row, events[0].start_byte), (0, 0));
        assert_eq!((events[1].start_row, events[1].start_byte), (1, 2));
        assert_eq!((events[2].start_row, events[2].start_byte), (2, 4));

        // Sanity check the batch really changed all three lines.
        let text = state.text().unwrap().to_bytes();
        assert_eq!(text, b"longer\nalso longer\nc2\n");
    }
}
