//! Buffer-local folds and the host-facing fold computation seams.
//!
//! Neovim identifies the six methods in `src/nvim/fold.c:321-361`, lazily
//! invalidates computed folds in `src/nvim/fold.c:763-829`, and recursively
//! searches nested folds in `src/nvim/fold.c:1086-1107`. This module keeps
//! those contracts independent of a window or Lua runtime.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use thiserror::Error;

/// Neovim's maximum representable fold nesting depth.
///
/// See `src/nvim/fold.c:78-84`.
pub const MAX_FOLD_DEPTH: usize = 20;

/// A zero-based, byte-oriented buffer position.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Position {
    /// Zero-based logical row.
    pub row: usize,
    /// Zero-based byte column.
    pub column: usize,
}

impl Position {
    /// Creates a position from a zero-based row and byte column.
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// A half-open fold range, `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FoldRange {
    /// First position in the fold.
    pub start: Position,
    /// First position after the fold.
    pub end: Position,
}

impl FoldRange {
    /// Creates a range and reverses its endpoints when necessary.
    ///
    /// Neovim reverses manual fold endpoints in `src/nvim/fold.c:535-552`.
    pub fn normalized(start: Position, end: Position) -> Result<Self, FoldError> {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        if start == end {
            return Err(FoldError::EmptyRange);
        }
        Ok(Self { start, end })
    }

    /// Creates a whole-line half-open range.
    pub fn lines(start_row: usize, end_row_exclusive: usize) -> Result<Self, FoldError> {
        Self::normalized(
            Position::new(start_row, 0),
            Position::new(end_row_exclusive, 0),
        )
    }

    /// Returns whether this range contains the position.
    #[must_use]
    pub fn contains(self, position: Position) -> bool {
        self.start <= position && position < self.end
    }

    /// Returns whether this range fully contains another range.
    #[must_use]
    pub fn contains_range(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// Returns whether the ranges share at least one position.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Returns the smallest range enclosing both ranges.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Returns whether the fold occupies any part of a zero-based row.
    ///
    /// Neovim's folds are whole lines: `fold_T` stores `fd_top` and `fd_len`
    /// as line counts (`src/nvim/fold_defs.h:31-45`). A whole-line range
    /// therefore occupies rows `[start.row, end.row)`, while a range ending
    /// inside a row still occupies that row.
    #[must_use]
    pub const fn covers_row(self, row: usize) -> bool {
        self.start.row <= row
            && (row < self.end.row || (row == self.end.row && self.end.column > 0))
    }

    /// Returns the last zero-based row the fold occupies.
    #[must_use]
    pub const fn last_row(self) -> usize {
        if self.end.column > 0 {
            self.end.row
        } else {
            self.end.row.saturating_sub(1)
        }
    }
}

/// The source used to derive folds.
///
/// The method set follows `src/nvim/fold.c:321-361`. Expr, syntax, and diff
/// deliberately have no evaluator here; [`FoldComputeRequest`] is their typed
/// host boundary.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum FoldMethod {
    /// Folds are explicitly created and deleted.
    #[default]
    Manual,
    /// Fold levels derive from leading indentation.
    Indent,
    /// Fold levels are supplied by the configured host expression.
    Expr,
    /// Fold levels derive from start and end marker bytes.
    Marker,
    /// Fold levels are supplied by the syntax host.
    Syntax,
    /// Fold ranges are supplied by the diff host.
    Diff,
}

impl FoldMethod {
    /// Returns the host kind for methods that require host computation.
    #[must_use]
    pub const fn host_kind(self) -> Option<HostFoldKind> {
        match self {
            Self::Expr => Some(HostFoldKind::Expr),
            Self::Syntax => Some(HostFoldKind::Syntax),
            Self::Diff => Some(HostFoldKind::Diff),
            Self::Manual | Self::Indent | Self::Marker => None,
        }
    }

    /// Returns the method `'foldmethod'` names, defaulting to manual.
    ///
    /// Neovim validates the string in `src/nvim/optionstr.c` and compares it
    /// with the same six names through the `foldmethodIs*` macros in
    /// `src/nvim/fold.h:15-21`.
    #[must_use]
    pub fn from_option_value(value: &str) -> Self {
        match value {
            "indent" => Self::Indent,
            "expr" => Self::Expr,
            "marker" => Self::Marker,
            "syntax" => Self::Syntax,
            "diff" => Self::Diff,
            _ => Self::Manual,
        }
    }
}

/// A fold's explicit display state.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum FoldState {
    /// The fold's contents are visible.
    Open,
    /// The fold is collapsed.
    #[default]
    Closed,
}

/// One normalized fold in deterministic outer-before-inner order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fold {
    /// Half-open byte range occupied by this fold.
    pub range: FoldRange,
    /// One-based nesting depth.
    pub depth: usize,
    /// Explicit open or closed state.
    pub state: FoldState,
}

/// A fold computation delegated to the editor host.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HostFoldKind {
    /// Evaluate the buffer's fold expression.
    Expr,
    /// Query syntax-derived fold levels.
    Syntax,
    /// Query diff-derived folded regions.
    Diff,
}

/// An immutable request for host-computed fold ranges.
///
/// Neovim dispatches expr, syntax, and diff to separate level getters in
/// `src/nvim/fold.c:1933-1983`. The request keeps that dispatch typed without
/// executing Lua or depending on syntax/diff implementations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldComputeRequest {
    /// Required host computation.
    pub kind: HostFoldKind,
    /// Text generation for which the result is valid.
    pub changedtick: u64,
    /// Number of logical buffer lines visible to the host.
    pub line_count: usize,
}

/// A host's completed fold computation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldComputeResult {
    /// Request this result answers.
    pub request: FoldComputeRequest,
    /// Normalized half-open ranges returned by the host.
    pub ranges: Vec<FoldRange>,
}

/// Outcome of lazily refreshing the active fold method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FoldRefresh {
    /// The cache is ready for queries.
    Ready {
        /// Text generation represented by the cache.
        changedtick: u64,
        /// Number of active folds.
        fold_count: usize,
    },
    /// The host must compute the requested ranges.
    Host(FoldComputeRequest),
}

/// A request to evaluate fold text for one closed fold.
///
/// The host receives the same essential context Neovim installs for foldtext:
/// start, end, and fold level (`src/nvim/fold.c:1681-1724`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldTextRequest {
    /// Text generation containing the fold.
    pub changedtick: u64,
    /// Fold being rendered.
    pub range: FoldRange,
    /// One-based fold depth.
    pub level: usize,
}

/// One highlighted chunk returned by a foldtext host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldTextChunk {
    /// Display bytes.
    pub text: Vec<u8>,
    /// Optional highlight group name.
    pub highlight: Option<String>,
}

/// A typed foldtext host result.
///
/// Neovim accepts either text or virtual-text chunks in
/// `src/nvim/fold.c:1726-1750`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FoldText {
    /// Unhighlighted display bytes.
    Plain(Vec<u8>),
    /// Highlighted virtual-text chunks.
    Virtual(Vec<FoldTextChunk>),
}

/// A completed foldtext computation tied to its request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldTextResult {
    /// Request this result answers.
    pub request: FoldTextRequest,
    /// Host-rendered fold text.
    pub text: FoldText,
}

/// A rejected fold operation or host result.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FoldError {
    /// A half-open fold must contain at least one position.
    #[error("fold range is empty")]
    EmptyRange,
    /// Indent folding cannot divide by a zero shift width.
    #[error("fold shift width must be greater than zero")]
    ZeroShiftWidth,
    /// Start and end marker byte strings must both be non-empty.
    #[error("fold markers must be non-empty")]
    EmptyMarker,
    /// Manual folds may only be changed while the manual method is active.
    #[error("manual fold operation requires the manual fold method")]
    NotManual,
    /// The normalized manual range already exists.
    #[error("fold range already exists")]
    DuplicateRange,
    /// No fold contains the requested position or matches the requested range.
    #[error("no matching fold")]
    NoFold,
    /// Host ranges crossed instead of being disjoint or nested.
    #[error("fold ranges cross and cannot form a deterministic nesting tree")]
    CrossingRanges,
    /// A host result does not match the currently active host method.
    #[error("fold result is for a different fold method")]
    WrongHostMethod,
    /// A host result was computed for an obsolete text generation.
    #[error("fold result is stale")]
    StaleResult,
}

/// Buffer-local fold data and lazy computation state.
#[derive(Clone, Debug)]
pub struct Folds {
    method: FoldMethod,
    shift_width: usize,
    marker_start: Vec<u8>,
    marker_end: Vec<u8>,
    manual: Vec<Fold>,
    computed: Vec<Fold>,
    changedtick: Option<u64>,
    requested_tick: Option<u64>,
    dirty: bool,
}

impl Default for Folds {
    fn default() -> Self {
        Self::new()
    }
}

impl Folds {
    /// Creates empty manual fold state with an eight-column shift width.
    #[must_use]
    pub fn new() -> Self {
        Self {
            method: FoldMethod::Manual,
            shift_width: 8,
            marker_start: b"{{{".to_vec(),
            marker_end: b"}}}".to_vec(),
            manual: Vec::new(),
            computed: Vec::new(),
            changedtick: None,
            requested_tick: None,
            dirty: true,
        }
    }

    /// Returns the active fold method.
    #[must_use]
    pub const fn method(&self) -> FoldMethod {
        self.method
    }

    /// Selects a fold method and invalidates computed data.
    pub fn set_method(&mut self, method: FoldMethod) {
        if self.method != method {
            self.method = method;
            self.computed.clear();
            self.changedtick = None;
            self.requested_tick = None;
            self.dirty = true;
        }
    }

    /// Returns the indentation shift width.
    #[must_use]
    pub const fn shift_width(&self) -> usize {
        self.shift_width
    }

    /// Sets the nonzero indentation shift width.
    pub fn set_shift_width(&mut self, shift_width: usize) -> Result<(), FoldError> {
        if shift_width == 0 {
            return Err(FoldError::ZeroShiftWidth);
        }
        if self.shift_width != shift_width {
            self.shift_width = shift_width;
            if self.method == FoldMethod::Indent {
                self.dirty = true;
            }
        }
        Ok(())
    }

    /// Returns the marker byte strings as `(start, end)`.
    #[must_use]
    pub fn markers(&self) -> (&[u8], &[u8]) {
        (&self.marker_start, &self.marker_end)
    }

    /// Sets non-empty marker byte strings.
    pub fn set_markers(
        &mut self,
        start: impl Into<Vec<u8>>,
        end: impl Into<Vec<u8>>,
    ) -> Result<(), FoldError> {
        let start = start.into();
        let end = end.into();
        if start.is_empty() || end.is_empty() {
            return Err(FoldError::EmptyMarker);
        }
        if self.marker_start != start || self.marker_end != end {
            self.marker_start = start;
            self.marker_end = end;
            if self.method == FoldMethod::Marker {
                self.dirty = true;
            }
        }
        Ok(())
    }

    /// Marks computed folds stale for `changedtick`.
    ///
    /// The next [`Self::refresh`] performs local computation or returns a host
    /// request. This mirrors Neovim's postponed full update in
    /// `src/nvim/fold.c:820-829`.
    pub fn invalidate(&mut self, changedtick: u64) {
        self.requested_tick = Some(changedtick);
        self.dirty = true;
    }

    /// Adjusts persistent manual fold rows through a line replacement.
    ///
    /// Starts use right gravity and half-open ends use left gravity, so text
    /// inserted inside a fold extends it while text inserted at its exclusive
    /// end does not. Ranges fully consumed by deletion are removed.
    pub(crate) fn splice_rows(
        &mut self,
        start: usize,
        old_rows: usize,
        new_rows: usize,
    ) -> Result<(), FoldError> {
        if self.method != FoldMethod::Manual {
            return Ok(());
        }
        for fold in &mut self.manual {
            fold.range.start.row = splice_row(
                fold.range.start.row,
                start,
                old_rows,
                new_rows,
                true,
            );
            fold.range.end.row = splice_row(
                fold.range.end.row,
                start,
                old_rows,
                new_rows,
                false,
            );
        }
        self.manual.retain(|fold| fold.range.start < fold.range.end);
        normalize_folds(&mut self.manual)
    }

    /// Returns whether active fold data needs refreshing.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns the changedtick represented by the active cache.
    #[must_use]
    pub const fn cached_changedtick(&self) -> Option<u64> {
        self.changedtick
    }

    /// Returns active folds in deterministic outer-before-inner order.
    #[must_use]
    pub fn folds(&self) -> &[Fold] {
        self.active()
    }

    /// Lazily refreshes local methods or returns typed host work.
    ///
    /// Each line is treated as bytes. Indentation counts ASCII spaces as one
    /// column and advances an ASCII tab to the next shift-width boundary.
    pub fn refresh<B: AsRef<[u8]>>(
        &mut self,
        changedtick: u64,
        lines: &[B],
    ) -> Result<FoldRefresh, FoldError> {
        self.requested_tick = Some(changedtick);
        if !self.dirty && self.changedtick == Some(changedtick) {
            return Ok(FoldRefresh::Ready {
                changedtick,
                fold_count: self.active().len(),
            });
        }

        match self.method {
            FoldMethod::Manual => {
                self.changedtick = Some(changedtick);
                self.dirty = false;
            }
            FoldMethod::Indent => {
                let levels = indent_levels(lines, self.shift_width)?;
                self.replace_computed(ranges_from_levels(&levels)?)?;
                self.changedtick = Some(changedtick);
                self.dirty = false;
            }
            FoldMethod::Marker => {
                let levels = marker_levels(lines, &self.marker_start, &self.marker_end);
                self.replace_computed(ranges_from_levels(&levels)?)?;
                self.changedtick = Some(changedtick);
                self.dirty = false;
            }
            FoldMethod::Expr | FoldMethod::Syntax | FoldMethod::Diff => {
                let kind = match self.method.host_kind() {
                    Some(kind) => kind,
                    None => return Err(FoldError::WrongHostMethod),
                };
                return Ok(FoldRefresh::Host(FoldComputeRequest {
                    kind,
                    changedtick,
                    line_count: lines.len(),
                }));
            }
        }

        Ok(FoldRefresh::Ready {
            changedtick,
            fold_count: self.active().len(),
        })
    }

    /// Applies expr, syntax, or diff ranges returned by the host.
    pub fn apply_host_result(&mut self, result: FoldComputeResult) -> Result<(), FoldError> {
        let expected_kind = match self.method.host_kind() {
            Some(kind) => kind,
            None => return Err(FoldError::WrongHostMethod),
        };
        if result.request.kind != expected_kind {
            return Err(FoldError::WrongHostMethod);
        }
        if self.requested_tick != Some(result.request.changedtick) {
            return Err(FoldError::StaleResult);
        }

        self.replace_computed(result.ranges)?;
        self.changedtick = Some(result.request.changedtick);
        self.dirty = false;
        Ok(())
    }

    /// Creates a normalized manual fold, initially closed.
    ///
    /// Crossing existing folds expand the new range until the set is properly
    /// nested, matching Neovim's enclosing normalization in
    /// `src/nvim/fold.c:564-655`.
    pub fn create_manual(
        &mut self,
        start: Position,
        end: Position,
    ) -> Result<FoldRange, FoldError> {
        self.require_manual()?;
        let mut range = FoldRange::normalized(start, end)?;

        loop {
            let mut expanded = false;
            for fold in &self.manual {
                if fold.range == range {
                    return Err(FoldError::DuplicateRange);
                }
                if fold.range.overlaps(range)
                    && !fold.range.contains_range(range)
                    && !range.contains_range(fold.range)
                {
                    range = range.union(fold.range);
                    expanded = true;
                }
            }
            if !expanded {
                break;
            }
        }
        if self.manual.iter().any(|fold| fold.range == range) {
            return Err(FoldError::DuplicateRange);
        }

        self.manual.push(Fold {
            range,
            depth: 1,
            state: FoldState::Closed,
        });
        normalize_folds(&mut self.manual)?;
        Ok(range)
    }

    /// Deletes one exact manual fold, retaining its nested folds.
    pub fn delete_manual(&mut self, range: FoldRange) -> Result<Fold, FoldError> {
        self.require_manual()?;
        let index = self
            .manual
            .iter()
            .position(|fold| fold.range == range)
            .ok_or(FoldError::NoFold)?;
        let removed = self.manual.remove(index);
        normalize_folds(&mut self.manual)?;
        Ok(removed)
    }

    /// Deletes the deepest manual fold containing `position`.
    ///
    /// With `recursive`, descendants are deleted too. Without it, descendants
    /// remain and are promoted, corresponding to Neovim's recursive deletion
    /// switch at `src/nvim/fold.c:662-725`.
    pub fn delete_manual_at(
        &mut self,
        position: Position,
        recursive: bool,
    ) -> Result<Vec<Fold>, FoldError> {
        self.require_manual()?;
        let target = self
            .manual
            .iter()
            .rfind(|fold| fold.range.contains(position))
            .map(|fold| fold.range)
            .ok_or(FoldError::NoFold)?;

        let mut removed = Vec::new();
        self.manual.retain(|fold| {
            let delete = fold.range == target
                || (recursive
                    && target.contains_range(fold.range)
                    && target != fold.range);
            if delete {
                removed.push(*fold);
            }
            !delete
        });
        normalize_folds(&mut self.manual)?;
        Ok(removed)
    }

    /// Removes every manual fold.
    pub fn clear_manual(&mut self) -> Result<usize, FoldError> {
        self.require_manual()?;
        let count = self.manual.len();
        self.manual.clear();
        Ok(count)
    }

    /// Returns all folds containing a position, outermost first.
    pub fn containing_folds(
        &self,
        position: Position,
    ) -> impl DoubleEndedIterator<Item = &Fold> {
        self.active()
            .iter()
            .filter(move |fold| fold.range.contains(position))
    }

    /// Returns the deepest fold containing a position.
    ///
    /// Neovim descends through every containing fold in
    /// `src/nvim/fold.c:1086-1107` and uses the deepest match for deletion in
    /// `src/nvim/fold.c:681-707`.
    #[must_use]
    pub fn deepest_fold(&self, position: Position) -> Option<&Fold> {
        self.active()
            .iter()
            .rev()
            .find(|fold| fold.range.contains(position))
    }

    /// Returns the nesting level at a position.
    #[must_use]
    pub fn level_at(&self, position: Position) -> usize {
        self.active()
            .iter()
            .filter(|fold| fold.range.contains(position))
            .count()
    }

    /// Returns the first and last zero-based rows of the closed fold covering
    /// `row`, or `None` when no fold covering it is closed.
    ///
    /// `hasFoldingWin` (`src/nvim/fold.c:173-263`) descends from the outermost
    /// fold and stops at the first closed one, so a closed fold nested inside
    /// an open one is reported while an open fold nested inside a closed one is
    /// not: the answer is always the outermost closed fold covering `row`.
    #[must_use]
    pub fn closed_rows_at(&self, row: usize) -> Option<(usize, usize)> {
        self.active()
            .iter()
            .find(|fold| fold.range.covers_row(row) && fold.state == FoldState::Closed)
            .map(|fold| (fold.range.start.row, fold.range.last_row()))
    }

    /// Returns the nesting level at a zero-based row, zero when no fold covers
    /// it.
    ///
    /// `foldLevelWin` (`src/nvim/fold.c:1088-1107`) counts the folds containing
    /// the line whatever their open or closed state.
    #[must_use]
    pub fn level_at_row(&self, row: usize) -> usize {
        self.active()
            .iter()
            .filter(|fold| fold.range.covers_row(row))
            .count()
    }

    /// Returns a foldtext request for an exact closed fold.
    pub fn fold_text_request(
        &self,
        range: FoldRange,
        changedtick: u64,
    ) -> Result<FoldTextRequest, FoldError> {
        let fold = self
            .active()
            .iter()
            .rev()
            .find(|fold| fold.range == range && fold.state == FoldState::Closed)
            .ok_or(FoldError::NoFold)?;
        Ok(FoldTextRequest {
            changedtick,
            range,
            level: fold.depth,
        })
    }

    /// Opens the outermost closed fold containing `position`.
    pub fn open(&mut self, position: Position) -> Result<bool, FoldError> {
        let folds = self.active_mut();
        let mut found = false;
        for fold in folds {
            if fold.range.contains(position) {
                found = true;
                if fold.state == FoldState::Closed {
                    fold.state = FoldState::Open;
                    return Ok(true);
                }
            }
        }
        if found {
            Ok(false)
        } else {
            Err(FoldError::NoFold)
        }
    }

    /// Closes the deepest currently visible open fold containing `position`.
    pub fn close(&mut self, position: Position) -> Result<bool, FoldError> {
        let folds = self.active_mut();
        let mut deepest_open = None;
        let mut found = false;
        for (index, fold) in folds.iter().enumerate() {
            if !fold.range.contains(position) {
                continue;
            }
            found = true;
            if fold.state == FoldState::Closed {
                break;
            }
            deepest_open = Some(index);
        }
        if let Some(index) = deepest_open {
            folds[index].state = FoldState::Closed;
            Ok(true)
        } else if found {
            Ok(false)
        } else {
            Err(FoldError::NoFold)
        }
    }

    /// Opens a closed fold at `position`, otherwise closes the deepest one.
    pub fn toggle(&mut self, position: Position) -> Result<bool, FoldError> {
        let folds = self.active_mut();
        let mut deepest = None;
        let mut first_closed = None;
        for (index, fold) in folds.iter().enumerate() {
            if !fold.range.contains(position) {
                continue;
            }
            if fold.state == FoldState::Closed {
                first_closed = Some(index);
                break;
            }
            deepest = Some(index);
        }
        if let Some(index) = first_closed {
            folds[index].state = FoldState::Open;
            return Ok(true);
        }
        let index = deepest.ok_or(FoldError::NoFold)?;
        folds[index].state = FoldState::Closed;
        Ok(true)
    }

    /// Opens the first closed containing fold and all of its descendants.
    ///
    /// Neovim opens nested folds recursively in `src/nvim/fold.c:1226-1234`
    /// and `src/nvim/fold.c:1267-1275`.
    pub fn open_recursive(&mut self, position: Position) -> Result<usize, FoldError> {
        let folds = self.active_mut();
        let mut deepest = None;
        let mut first_closed = None;
        for (index, fold) in folds.iter().enumerate() {
            if fold.range.contains(position) {
                deepest = Some(index);
                if first_closed.is_none() && fold.state == FoldState::Closed {
                    first_closed = Some(index);
                }
            }
        }
        let target = first_closed.or(deepest).ok_or(FoldError::NoFold)?;
        let target_range = folds[target].range;
        let mut changed = 0;
        for fold in &mut folds[target..] {
            if target_range.contains_range(fold.range) && fold.state == FoldState::Closed {
                fold.state = FoldState::Open;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Closes the outermost fold containing `position`.
    ///
    /// Descendant state is retained so reopening non-recursively remains
    /// deterministic, as in `src/nvim/fold.c:1220-1250`.
    pub fn close_recursive(&mut self, position: Position) -> Result<usize, FoldError> {
        let fold = self
            .active_mut()
            .iter_mut()
            .find(|fold| fold.range.contains(position))
            .ok_or(FoldError::NoFold)?;
        if fold.state == FoldState::Closed {
            Ok(0)
        } else {
            fold.state = FoldState::Closed;
            Ok(1)
        }
    }

    /// Opens every active fold, corresponding to `zR`.
    pub fn open_all(&mut self) -> usize {
        set_all(self.active_mut(), FoldState::Open)
    }

    /// Closes every active fold, corresponding to `zM`.
    pub fn close_all(&mut self) -> usize {
        set_all(self.active_mut(), FoldState::Closed)
    }

    fn require_manual(&self) -> Result<(), FoldError> {
        if self.method == FoldMethod::Manual {
            Ok(())
        } else {
            Err(FoldError::NotManual)
        }
    }

    fn active(&self) -> &[Fold] {
        if self.method == FoldMethod::Manual {
            &self.manual
        } else {
            &self.computed
        }
    }

    fn active_mut(&mut self) -> &mut Vec<Fold> {
        if self.method == FoldMethod::Manual {
            &mut self.manual
        } else {
            &mut self.computed
        }
    }

    fn replace_computed(&mut self, ranges: Vec<FoldRange>) -> Result<(), FoldError> {
        let states: BTreeMap<_, _> = self
            .computed
            .iter()
            .map(|fold| ((fold.range, fold.depth), fold.state))
            .collect();
        let mut folds: Vec<_> = ranges
            .into_iter()
            .map(|range| Fold {
                range,
                depth: 1,
                state: FoldState::Closed,
            })
            .collect();
        normalize_folds(&mut folds)?;
        for fold in &mut folds {
            fold.state = states
                .get(&(fold.range, fold.depth))
                .copied()
                .unwrap_or(FoldState::Closed);
        }
        self.computed = folds;
        Ok(())
    }
}

/// Computes indent fold levels for byte lines.
///
/// Neovim reports an undefined level for blank/ignored lines and otherwise
/// divides indentation by shift width in `src/nvim/fold.c:2841-2862`, then
/// skips over undefined lines to resolve a fold level in
/// `src/nvim/fold.c:2410-2425`. Per `src/nvim/fold.txt:54-61` such lines take
/// the level of the line above or below, whichever is lower. Here an interior
/// blank line resolves to the lower of the nearest concrete level above and
/// below (looking across runs of blanks); the first and last lines are never
/// undefined and stay at level zero (`src/nvim/fold.c:2852-2854`).
pub fn indent_levels<B: AsRef<[u8]>>(
    lines: &[B],
    shift_width: usize,
) -> Result<Vec<usize>, FoldError> {
    if shift_width == 0 {
        return Err(FoldError::ZeroShiftWidth);
    }
    let mut raw = Vec::with_capacity(lines.len());
    for line in lines {
        let bytes = line.as_ref();
        let mut columns = 0usize;
        let mut blank = true;
        for &byte in bytes {
            match byte {
                b' ' => columns = columns.saturating_add(1),
                b'\t' => {
                    let remainder = columns % shift_width;
                    columns = columns.saturating_add(shift_width - remainder);
                }
                b'\r' | b'\n' => {}
                _ => {
                    blank = false;
                    break;
                }
            }
        }
        raw.push((!blank).then_some((columns / shift_width).min(MAX_FOLD_DEPTH)));
    }

    let line_count = raw.len();
    let mut levels = vec![0usize; line_count];
    let mut previous = 0usize;
    // Forward pass: for each blank seed the nearest concrete level above
    // (0 before the buffer start).
    for (index, level) in raw.iter().copied().enumerate() {
        if let Some(lvl) = level {
            previous = lvl;
            levels[index] = lvl;
        } else {
            levels[index] = previous;
        }
    }
    // Backward pass: take the lower of the level above and the nearest
    // concrete level below (0 past the buffer end), which is how a blank
    // run inside a fold resolves.
    let mut next = 0usize;
    for (index, level) in raw.iter().copied().enumerate().rev() {
        if let Some(lvl) = level {
            next = lvl;
        } else {
            levels[index] = levels[index].min(next);
        }
    }
    Ok(levels)
}

fn marker_levels<B: AsRef<[u8]>>(lines: &[B], start: &[u8], end: &[u8]) -> Vec<usize> {
    let mut level = 0usize;
    let mut levels = Vec::with_capacity(lines.len());
    for line in lines {
        let bytes = line.as_ref();
        let mut offset = 0usize;
        let mut next_level = level;
        let mut line_level = level;
        while offset < bytes.len() {
            let start_at = find_bytes(&bytes[offset..], start).map(|at| offset + at);
            let end_at = find_bytes(&bytes[offset..], end).map(|at| offset + at);
            match (start_at, end_at) {
                (None, None) => break,
                (Some(at), None) => {
                    let explicit = decimal_after(bytes, at + start.len());
                    next_level = explicit
                        .unwrap_or_else(|| next_level.saturating_add(1))
                        .min(MAX_FOLD_DEPTH);
                    line_level = line_level.max(next_level);
                    offset = at + start.len();
                }
                (Some(start_at), Some(end_at)) if start_at <= end_at => {
                    let explicit = decimal_after(bytes, start_at + start.len());
                    next_level = explicit
                        .unwrap_or_else(|| next_level.saturating_add(1))
                        .min(MAX_FOLD_DEPTH);
                    line_level = line_level.max(next_level);
                    offset = start_at + start.len();
                }
                (_, Some(at)) => {
                    line_level = line_level.max(next_level);
                    let explicit = decimal_after(bytes, at + end.len());
                    next_level = explicit
                        .map(|value| value.saturating_sub(1))
                        .unwrap_or_else(|| next_level.saturating_sub(1));
                    offset = at + end.len();
                }
            }
        }
        levels.push(line_level.min(MAX_FOLD_DEPTH));
        level = next_level.min(MAX_FOLD_DEPTH);
    }
    levels
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decimal_after(bytes: &[u8], start: usize) -> Option<usize> {
    let mut value = 0usize;
    let mut found = false;
    for &byte in bytes.get(start..)? {
        if !byte.is_ascii_digit() {
            break;
        }
        found = true;
        value = value
            .saturating_mul(10)
            .saturating_add(usize::from(byte - b'0'));
    }
    found.then_some(value)
}

fn ranges_from_levels(levels: &[usize]) -> Result<Vec<FoldRange>, FoldError> {
    let max_level = levels.iter().copied().max().unwrap_or(0).min(MAX_FOLD_DEPTH);
    let mut ranges = Vec::new();
    for depth in 1..=max_level {
        let mut start = None;
        for row in 0..=levels.len() {
            let folded = levels.get(row).is_some_and(|&level| level >= depth);
            match (start, folded) {
                (None, true) => start = Some(row),
                (Some(start_row), false) => {
                    ranges.push(FoldRange::lines(start_row, row)?);
                    start = None;
                }
                _ => {}
            }
        }
    }
    Ok(ranges)
}

fn normalize_folds(folds: &mut Vec<Fold>) -> Result<(), FoldError> {
    folds.sort_by(|left, right| compare_ranges(left.range, right.range));
    let mut stack: Vec<FoldRange> = Vec::new();
    for fold in folds {
        while let Some(candidate) = stack.last().copied() {
            if candidate.contains_range(fold.range) {
                break;
            }
            if candidate.overlaps(fold.range) {
                return Err(FoldError::CrossingRanges);
            }
            stack.pop();
        }
        fold.depth = stack.len() + 1;
        stack.push(fold.range);
    }
    Ok(())
}

fn compare_ranges(left: FoldRange, right: FoldRange) -> Ordering {
    left.start
        .cmp(&right.start)
        .then_with(|| right.end.cmp(&left.end))
}

fn splice_row(
    row: usize,
    start: usize,
    old_rows: usize,
    new_rows: usize,
    right_gravity: bool,
) -> usize {
    let old_end = start.saturating_add(old_rows);
    let new_end = start.saturating_add(new_rows);
    if old_rows == 0 {
        if row > start || (row == start && right_gravity) {
            return row.saturating_add(new_rows);
        }
        return row;
    }
    if row < start || (row == start && !right_gravity) {
        return row;
    }
    if row > old_end || (row == old_end && right_gravity) {
        return new_end.saturating_add(row.saturating_sub(old_end));
    }
    if right_gravity {
        new_end
    } else {
        start
    }
}

fn set_all(folds: &mut [Fold], state: FoldState) -> usize {
    let mut changed = 0;
    for fold in folds {
        if fold.state != state {
            fold.state = state;
            changed += 1;
        }
    }
    changed
}
