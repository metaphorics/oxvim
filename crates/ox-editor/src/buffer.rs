//! Buffer ownership, text mutation, and buflist lifecycle.

use std::collections::BTreeMap;

use ox_text::buffer::LineSplice;
use ox_text::{Buffer, BufferError, Cursor, LineEdit, Position, UndoError, UndoStep, UndoTree};
use ox_types::{Dict, Object, OxStr};
use thiserror::Error;

use crate::marks::LocalMarks;
use crate::{Extmarks, Folds};
use crate::extmark::{
    ExtmarkError, ExtmarkId, ExtmarkPlacement, ExtmarkPosition, ExtmarkSpliceUndo, TextSplice,
};
use crate::NamespaceId;
use crate::fold::FoldError;

/// A buffer-local user command retained for later command execution.
#[derive(Clone, Debug, PartialEq)]
pub struct UserCommandDefinition {
    /// Command body or callable reference supplied through the API.
    pub command: Object,
    /// API options retained with the definition.
    pub options: Dict,
}

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
    /// Raw replacement lines under nvim_buf_set_text semantics.
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

/// Text and buffer-local state owned by [`crate::Editor`].
#[derive(Clone, Debug)]
pub struct BufferState {
    /// Rope-backed text.
    text: Buffer,
    /// API-visible buffer name as uninterpreted bytes.
    name: OxStr,
    /// Buffer-local API variables in insertion order.
    variables: Dict,
    /// Buffer-local user commands keyed by command name.
    user_commands: BTreeMap<OxStr, UserCommandDefinition>,
    /// Attached RPC channels keyed by channel identity.
    subscriptions: BTreeMap<u64, BufferAttachSubscription>,
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
    /// Whether resident text differs from the last saved undo state.
    pub modified: bool,
    /// Read-only policy data; command layers decide whether to raise E37/E89-class errors.
    pub readonly: bool,
    /// Prefix currently owned by prompt-buffer input (`b_prompt_text`).
    prompt: Vec<u8>,
    /// One-based line containing the live prompt (`b_prompt_start.mark.lnum`).
    prompt_start: usize,
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
    /// Whether the buffer appears in the buffer list.
    pub listed: bool,
    /// Whether text and undo state are resident.
    pub loaded: bool,
    /// Whether a loaded buffer currently has no window.
    pub hidden: bool,
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
        let saved_changedtick = text.changedtick();
        let saved_has_eol = text.has_eol();
        let prompt_start = text.line_count().max(1);
        Self {
            text,
            name: OxStr::from(""),
            variables: Dict(Vec::new()),
            user_commands: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            undo: UndoTree::new(),
            marks: LocalMarks::new(),
            extmarks: Extmarks::new(),
            extmark_undo: BTreeMap::new(),
            folds: Folds::new(),
            modified: false,
            readonly: false,
            prompt: Vec::new(),
            prompt_start,
            saved_changedtick,
            saved_has_eol,
            saved_undo_state: (0, 0),
            listed,
            loaded: true,
            hidden: true,
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
    pub const fn variables_mut(&mut self) -> &mut Dict {
        &mut self.variables
    }

    /// Returns stored buffer-local user commands.
    #[must_use]
    pub const fn user_commands(&self) -> &BTreeMap<OxStr, UserCommandDefinition> {
        &self.user_commands
    }

    /// Returns mutable stored buffer-local user commands.
    pub const fn user_commands_mut(
        &mut self,
    ) -> &mut BTreeMap<OxStr, UserCommandDefinition> {
        &mut self.user_commands
    }

    /// Returns requested buffer event subscriptions.
    #[must_use]
    pub const fn subscriptions(&self) -> &BTreeMap<u64, BufferAttachSubscription> {
        &self.subscriptions
    }

    /// Returns mutable requested buffer event subscriptions.
    pub const fn subscriptions_mut(
        &mut self,
    ) -> &mut BTreeMap<u64, BufferAttachSubscription> {
        &mut self.subscriptions
    }

    /// Returns the text change counter maintained by `ox-text`.
    #[must_use]
    pub const fn changedtick(&self) -> u64 {
        self.text.changedtick()
    }

    /// Returns the text generation recorded at the last successful save.
    #[must_use]
    pub const fn saved_changedtick(&self) -> u64 {
        self.saved_changedtick
    }

    /// Records the current undo state as saved and clears `'modified'`.
    ///
    /// Neovim marks the active undo branch unchanged after writing so that
    /// undoing away from, or returning to, that point restores the flag
    /// (`src/nvim/undo.c:2818-2824`, `src/nvim/bufwrite.c:1727-1738`).
    pub fn mark_saved(&mut self) {
        self.saved_changedtick = self.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_state = self.undo_state();
        self.modified = false;
    }

    /// Returns resident text, or an unloaded-state error.
    pub fn text(&self) -> Result<&Buffer, BufferStateError> {
        if self.loaded {
            Ok(&self.text)
        } else {
            Err(BufferStateError::Unloaded)
        }
    }

    /// Replaces unloaded resident text before a window attaches.
    pub fn load(&mut self, text: Buffer) {
        self.text = text;
        self.prompt_start = self.prompt_start.clamp(1, self.text.line_count().max(1));
        self.undo = UndoTree::new();
        self.extmarks = Extmarks::new();
        self.extmark_undo.clear();
        self.folds = Folds::new();
        self.modified = false;
        self.saved_changedtick = self.text.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_state = (0, 0);
        self.loaded = true;
        self.hidden = self.attachments == 0;
    }

    /// Attaches one window to resident text.
    pub fn attach(&mut self) -> Result<(), BufferStateError> {
        self.require_loaded()?;
        self.attachments = self.attachments.saturating_add(1);
        self.hidden = false;
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
        self.hidden = keep_loaded;
        self.loaded = keep_loaded;
        if !keep_loaded {
            self.release_resident_state();
        }
    }

    /// Unloads text state when no window displays the buffer.
    pub fn unload(&mut self) -> Result<(), BufferStateError> {
        if self.attachments != 0 {
            return Err(BufferStateError::Attached(self.attachments));
        }
        self.loaded = false;
        self.hidden = false;
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
        self.text.replace_lines(lnum, lnum, &[line])?;
        self.marks.splice(lnum, 1, 1);
        let _ = self.extmarks.splice_recording(splice);
        self.splice_folds(lnum, 1, 1);
        self.splice_prompt_start(lnum, 1, 1);
        self.bump_derived_ticks();
        Ok(())
    }

    /// Replaces an inclusive line range, joining the open undo block or
    /// starting a new one.
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
        Ok(self.commit_recorded_splice(
            start,
            before,
            after,
            splice,
            cursor_before,
            cursor_after,
            timestamp,
        ))
    }

    /// Inserts logical lines after `after_lnum` with explicit undo cursors.
    pub(crate) fn insert_lines(
        &mut self,
        after_lnum: usize,
        lines: &[Vec<u8>],
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> u64 {
        let start = after_lnum.saturating_add(1);
        let after = lines.to_vec();
        let splice = TextSplice::line_anchored(after_lnum, 0, after.len());
        self.commit_recorded_splice(
            start,
            Vec::new(),
            after,
            splice,
            cursor_before,
            cursor_after,
            timestamp,
        )
    }

    /// Inserts logical lines after `lnum`, joining the open undo block or
    /// starting a new one.
    pub fn append_lines(
        &mut self,
        lnum: usize,
        lines: &[Vec<u8>],
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.require_loaded()?;
        Ok(self.insert_lines(
            lnum,
            lines,
            cursor,
            Position {
                lnum: cursor.lnum.saturating_add(lines.len()),
                col: cursor.col,
            },
            timestamp,
        ))
    }

    /// Deletes an inclusive logical-line range, joining the open undo block
    /// or starting a new one.
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

    /// Replaces one validated byte range using nvim_buf_set_text semantics.
    pub fn replace_buffer_text(
        &mut self,
        request: &BufferTextEditRequest,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        let prepared = self.prepare_buffer_text_edit(request)?;
        Ok(self.commit_buffer_text_edit(prepared, cursor_before, cursor_after, timestamp))
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
        if replacement.iter().any(|line| std::str::from_utf8(line).is_err()) {
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
    ) -> u64 {
        self.commit_recorded_splice(
            prepared.start_line,
            prepared.before,
            prepared.after,
            prepared.splice,
            cursor_before,
            cursor_after,
            timestamp,
        )
    }

    /// Undoes the most recent undo block, replaying the inverse of every edit
    /// it grouped through the text and mark pipeline with the changedtick
    /// advanced. Returns one entry per replayed edit, or `None` when already
    /// at the oldest change.
    pub fn undo(&mut self) -> Result<Option<Vec<ReplayedEdit>>, BufferStateError> {
        let Ok(step) = self.undo.undo() else {
            return Ok(None);
        };
        self.apply_undo_step(&step).map(Some)
    }

    /// Redoes the next undo block, replaying each of its stored edits through
    /// the text and mark pipeline with the changedtick advanced. Returns one
    /// entry per replayed edit, or `None` when already at the newest change.
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
    pub fn undo_to_seq(
        &mut self,
        seq: u64,
    ) -> Result<Vec<Vec<ReplayedEdit>>, BufferStateError> {
        let steps = self.undo.undo_to_seq(seq)?;
        let mut replayed = Vec::with_capacity(steps.len());
        for step in steps {
            replayed.push(self.apply_undo_step(&step)?);
        }
        Ok(replayed)
    }

    /// Reopens the newest undo block so the next edit joins it (`:undojoin`).
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
    fn apply_undo_step(
        &mut self,
        step: &UndoStep,
    ) -> Result<Vec<ReplayedEdit>, BufferStateError> {
        let (entry, undoing) = match step {
            UndoStep::Undo(entry) => (entry, true),
            UndoStep::Redo(entry) => (entry, false),
        };
        let count = entry.edits.len();
        let mut replayed = Vec::with_capacity(count);
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
            self.replay_text(edit.start, remove, apply)
                .expect("recorded undo ranges are valid by construction");
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
                    entry.seq,
                    index
                );
            }
            self.splice_folds(edit.start, remove.len(), apply.len());
            let cursor = if undoing { edit.cursor_before } else { edit.cursor_after };
            replayed.push(ReplayedEdit {
                seq: entry.seq,
                start: edit.start,
                old_count: remove.len(),
                new_count: apply.len(),
                cursor: Position { lnum: cursor.lnum, col: cursor.col },
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
    pub fn set_eol(&mut self, has_eol: bool) -> Result<(), BufferStateError> {
        self.require_loaded()?;
        let changed = self.text.has_eol() != has_eol;
        self.text.set_eol(has_eol);
        if changed {
            self.folds.invalidate(self.changedtick());
        }
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(())
    }

    fn require_loaded(&self) -> Result<(), BufferStateError> {
        if self.loaded {
            Ok(())
        } else {
            Err(BufferStateError::Unloaded)
        }
    }

    fn release_resident_state(&mut self) {
        self.text = Buffer::new();
        self.undo = UndoTree::new();
        self.extmarks.invalidate_for_unload();
        self.extmark_undo.clear();
        self.folds = Folds::new();
        self.modified = false;
        self.saved_changedtick = self.text.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_state = (0, 0);
        self.subscriptions.clear();
    }

    fn commit_recorded_splice(
        &mut self,
        start_line: usize,
        before: Vec<Vec<u8>>,
        after: Vec<Vec<u8>>,
        splice: TextSplice,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> u64 {
        if before.is_empty() {
            self.text
                .append_lines(start_line.saturating_sub(1), &after)
                .expect("prepared append range is valid by construction");
        } else {
            let end = start_line
                .checked_add(before.len())
                .and_then(|line| line.checked_sub(1))
                .expect("prepared line range is valid by construction");
            self.text
                .replace_lines(start_line, end, &after)
                .expect("prepared line range is valid by construction");
        }
        let seq = self.record_committed_splice(
            start_line,
            before,
            after,
            splice,
            cursor_before,
            cursor_after,
            timestamp,
        );
        self.refresh_modified();
        self.bump_derived_ticks();
        seq
    }

    fn record_committed_splice(
        &mut self,
        start_line: usize,
        before: Vec<Vec<u8>>,
        after: Vec<Vec<u8>>,
        splice: TextSplice,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> u64 {
        self.marks.splice(start_line, before.len(), after.len());
        let (_, extmark_undo) = self.extmarks.splice_recording(splice);
        self.splice_folds(start_line, before.len(), after.len());
        self.splice_prompt_start(start_line, before.len(), after.len());
        let seq = self.undo.record(
            LineEdit {
                start: start_line,
                before,
                after,
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
        seq
    }

    pub(crate) fn commit_prepared_line_preserving_batch(
        &mut self,
        prepared: Vec<PreparedBufferTextEdit>,
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> u64 {
        if prepared.is_empty() {
            return 0;
        }
        let splices: Vec<LineSplice<'_>> = prepared
            .iter()
            .map(|edit| {
                let end = edit
                    .start_line
                    .checked_add(edit.before.len())
                    .and_then(|line| line.checked_sub(1))
                    .expect("prepared line range is valid by construction");
                LineSplice {
                    start: edit.start_line,
                    end,
                    lines: &edit.after,
                }
            })
            .collect();
        self.text
            .replace_lines_disjoint(&splices)
            .expect("prepared line range is valid by construction");
        let mut seq = 0;
        for edit in prepared {
            debug_assert!(edit.preserves_line_count());
            seq = self.record_committed_splice(
                edit.start_line,
                edit.before,
                edit.after,
                edit.splice,
                cursor_before,
                cursor_after,
                timestamp,
            );
        }
        self.refresh_modified();
        self.bump_derived_ticks();
        seq
    }

    fn splice_folds(&mut self, start: usize, old_rows: usize, new_rows: usize) {
        self.folds
            .splice_rows(start.saturating_sub(1), old_rows, new_rows);
        self.folds.invalidate(self.changedtick());
    }

    /// Adjusts the prompt row through the same line splice that moves marks.
    fn splice_prompt_start(&mut self, start: usize, old_count: usize, new_count: usize) {
        let old_end = start.saturating_add(old_count);
        self.prompt_start = if old_count == 0 && self.prompt_start >= start {
            self.prompt_start.saturating_add(new_count)
        } else if self.prompt_start >= old_end {
            self.prompt_start.saturating_sub(old_count).saturating_add(new_count)
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
        self.modified = self.undo_state() != self.saved_undo_state
            || self.text.has_eol() != self.saved_has_eol;
    }

    fn bump_derived_ticks(&mut self) {
        self.changedtick_diag = self.changedtick_diag.wrapping_add(1);
        self.changedtick_fold = self.changedtick_fold.wrapping_add(1);
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
            state.modified,
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
            mark.placement.end.map(|end| (end.position.row, end.position.column)),
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
        invalidate.attributes.invalidate = true;
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
        assert_eq!(range_tuple(&state, namespace, id), (0, 3, Some((0, 6)), false));
        state.undo().unwrap();
        assert_eq!(range_tuple(&state, namespace, id), (0, 0, Some((0, 0)), false));
        assert_eq!(
            range_tuple(&state, namespace, invalidated),
            (0, 0, Some((0, 0)), true)
        );
        state.redo().unwrap();
        assert_eq!(range_tuple(&state, namespace, id), (0, 3, Some((0, 6)), false));
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
}
