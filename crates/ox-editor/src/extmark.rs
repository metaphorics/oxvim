//! Buffer-local extmarks with namespace isolation and edit-stable positions.
//!
//! This module follows Neovim's marktree rules rather than exposing the tree's
//! internal representation. In particular, the splice algorithm collapses
//! endpoints inside deleted text toward the old start or replacement end based
//! on gravity, and translates endpoints after the old extent by the replacement
//! delta (`src/nvim/marktree.c:1921-2073`). Equal-boundary behavior is part of
//! that rule: left-gravity endpoints remain before inserted text while
//! right-gravity endpoints move after it (`src/nvim/marktree.c:1937-1955`,
//! `src/nvim/api/extmark.c:414-415,440-441`).
//!
//! Queries use inclusive zero-based bounds and reverse traversal when the end is
//! before the start, matching `src/nvim/api/extmark.c:235-254,278-291,329-372`.
//! Marks are ordered by start position and then stable id within a namespace.
//! Invalidation follows `src/nvim/extmark.c:440-462`: a configured range is
//! hidden when a deletion contains its complete span.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

/// A zero-based byte-oriented buffer position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtmarkPosition {
    /// Zero-based logical row.
    pub row: usize,
    /// Zero-based byte column.
    pub column: usize,
}

impl ExtmarkPosition {
    /// Creates a zero-based byte position.
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// A row/column extent relative to a splice start.
///
/// For a zero-row extent, `columns` is added to the start column. For a
/// multi-row extent, `columns` is the column on the final row. This is the
/// `(extent_line, extent_col)` convention consumed by Neovim's
/// `marktree_splice` (`src/nvim/marktree.c:1921-1931`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextExtent {
    /// Number of rows spanned.
    pub rows: usize,
    /// Bytes spanned on the same row, or the final-row column.
    pub columns: usize,
}

impl TextExtent {
    /// Creates a relative text extent.
    #[must_use]
    pub const fn new(rows: usize, columns: usize) -> Self {
        Self { rows, columns }
    }

    /// The empty extent used by a pure insertion or deletion result.
    pub const EMPTY: Self = Self::new(0, 0);
}

/// The single geometry representation shared by live edits and undo replay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextSplice {
    pub(crate) start: ExtmarkPosition,
    pub(crate) old_extent: TextExtent,
    pub(crate) new_extent: TextExtent,
}

impl TextSplice {
    pub(crate) fn from_byte_edit(
        start: ExtmarkPosition,
        end: ExtmarkPosition,
        replacement: &[Vec<u8>],
    ) -> Self {
        debug_assert!(!replacement.is_empty());
        debug_assert!(start <= end);
        let old_extent = if start.row == end.row {
            TextExtent::new(0, end.column - start.column)
        } else {
            TextExtent::new(end.row - start.row, end.column)
        };
        let new_extent = if replacement.len() == 1 {
            TextExtent::new(0, replacement[0].len())
        } else {
            TextExtent::new(
                replacement.len() - 1,
                replacement.last().map_or(0, Vec::len),
            )
        };
        Self {
            start,
            old_extent,
            new_extent,
        }
    }

    pub(crate) const fn line_anchored(start_row: usize, old_rows: usize, new_rows: usize) -> Self {
        Self {
            start: ExtmarkPosition::new(start_row, 0),
            old_extent: TextExtent::new(old_rows, 0),
            new_extent: TextExtent::new(new_rows, 0),
        }
    }

    pub(crate) fn old_end(self) -> ExtmarkPosition {
        extent_end(self.start, self.old_extent)
    }

    pub(crate) fn new_end(self) -> ExtmarkPosition {
        extent_end(self.start, self.new_extent)
    }
}

/// Which side of text inserted at an endpoint owns that endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExtmarkGravity {
    /// The endpoint stays before text inserted at the same position.
    Left,
    /// The endpoint moves after text inserted at the same position.
    #[default]
    Right,
}

/// An allocated namespace identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespaceId(u32);

impl NamespaceId {
    /// Internal namespace used by legacy `:sign` placements.
    pub(crate) const fn legacy_sign() -> Self {
        Self(0)
    }

    /// Creates a positive namespace identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ExtmarkError::UnknownNamespace`] when `value` is zero.
    pub const fn new(value: u32) -> Result<Self, ExtmarkError> {
        if value == 0 {
            Err(ExtmarkError::UnknownNamespace(value))
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the positive integer identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Identity of a legacy `:sign` group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SignGroup(NamespaceId);

impl SignGroup {
    /// First namespace handed to a named sign group. The API-layer registry
    /// (`nvim_create_namespace`) counts up from one, so named groups start
    /// far above it and the two allocators never meet in a buffer.
    pub(crate) const NAMED_BASE: u32 = 1 << 30;

    /// The default unnamed sign group.
    pub(crate) const fn default_group() -> Self {
        Self(NamespaceId::legacy_sign())
    }

    /// Wraps an editor-allocated namespace as a named group.
    pub(crate) const fn from_namespace(namespace: NamespaceId) -> Self {
        Self(namespace)
    }

    /// The namespace this group's signs are stored in.
    pub(crate) const fn namespace(self) -> NamespaceId {
        self.0
    }
}

/// A stable identifier unique within one namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtmarkId(u32);

impl ExtmarkId {
    /// Creates a requested positive id.
    ///
    /// # Errors
    ///
    /// Returns [`ExtmarkError::InvalidExtmarkId`] when `value` is zero.
    pub const fn new(value: u32) -> Result<Self, ExtmarkError> {
        if value == 0 {
            Err(ExtmarkError::InvalidExtmarkId)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the positive integer identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One highlighted virtual-text chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VirtualTextChunk {
    /// Text displayed by the chunk.
    pub text: String,
    /// Highlight groups applied in order.
    pub highlight_groups: Vec<String>,
}

impl VirtualTextChunk {
    /// Creates an unhighlighted chunk.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            highlight_groups: Vec::new(),
        }
    }
}

/// A virtual line represented as independently highlighted chunks.
pub type VirtualLine = Vec<VirtualTextChunk>;

/// Virtual-text anchor relative to the buffer or window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExtmarkVirtualTextPosition {
    /// After the buffer line.
    #[default]
    EndOfLine,
    /// Over the buffer text at the extmark column.
    Overlay,
    /// Right-aligned in the window.
    RightAlign,
    /// Right-aligned after the buffer line.
    EndOfLineRightAlign,
    /// Inline, shifting following buffer text to the right.
    Inline,
    /// A fixed zero-based window column.
    WindowColumn(usize),
}

/// Highlight composition for range decorations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExtmarkHighlightMode {
    /// Replace the underlying cell highlight.
    #[default]
    Replace,
    /// Combine with the underlying highlight.
    Combine,
    /// Blend with the underlying highlight.
    Blend,
}

/// Overflow behavior for virtual lines, mirroring `VirtLinesOverflow` in
/// `decoration_defs.h`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExtmarkVirtualLinesOverflow {
    /// Truncate virtual lines that exceed the window height.
    #[default]
    Trunc,
    /// Scroll virtual lines into the buffer.
    Scroll,
    /// Wrap virtual lines.
    Wrap,
    /// Automatically choose between truncation and scrolling.
    Auto,
}

/// Independently combinable extmark rendering and lifetime flags.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExtmarkFlags(u8);

impl ExtmarkFlags {
    /// The range highlight extends through end-of-line.
    pub const HIGHLIGHT_EOL: Self = Self(1 << 0);
    /// `priority` was explicitly supplied.
    pub const PRIORITY_SET: Self = Self(1 << 1);
    /// Hide the mark once a deletion consumes its complete configured range.
    pub const INVALIDATE: Self = Self(1 << 2);
    /// Restore the mark's exact position when surrounding text is undone.
    /// Enabled by default (`src/nvim/extmark.c:451-453` semantics).
    pub const UNDO_RESTORE: Self = Self(1 << 3);
    /// Publish the rendered position to attached UIs.
    pub const UI_WATCHED: Self = Self(1 << 4);

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

/// Rendering and lifetime attributes attached to an extmark.
///
/// The shapes mirror the virtual-text, virtual-line, highlight, sign, and
/// priority data in `src/nvim/decoration_defs.h:11-16,29-45,67-80,102-120`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors upstream decoration_defs.h flag fields; each bool is an independent rendering toggle"
)]
pub struct ExtmarkAttributes {
    /// Virtual text rendered at the mark.
    pub virtual_text: Vec<VirtualTextChunk>,
    /// Virtual-text anchor.
    pub virtual_text_position: ExtmarkVirtualTextPosition,
    /// Fixed window column for overlay virtual text, when `virt_text_win_col` is set.
    pub virt_text_win_col: Option<usize>,
    /// Whether virtual text is hidden when the extmark is off-screen.
    pub virt_text_hide: bool,
    /// Whether virtual text repeats on wrapped line breaks.
    pub virt_text_repeat_linebreak: bool,
    /// Virtual lines associated with the mark.
    pub virtual_lines: Vec<VirtualLine>,
    /// Whether virtual lines render above the buffer line.
    pub virt_lines_above: bool,
    /// Whether virtual lines render in the left column.
    pub virt_lines_leftcol: bool,
    /// Overflow behavior for virtual lines.
    pub virt_lines_overflow: ExtmarkVirtualLinesOverflow,
    /// Highlight group applied to the marked span.
    pub highlight_group: Option<String>,
    /// Additional highlight groups stacked after the first.
    pub additional_highlight_groups: Vec<String>,
    /// Range-highlight composition, when explicitly supplied.
    pub highlight_mode: Option<ExtmarkHighlightMode>,
    /// Text rendered in the sign column.
    pub sign_text: Option<String>,
    /// Sign text highlight group.
    pub sign_highlight_group: Option<String>,
    /// Number-column highlight group.
    pub number_highlight_group: Option<String>,
    /// Whole-line highlight group.
    pub line_highlight_group: Option<String>,
    /// Sign-column highlight used on the cursor line.
    pub cursorline_highlight_group: Option<String>,
    /// Legacy sign name, when the mark came from `:sign`.
    pub sign_name: Option<String>,
    /// Conceal character replacing the marked text.
    pub conceal: Option<String>,
    /// Conceal replacement for the entire line, when set.
    pub conceal_lines: Option<String>,
    /// Spell-check override: `Some(true)` enables, `Some(false)` disables,
    /// `None` leaves the buffer default.
    pub spell: Option<bool>,
    /// URL attached to the extmark.
    pub url: Option<String>,
    /// Rendering priority; larger values render later.
    pub priority: u32,
    /// Rendering and lifetime flags (`ExtmarkFlags`).
    pub flags: ExtmarkFlags,
}

impl Default for ExtmarkAttributes {
    fn default() -> Self {
        Self {
            virtual_text: Vec::new(),
            virtual_text_position: ExtmarkVirtualTextPosition::EndOfLine,
            virt_text_win_col: None,
            virt_text_hide: false,
            virt_text_repeat_linebreak: false,
            virtual_lines: Vec::new(),
            virt_lines_above: false,
            virt_lines_leftcol: false,
            virt_lines_overflow: ExtmarkVirtualLinesOverflow::Trunc,
            highlight_group: None,
            additional_highlight_groups: Vec::new(),
            highlight_mode: None,
            sign_text: None,
            sign_highlight_group: None,
            number_highlight_group: None,
            line_highlight_group: None,
            cursorline_highlight_group: None,
            sign_name: None,
            conceal: None,
            conceal_lines: None,
            spell: None,
            url: None,
            priority: 0,
            flags: ExtmarkFlags::UNDO_RESTORE,
        }
    }
}

impl ExtmarkAttributes {
    /// Whether this mark contributes sign-column data.
    #[must_use]
    pub fn has_sign(&self) -> bool {
        self.sign_text.is_some()
            || self.sign_highlight_group.is_some()
            || self.number_highlight_group.is_some()
            || self.line_highlight_group.is_some()
            || self.cursorline_highlight_group.is_some()
            || self.sign_name.is_some()
    }
}

/// The optional end of an extmark range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtmarkEnd {
    /// Exclusive range end.
    pub position: ExtmarkPosition,
    /// Gravity applied independently at the end.
    pub gravity: ExtmarkGravity,
}

impl ExtmarkEnd {
    /// Creates an end with Neovim's default left gravity.
    #[must_use]
    pub const fn new(position: ExtmarkPosition) -> Self {
        Self {
            position,
            gravity: ExtmarkGravity::Left,
        }
    }
}

/// Complete placement data used to create or replace an extmark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtmarkPlacement {
    /// Start position.
    pub position: ExtmarkPosition,
    /// Optional exclusive range end.
    pub end: Option<ExtmarkEnd>,
    /// Start gravity. Neovim defaults starts to right gravity.
    pub gravity: ExtmarkGravity,
    /// Decoration and invalidation attributes.
    pub attributes: ExtmarkAttributes,
}

impl ExtmarkPlacement {
    /// Creates a point extmark with right gravity and no attributes.
    #[must_use]
    pub fn new(position: ExtmarkPosition) -> Self {
        Self {
            position,
            end: None,
            gravity: ExtmarkGravity::Right,
            attributes: ExtmarkAttributes::default(),
        }
    }

    /// Adds a left-gravity range end.
    #[must_use]
    pub fn with_end(mut self, position: ExtmarkPosition) -> Self {
        self.end = Some(ExtmarkEnd::new(position));
        self
    }
}

/// A stored extmark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extmark {
    /// Namespace that owns the mark.
    pub namespace: NamespaceId,
    /// Stable namespace-local id.
    pub id: ExtmarkId,
    /// Current placement and decoration data.
    pub placement: ExtmarkPlacement,
    /// Whether complete-range deletion has hidden the mark.
    pub invalid: bool,
    /// Stable insertion order used to break equal rendering priorities.
    render_order: u64,
}

impl Extmark {
    /// Returns the current start position.
    #[must_use]
    pub const fn position(&self) -> ExtmarkPosition {
        self.placement.position
    }
}

/// Counts produced by a text splice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpliceResult {
    /// Marks with at least one endpoint moved.
    pub moved: usize,
    /// Previously valid marks invalidated by the deletion.
    pub invalidated: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpliceUndoEntry {
    namespace: NamespaceId,
    id: ExtmarkId,
    position: ExtmarkPosition,
    end: Option<ExtmarkPosition>,
    invalid: bool,
    after_position: ExtmarkPosition,
    after_end: Option<ExtmarkPosition>,
    after_invalid: bool,
    restore_before: bool,
}

/// Compact position and invalidation delta retained with one undo header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExtmarkSpliceUndo {
    splice: TextSplice,
    entries: Vec<SpliceUndoEntry>,
}

impl ExtmarkSpliceUndo {
    /// Binds an explicit create/move to this recorded splice so redo restores
    /// the set-time point or range instead of the splice-adjusted result.
    pub(crate) fn retarget_set(
        &mut self,
        namespace: NamespaceId,
        id: ExtmarkId,
        after_position: ExtmarkPosition,
        after_end: Option<ExtmarkPosition>,
        after_invalid: bool,
        previous: Option<(ExtmarkPosition, Option<ExtmarkPosition>, bool)>,
    ) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.namespace == namespace && entry.id == id)
        {
            entry.after_position = after_position;
            entry.after_end = after_end;
            entry.after_invalid = after_invalid;
            return;
        }
        let (position, end, invalid, restore_before) = match previous {
            Some((position, end, invalid)) => (position, end, invalid, true),
            None => (after_position, after_end, after_invalid, false),
        };
        self.entries.push(SpliceUndoEntry {
            namespace,
            id,
            position,
            end,
            invalid,
            after_position,
            after_end,
            after_invalid,
            restore_before,
        });
    }
}

/// An invalid namespace, id, range, or splice operation.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ExtmarkError {
    /// Namespace identifiers were exhausted.
    #[error("extmark namespace identifiers are exhausted")]
    NamespaceIdExhausted,
    /// The supplied namespace was not allocated by this store.
    #[error("unknown extmark namespace {0}")]
    UnknownNamespace(u32),
    /// Extmark identifiers were exhausted in a namespace.
    #[error("extmark identifiers are exhausted in namespace {0}")]
    ExtmarkIdExhausted(u32),
    /// Requested extmark id zero is reserved for automatic allocation.
    #[error("extmark id must be positive")]
    InvalidExtmarkId,
    /// An update named an id that does not exist in the namespace.
    #[error("extmark {id} does not exist in namespace {namespace}")]
    UnknownExtmark {
        /// Namespace containing the requested id.
        namespace: u32,
        /// Missing namespace-local id.
        id: u32,
    },
    /// A configured range ended before it started.
    #[error("extmark range end precedes its start")]
    EndBeforeStart,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NamespaceState {
    highest_id: u32,
    by_id: BTreeMap<ExtmarkId, Extmark>,
    by_position: BTreeMap<ExtmarkPosition, BTreeSet<ExtmarkId>>,
}

impl NamespaceState {
    fn insert_index(&mut self, position: ExtmarkPosition, id: ExtmarkId) {
        self.by_position.entry(position).or_default().insert(id);
    }

    fn remove_index(&mut self, position: ExtmarkPosition, id: ExtmarkId) {
        let remove_position = if let Some(ids) = self.by_position.get_mut(&position) {
            ids.remove(&id);
            ids.is_empty()
        } else {
            false
        };
        if remove_position {
            self.by_position.remove(&position);
        }
    }

    /// Moves one mark's index entry after an in-place geometry change.
    ///
    /// Incremental callers use this instead of `rebuild_index` so marks
    /// whose start position did not move keep their entries across a
    /// splice.
    fn move_index(&mut self, old: ExtmarkPosition, new: ExtmarkPosition, id: ExtmarkId) {
        self.remove_index(old, id);
        self.insert_index(new, id);
    }

    /// Reconciles the index after in-place geometry changes.
    ///
    /// Each relocation costs two tree operations (remove plus insert)
    /// against one per mark for a full rebuild, so past the halfway point
    /// the rebuild is cheaper: worst-case splices (paste at the top of a
    /// mark-dense file, full-file reindent) fall back instead of
    /// regressing.
    fn reconcile_index(&mut self, relocated: &[(ExtmarkId, ExtmarkPosition, ExtmarkPosition)]) {
        if relocated.len() * 2 > self.by_id.len() {
            self.rebuild_index();
        } else {
            for &(id, old_start, new_start) in relocated {
                self.move_index(old_start, new_start, id);
            }
        }
    }

    fn rebuild_index(&mut self) {
        self.by_position.clear();
        for (&id, mark) in &self.by_id {
            self.by_position
                .entry(mark.position())
                .or_default()
                .insert(id);
        }
    }
}

/// Namespace-isolated extmark storage for one buffer.
///
/// Named namespace creation is idempotent and empty names allocate fresh
/// anonymous namespaces, following `src/nvim/api/extmark.c:47-70`. Requested
/// positive extmark ids update existing marks or create that id and advance the
/// automatic allocator (`src/nvim/extmark.c:81-123`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extmarks {
    highest_namespace: u32,
    named_namespaces: BTreeMap<String, NamespaceId>,
    namespaces: BTreeMap<NamespaceId, NamespaceState>,
    next_render_order: u64,
}

impl Extmarks {
    /// Creates an empty per-buffer store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            highest_namespace: 0,
            named_namespaces: BTreeMap::new(),
            namespaces: BTreeMap::new(),
            next_render_order: 0,
        }
    }

    /// Creates or retrieves a namespace.
    ///
    /// A nonempty name retrieves its prior allocation; an empty name always
    /// allocates a new anonymous namespace.
    ///
    /// # Errors
    ///
    /// Returns [`ExtmarkError::NamespaceIdExhausted`] after all namespace ids
    /// have been allocated.
    pub fn create_namespace(
        &mut self,
        name: impl Into<String>,
    ) -> Result<NamespaceId, ExtmarkError> {
        let name = name.into();
        if !name.is_empty()
            && let Some(&id) = self.named_namespaces.get(&name)
        {
            return Ok(id);
        }

        let raw = self
            .highest_namespace
            .checked_add(1)
            .ok_or(ExtmarkError::NamespaceIdExhausted)?;
        let id = NamespaceId(raw);
        self.highest_namespace = raw;
        self.namespaces.insert(id, NamespaceState::default());
        if !name.is_empty() {
            self.named_namespaces.insert(name, id);
        }
        Ok(id)
    }

    /// Ensures a namespace allocated by the editor-global API registry exists locally.
    ///
    /// # Errors
    ///
    /// This operation is currently infallible.
    pub fn ensure_namespace(&mut self, namespace: NamespaceId) -> Result<(), ExtmarkError> {
        self.highest_namespace = self.highest_namespace.max(namespace.get());
        self.namespaces.entry(namespace).or_default();
        Ok(())
    }

    /// Gets a previously allocated named namespace.
    #[must_use]
    pub fn namespace(&self, name: &str) -> Option<NamespaceId> {
        self.named_namespaces.get(name).copied()
    }

    /// Returns all allocated namespace ids in ascending order.
    #[must_use]
    pub fn namespace_ids(&self) -> Vec<NamespaceId> {
        self.namespaces.keys().copied().collect()
    }

    /// Inserts a mark or replaces the mark with the requested id.
    ///
    /// Passing `None` allocates a fresh stable id. Passing `Some(id)` mirrors
    /// Neovim's explicit-id set/update behavior.
    ///
    /// # Errors
    ///
    /// Returns an error when the namespace is unknown, the range end precedes
    /// its start, or the render-order or namespace-local id space is exhausted.
    pub fn set(
        &mut self,
        namespace: NamespaceId,
        requested_id: Option<ExtmarkId>,
        placement: ExtmarkPlacement,
    ) -> Result<ExtmarkId, ExtmarkError> {
        validate_placement(&placement)?;
        self.namespace_state(namespace)?;
        let existing_order = requested_id.and_then(|id| {
            self.namespace_state(namespace)
                .ok()
                .and_then(|state| state.by_id.get(&id))
                .map(|mark| mark.render_order)
        });
        let render_order = existing_order.unwrap_or(self.next_render_order);
        if existing_order.is_none() {
            self.next_render_order = self
                .next_render_order
                .checked_add(1)
                .ok_or(ExtmarkError::ExtmarkIdExhausted(namespace.get()))?;
        }
        let state = self.namespace_mut(namespace)?;
        let id = if let Some(id) = requested_id {
            state.highest_id = state.highest_id.max(id.get());
            id
        } else {
            let raw = state
                .highest_id
                .checked_add(1)
                .ok_or(ExtmarkError::ExtmarkIdExhausted(namespace.get()))?;
            state.highest_id = raw;
            ExtmarkId(raw)
        };

        let old_position = state.by_id.get(&id).map(Extmark::position);
        if let Some(position) = old_position {
            state.remove_index(position, id);
        }
        let position = placement.position;
        state.by_id.insert(
            id,
            Extmark {
                namespace,
                id,
                placement,
                invalid: false,
                render_order,
            },
        );
        state.insert_index(position, id);
        Ok(id)
    }

    /// Replaces an existing mark without permitting accidental creation.
    ///
    /// # Errors
    ///
    /// Returns an error when the namespace or extmark is unknown, the range end
    /// precedes its start, or the render-order space is exhausted.
    pub fn update(
        &mut self,
        namespace: NamespaceId,
        id: ExtmarkId,
        placement: ExtmarkPlacement,
    ) -> Result<(), ExtmarkError> {
        let exists = self.namespace_state(namespace)?.by_id.contains_key(&id);
        if !exists {
            return Err(ExtmarkError::UnknownExtmark {
                namespace: namespace.get(),
                id: id.get(),
            });
        }
        self.set(namespace, Some(id), placement).map(|_| ())
    }

    /// Gets a mark by namespace-local id.
    ///
    /// # Errors
    ///
    /// Returns [`ExtmarkError::UnknownNamespace`] when `namespace` is not
    /// allocated by this store.
    pub fn get(
        &self,
        namespace: NamespaceId,
        id: ExtmarkId,
    ) -> Result<Option<&Extmark>, ExtmarkError> {
        Ok(self.namespace_state(namespace)?.by_id.get(&id))
    }

    /// Deletes a mark, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns [`ExtmarkError::UnknownNamespace`] when `namespace` is not
    /// allocated by this store.
    pub fn delete(&mut self, namespace: NamespaceId, id: ExtmarkId) -> Result<bool, ExtmarkError> {
        let state = self.namespace_mut(namespace)?;
        if let Some(mark) = state.by_id.remove(&id) {
            state.remove_index(mark.position(), id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Clears marks whose starts lie within inclusive bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ExtmarkError::UnknownNamespace`] when `namespace` is not
    /// allocated by this store.
    pub fn clear(
        &mut self,
        namespace: NamespaceId,
        first: ExtmarkPosition,
        last: ExtmarkPosition,
    ) -> Result<usize, ExtmarkError> {
        let state = self.namespace_mut(namespace)?;
        let (lower, upper) = ordered_bounds(first, last);
        let ids: Vec<ExtmarkId> = state
            .by_position
            .range(lower..=upper)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();
        for id in &ids {
            if let Some(mark) = state.by_id.remove(id) {
                state.remove_index(mark.position(), *id);
            }
        }
        Ok(ids.len())
    }

    /// Returns marks in deterministic traversal order within inclusive bounds.
    ///
    /// When `last < first`, the exact forward order is reversed. `None` means
    /// unlimited and `Some(0)` returns no marks. Only start positions determine
    /// membership, as in Neovim queries without the `overlap` option.
    ///
    /// # Errors
    ///
    /// Returns [`ExtmarkError::UnknownNamespace`] when `namespace` is not
    /// allocated by this store.
    pub fn query(
        &self,
        namespace: NamespaceId,
        first: ExtmarkPosition,
        last: ExtmarkPosition,
        limit: Option<usize>,
    ) -> Result<Vec<Extmark>, ExtmarkError> {
        let state = self.namespace_state(namespace)?;
        let capacity = limit.unwrap_or(usize::MAX);
        if capacity == 0 {
            return Ok(Vec::new());
        }
        let (lower, upper) = ordered_bounds(first, last);
        let mut marks = Vec::new();

        if first <= last {
            for (_, ids) in state.by_position.range(lower..=upper) {
                for id in ids {
                    if let Some(mark) = state.by_id.get(id) {
                        marks.push(mark.clone());
                        if marks.len() == capacity {
                            return Ok(marks);
                        }
                    }
                }
            }
        } else {
            for (_, ids) in state.by_position.range(lower..=upper).rev() {
                for id in ids.iter().rev() {
                    if let Some(mark) = state.by_id.get(id) {
                        marks.push(mark.clone());
                        if marks.len() == capacity {
                            return Ok(marks);
                        }
                    }
                }
            }
        }
        Ok(marks)
    }

    /// Returns marks from every namespace in traversal order.
    ///
    /// This is the `ns_id = -1` form accepted by
    /// `nvim_buf_get_extmarks` (`src/nvim/api/extmark.c:276-292,361-373`).
    #[must_use]
    pub fn query_all(
        &self,
        first: ExtmarkPosition,
        last: ExtmarkPosition,
        limit: Option<usize>,
    ) -> Vec<Extmark> {
        let capacity = limit.unwrap_or(usize::MAX);
        if capacity == 0 {
            return Vec::new();
        }
        let (lower, upper) = ordered_bounds(first, last);
        let mut marks: Vec<_> = self
            .namespaces
            .values()
            .flat_map(|state| state.by_id.values())
            .filter(|mark| lower <= mark.position() && mark.position() <= upper)
            .cloned()
            .collect();
        marks.sort_by_key(|mark| (mark.position(), mark.namespace, mark.id));
        if first > last {
            marks.reverse();
        }
        marks.truncate(capacity);
        marks
    }

    /// Returns extmarks in decoration compositing order: low priority first,
    /// then stable insertion order for deterministic equal-priority overlap.
    #[must_use]
    pub fn render_ordered(&self) -> Vec<&Extmark> {
        let mut marks: Vec<_> = self
            .namespaces
            .values()
            .flat_map(|state| state.by_id.values())
            .filter(|mark| {
                !mark.invalid
                    && (mark.placement.attributes.highlight_group.is_some()
                        || mark.placement.attributes.has_sign()
                        || mark
                            .placement
                            .attributes
                            .flags
                            .contains(ExtmarkFlags::UI_WATCHED))
            })
            .collect();
        marks.sort_by_key(|mark| (mark.placement.attributes.priority, mark.render_order));
        marks
    }

    /// Applies a general text replacement to every namespace.
    ///
    /// `old_extent == TextExtent::EMPTY` is insertion; `new_extent ==
    /// TextExtent::EMPTY` is deletion. Endpoints strictly inside deleted text
    /// collapse to `start` under left gravity or the replacement end under
    /// right gravity. The same choice applies at both deletion boundaries,
    /// matching the gravity-sensitive boundary selection in
    /// `src/nvim/marktree.c:1937-2046`. Endpoints after the deleted extent are
    /// translated by the replacement delta (`src/nvim/marktree.c:2049-2073`).
    /// If independently transformed range endpoints cross, their positions are
    /// ordered again while retaining the gravity configured for each endpoint.
    pub(crate) fn splice(&mut self, splice: TextSplice) -> SpliceResult {
        self.splice_recording(splice).0
    }

    /// Hide `invalidate` marks and drop the rest when a nofile buffer unloads.
    ///
    /// Neovim splices the whole buffer away (`src/nvim/buffer.c:928-932`).
    /// Marks with `invalidate` and `undo_restore` remain at the origin as
    /// invalid so `nvim_buf_get_extmark_by_id` still finds them. Marks without
    /// `invalidate`, or with `undo_restore = false`, are deleted.
    pub fn invalidate_for_unload(&mut self) {
        for state in self.namespaces.values_mut() {
            let mut kept = BTreeMap::new();
            for (id, mut mark) in std::mem::take(&mut state.by_id) {
                if mark
                    .placement
                    .attributes
                    .flags
                    .contains(ExtmarkFlags::INVALIDATE)
                    && mark
                        .placement
                        .attributes
                        .flags
                        .contains(ExtmarkFlags::UNDO_RESTORE)
                {
                    mark.placement.position = ExtmarkPosition::new(0, 0);
                    if let Some(end) = &mut mark.placement.end {
                        end.position = ExtmarkPosition::new(0, 0);
                    }
                    mark.invalid = true;
                    kept.insert(id, mark);
                }
            }
            state.by_id = kept;
            state.rebuild_index();
        }
    }

    pub(crate) fn splice_recording(
        &mut self,
        splice: TextSplice,
    ) -> (SpliceResult, ExtmarkSpliceUndo) {
        let start = splice.start;
        let old_extent = splice.old_extent;
        let old_end = splice.old_end();
        let new_end = splice.new_end();
        let mut result = SpliceResult::default();
        let mut undo = ExtmarkSpliceUndo {
            splice,
            entries: Vec::new(),
        };
        for state in self.namespaces.values_mut() {
            let mut removed = Vec::new();
            let mut relocated = Vec::new();
            for mark in state.by_id.values_mut() {
                let old_start = mark.position();
                let old_range_end = mark.placement.end.map(|end| end.position);
                let old_invalid = mark.invalid;
                if old_extent != TextExtent::EMPTY
                    && !mark.invalid
                    && mark
                        .placement
                        .attributes
                        .flags
                        .contains(ExtmarkFlags::INVALIDATE)
                    && (old_range_end.is_some_and(|end| start <= old_start && end <= old_end)
                        || (old_range_end.is_none()
                            && old_extent.rows > 0
                            && start <= old_start
                            && old_start.row < old_end.row))
                {
                    result.invalidated += 1;
                    if !mark
                        .placement
                        .attributes
                        .flags
                        .contains(ExtmarkFlags::UNDO_RESTORE)
                    {
                        // `undo_restore = false`: delete instead of hiding
                        // (`src/nvim/extmark.c:451-453`).
                        removed.push(mark.id);
                        continue;
                    }
                    mark.invalid = true;
                }

                mark.placement.position = transform_position(
                    old_start,
                    mark.placement.gravity,
                    start,
                    old_end,
                    new_end,
                    old_extent == TextExtent::EMPTY,
                );
                if let Some(end) = &mut mark.placement.end {
                    end.position = transform_position(
                        end.position,
                        end.gravity,
                        start,
                        old_end,
                        new_end,
                        old_extent == TextExtent::EMPTY,
                    );
                    if end.position < mark.placement.position {
                        std::mem::swap(&mut mark.placement.position, &mut end.position);
                    }
                }

                if mark.position() != old_start
                    || mark.placement.end.map(|end| end.position) != old_range_end
                {
                    result.moved += 1;
                }
                let changed = mark.position() != old_start
                    || mark.placement.end.map(|end| end.position) != old_range_end
                    || mark.invalid != old_invalid;
                let deletion_touched_endpoint = old_extent != TextExtent::EMPTY
                    && ((start <= old_start && old_start < old_end)
                        || old_range_end.is_some_and(|end| start <= end && end < old_end));
                if changed || deletion_touched_endpoint {
                    undo.entries.push(SpliceUndoEntry {
                        namespace: mark.namespace,
                        id: mark.id,
                        position: old_start,
                        end: old_range_end,
                        invalid: old_invalid,
                        after_position: mark.position(),
                        after_end: mark.placement.end.map(|end| end.position),
                        after_invalid: mark.invalid,
                        restore_before: true,
                    });
                }
                if mark.position() != old_start {
                    relocated.push((mark.id, old_start, mark.position()));
                }
            }
            for id in removed {
                if let Some(mark) = state.by_id.remove(&id) {
                    state.remove_index(mark.position(), id);
                }
            }
            state.reconcile_index(&relocated);
        }
        (result, undo)
    }

    pub(crate) fn undo_splice(&mut self, undo: &ExtmarkSpliceUndo) {
        let restorable: BTreeSet<_> = undo
            .entries
            .iter()
            .filter_map(|entry| {
                let mark = self
                    .namespaces
                    .get(&entry.namespace)?
                    .by_id
                    .get(&entry.id)?;
                (mark.position() == entry.after_position
                    && mark.placement.end.map(|end| end.position) == entry.after_end
                    && mark.invalid == entry.after_invalid)
                    .then_some((entry.namespace, entry.id))
            })
            .collect();
        self.splice(TextSplice {
            start: undo.splice.start,
            old_extent: undo.splice.new_extent,
            new_extent: undo.splice.old_extent,
        });
        let mut relocated = Vec::new();
        for entry in &undo.entries {
            if !entry.restore_before || !restorable.contains(&(entry.namespace, entry.id)) {
                continue;
            }
            if let Some(state) = self.namespaces.get_mut(&entry.namespace)
                && let Some(mark) = state.by_id.get_mut(&entry.id)
            {
                let old_start = mark.position();
                apply_recorded_geometry(mark, entry.position, entry.end, entry.invalid);
                if mark.position() != old_start {
                    relocated.push((entry.namespace, entry.id, old_start, mark.position()));
                }
            }
        }
        let mut by_namespace: BTreeMap<
            NamespaceId,
            Vec<(ExtmarkId, ExtmarkPosition, ExtmarkPosition)>,
        > = BTreeMap::new();
        for (namespace, id, old_start, new_start) in relocated {
            by_namespace
                .entry(namespace)
                .or_default()
                .push((id, old_start, new_start));
        }
        for (namespace, entries) in &by_namespace {
            if let Some(state) = self.namespaces.get_mut(namespace) {
                state.reconcile_index(entries);
            }
        }
    }

    pub(crate) fn redo_splice(&mut self, undo: &ExtmarkSpliceUndo) {
        let restorable: BTreeSet<_> = undo
            .entries
            .iter()
            .filter_map(|entry| {
                let mark = self
                    .namespaces
                    .get(&entry.namespace)?
                    .by_id
                    .get(&entry.id)?;
                (!entry.restore_before
                    || (mark.position() == entry.position
                        && mark.placement.end.map(|end| end.position) == entry.end
                        && mark.invalid == entry.invalid))
                    .then_some((entry.namespace, entry.id))
            })
            .collect();
        self.splice(undo.splice);
        let mut relocated = Vec::new();
        for entry in &undo.entries {
            if !restorable.contains(&(entry.namespace, entry.id)) {
                continue;
            }
            if let Some(state) = self.namespaces.get_mut(&entry.namespace)
                && let Some(mark) = state.by_id.get_mut(&entry.id)
            {
                let old_start = mark.position();
                apply_recorded_geometry(
                    mark,
                    entry.after_position,
                    entry.after_end,
                    entry.after_invalid,
                );
                if mark.position() != old_start {
                    relocated.push((entry.namespace, entry.id, old_start, mark.position()));
                }
            }
        }
        let mut by_namespace: BTreeMap<
            NamespaceId,
            Vec<(ExtmarkId, ExtmarkPosition, ExtmarkPosition)>,
        > = BTreeMap::new();
        for (namespace, id, old_start, new_start) in relocated {
            by_namespace
                .entry(namespace)
                .or_default()
                .push((id, old_start, new_start));
        }
        for (namespace, entries) in &by_namespace {
            if let Some(state) = self.namespaces.get_mut(namespace) {
                state.reconcile_index(entries);
            }
        }
    }

    fn namespace_state(&self, namespace: NamespaceId) -> Result<&NamespaceState, ExtmarkError> {
        self.namespaces
            .get(&namespace)
            .ok_or(ExtmarkError::UnknownNamespace(namespace.get()))
    }

    fn namespace_mut(
        &mut self,
        namespace: NamespaceId,
    ) -> Result<&mut NamespaceState, ExtmarkError> {
        self.namespaces
            .get_mut(&namespace)
            .ok_or(ExtmarkError::UnknownNamespace(namespace.get()))
    }
}

fn apply_recorded_geometry(
    mark: &mut Extmark,
    position: ExtmarkPosition,
    end: Option<ExtmarkPosition>,
    invalid: bool,
) {
    mark.placement.position = position;
    match (mark.placement.end.as_mut(), end) {
        (Some(existing), Some(position)) => existing.position = position,
        (None, Some(position)) => mark.placement.end = Some(ExtmarkEnd::new(position)),
        (Some(_), None) => mark.placement.end = None,
        (None, None) => {}
    }
    mark.invalid = invalid;
}

fn validate_placement(placement: &ExtmarkPlacement) -> Result<(), ExtmarkError> {
    if placement
        .end
        .is_some_and(|end| end.position < placement.position)
    {
        Err(ExtmarkError::EndBeforeStart)
    } else {
        Ok(())
    }
}

fn ordered_bounds(
    first: ExtmarkPosition,
    last: ExtmarkPosition,
) -> (ExtmarkPosition, ExtmarkPosition) {
    if first <= last {
        (first, last)
    } else {
        (last, first)
    }
}

fn extent_end(start: ExtmarkPosition, extent: TextExtent) -> ExtmarkPosition {
    if extent.rows == 0 {
        ExtmarkPosition::new(start.row, start.column.saturating_add(extent.columns))
    } else {
        ExtmarkPosition::new(start.row.saturating_add(extent.rows), extent.columns)
    }
}

fn transform_position(
    position: ExtmarkPosition,
    gravity: ExtmarkGravity,
    start: ExtmarkPosition,
    old_end: ExtmarkPosition,
    new_end: ExtmarkPosition,
    insertion: bool,
) -> ExtmarkPosition {
    if position < start {
        return position;
    }

    if insertion && position == start {
        return match gravity {
            ExtmarkGravity::Left => start,
            ExtmarkGravity::Right => new_end,
        };
    }

    if !insertion && position <= old_end {
        return match gravity {
            ExtmarkGravity::Left => start,
            ExtmarkGravity::Right => new_end,
        };
    }

    if position.row == old_end.row {
        let suffix = position.column.saturating_sub(old_end.column);
        ExtmarkPosition::new(new_end.row, new_end.column.saturating_add(suffix))
    } else {
        let suffix_rows = position.row.saturating_sub(old_end.row);
        ExtmarkPosition::new(new_end.row.saturating_add(suffix_rows), position.column)
    }
}
