//! Buffer ownership, text mutation, and buflist lifecycle.

use std::collections::BTreeMap;

use ox_text::{Buffer, BufferError, Cursor, LineEdit, Position, UndoStep, UndoTree};
use ox_types::{Dict, Object, OxStr};
use thiserror::Error;

use crate::marks::LocalMarks;
use crate::{Extmarks, Folds};
use crate::extmark::{ExtmarkError, ExtmarkPosition, TextExtent};
use crate::extmark::ExtmarkSpliceUndo;
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
    /// Compact extmark position deltas keyed by undo sequence.
    extmark_undo: BTreeMap<u64, ExtmarkSpliceUndo>,
    /// Lazily computed and manual buffer folds.
    pub folds: Folds,
    /// Whether resident text differs from the last saved undo state.
    pub modified: bool,
    /// Read-only policy data; command layers decide whether to raise E37/E89-class errors.
    pub readonly: bool,
    /// Text changedtick observed when the buffer was last marked saved.
    saved_changedtick: u64,
    /// Final-EOL state at the last save.
    saved_has_eol: bool,
    /// Undo-tree state corresponding to the last saved contents.
    saved_undo_seq: u64,
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
            saved_changedtick,
            saved_has_eol,
            saved_undo_seq: 0,
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
        self.saved_undo_seq = self.undo.current_seq();
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
        self.undo = UndoTree::new();
        self.extmarks = Extmarks::new();
        self.extmark_undo.clear();
        self.folds = Folds::new();
        self.modified = false;
        self.saved_changedtick = self.text.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_seq = 0;
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

    /// Replaces an inclusive line range and records one undo header.
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
        self.text.replace_lines(start, end, lines)?;
        self.marks.splice(start, before.len(), lines.len());
        let extmark_undo = self.splice_derived_state(start, before.len(), lines.len())?;
        let seq = self.undo.record(
            LineEdit {
                start,
                before,
                after: lines.to_vec(),
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
        self.extmark_undo.insert(seq, extmark_undo);
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(seq)
    }

    /// Inserts logical lines after `lnum` and records one undo header.
    pub fn append_lines(
        &mut self,
        lnum: usize,
        lines: &[Vec<u8>],
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.require_loaded()?;
        self.text.append_lines(lnum, lines)?;
        self.marks.splice(lnum.saturating_add(1), 0, lines.len());
        let extmark_undo =
            self.splice_derived_state(lnum.saturating_add(1), 0, lines.len())?;
        let seq = self.undo.record(
            LineEdit {
                start: lnum.saturating_add(1),
                before: Vec::new(),
                after: lines.to_vec(),
                cursor_before: Cursor {
                    lnum: cursor.lnum,
                    col: cursor.col,
                },
                cursor_after: Cursor {
                    lnum: cursor.lnum.saturating_add(lines.len()),
                    col: cursor.col,
                },
            },
            timestamp,
        );
        self.extmark_undo.insert(seq, extmark_undo);
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(seq)
    }

    /// Deletes an inclusive logical-line range and records one undo header.
    pub fn delete_lines(
        &mut self,
        start: usize,
        end: usize,
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, BufferStateError> {
        self.replace_lines(start, end, &[], cursor, cursor, timestamp)
    }

    /// Undoes the most recent change, replaying its inverse edit through the
    /// text and mark pipeline with the changedtick advanced. Returns the
    /// undone header's sequence, or `None` when already at the oldest change.
    pub fn undo(&mut self) -> Result<Option<ReplayedEdit>, BufferStateError> {
        let Ok(UndoStep::Undo(entry)) = self.undo.undo() else {
            return Ok(None);
        };
        let edit = &entry.edit;
        self.replay_text(edit.start, &edit.after, &edit.before)?;
        self.marks
            .splice(edit.start, edit.after.len(), edit.before.len());
        self.splice_folds(edit.start, edit.after.len(), edit.before.len())?;
        if let Some(extmark_undo) = self.extmark_undo.get(&entry.seq) {
            self.extmarks.undo_splice(
                extmark_undo,
                ExtmarkPosition::new(edit.start.saturating_sub(1), 0),
                TextExtent::new(edit.before.len(), 0),
                TextExtent::new(edit.after.len(), 0),
            )?;
        } else {
            self.extmarks.splice(
                ExtmarkPosition::new(edit.start.saturating_sub(1), 0),
                TextExtent::new(edit.after.len(), 0),
                TextExtent::new(edit.before.len(), 0),
            )?;
        }
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(Some(ReplayedEdit {
            seq: entry.seq,
            start: edit.start,
            old_count: edit.after.len(),
            new_count: edit.before.len(),
            cursor: Position {
                lnum: edit.cursor_before.lnum,
                col: edit.cursor_before.col,
            },
        }))
    }

    /// Redoes the next change, replaying its stored edit through the text and
    /// mark pipeline with the changedtick advanced. Returns the redone
    /// header's sequence, or `None` when already at the newest change.
    pub fn redo(&mut self) -> Result<Option<ReplayedEdit>, BufferStateError> {
        let Ok(UndoStep::Redo(entry)) = self.undo.redo() else {
            return Ok(None);
        };
        let edit = &entry.edit;
        self.replay_text(edit.start, &edit.before, &edit.after)?;
        self.marks
            .splice(edit.start, edit.before.len(), edit.after.len());
        let _ = self.splice_derived_state(edit.start, edit.before.len(), edit.after.len())?;
        self.refresh_modified();
        self.bump_derived_ticks();
        Ok(Some(ReplayedEdit {
            seq: entry.seq,
            start: edit.start,
            old_count: edit.before.len(),
            new_count: edit.after.len(),
            cursor: Position {
                lnum: edit.cursor_after.lnum,
                col: edit.cursor_after.col,
            },
        }))
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
    pub fn set_eol(&mut self, has_eol: bool) -> Result<(), BufferStateError> {
        self.require_loaded()?;
        let changed = self.text.has_eol() != has_eol;
        self.text.set_eol(has_eol);
        if changed {
            self.modified = true;
            self.folds.invalidate(self.changedtick());
        }
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
        self.extmarks = Extmarks::new();
        self.extmark_undo.clear();
        self.folds = Folds::new();
        self.modified = false;
        self.saved_changedtick = self.text.changedtick();
        self.saved_has_eol = self.text.has_eol();
        self.saved_undo_seq = 0;
        self.subscriptions.clear();
    }

    fn splice_derived_state(
        &mut self,
        start: usize,
        old_rows: usize,
        new_rows: usize,
    ) -> Result<ExtmarkSpliceUndo, BufferStateError> {
        let (_, undo) = self.extmarks.splice_recording(
            ExtmarkPosition::new(start.saturating_sub(1), 0),
            TextExtent::new(old_rows, 0),
            TextExtent::new(new_rows, 0),
        )?;
        self.splice_folds(start, old_rows, new_rows)?;
        Ok(undo)
    }

    fn splice_folds(
        &mut self,
        start: usize,
        old_rows: usize,
        new_rows: usize,
    ) -> Result<(), FoldError> {
        self.folds
            .splice_rows(start.saturating_sub(1), old_rows, new_rows)?;
        self.folds.invalidate(self.changedtick());
        Ok(())
    }

    fn refresh_modified(&mut self) {
        self.modified = self.undo.current_seq() != self.saved_undo_seq
            || self.text.has_eol() != self.saved_has_eol;
    }

    fn bump_derived_ticks(&mut self) {
        self.changedtick_diag = self.changedtick_diag.wrapping_add(1);
        self.changedtick_fold = self.changedtick_fold.wrapping_add(1);
    }
}
