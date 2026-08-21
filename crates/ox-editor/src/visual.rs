//! Visual selection state (`visual.c`).

use ox_text::Position;

use crate::{EditRange, MotionKind};

/// Shape of an active visual selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisualKind {
    /// Inclusive characterwise selection.
    Character,
    /// Complete-line selection.
    Line,
    /// Rectangular byte-column selection.
    Block,
}

/// Anchor and active endpoint of a visual selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualState {
    /// Fixed selection endpoint.
    pub anchor: Position,
    /// Active cursor endpoint.
    pub cursor: Position,
    /// Selection shape.
    pub kind: VisualKind,
    /// Count accumulated before a visual motion.
    pub count: usize,
    /// Incomplete `g`, find, or text-object prefix.
    pub prefix: String,
    /// Virtual column the active endpoint holds during vertical block motions,
    /// tracked independently of the clamped byte cursor.  Block visual keeps this
    /// edge column (possibly beyond a shorter line's end) so a ragged rectangle
    /// keeps its width (`ops.c:2223-2231`).
    pub wanted: Option<usize>,
}

impl VisualState {
    /// Starts a selection at `anchor`.
    #[must_use] pub fn new(anchor: Position, kind: VisualKind) -> Self { Self { anchor, cursor: anchor, kind, count: 0, prefix: String::new(), wanted: (kind == VisualKind::Block).then_some(anchor.col) } }
    /// Extends the active endpoint.
    pub fn extend(&mut self, cursor: Position) { self.cursor = cursor; }
    /// Extends the active endpoint in a block, preserving the wanted column across
    /// vertical motions and adopting the new column on horizontal motions.
    pub fn extend_block(&mut self, target: Position, from: Position) {
        if target.lnum == from.lnum {
            self.wanted = Some(target.col);
            self.cursor = target;
        } else {
            self.cursor.lnum = target.lnum;
            self.cursor.col = self.wanted.unwrap_or(self.cursor.col);
        }
    }
    /// Exchanges the active and fixed endpoints.
    pub fn swap_ends(&mut self) { std::mem::swap(&mut self.anchor, &mut self.cursor); if self.kind == VisualKind::Block { self.wanted = Some(self.cursor.col); } }
    /// Exchanges only endpoint columns, moving to the other block corner on the active row.
    pub fn swap_columns(&mut self) { std::mem::swap(&mut self.anchor.col, &mut self.cursor.col); self.wanted = Some(self.cursor.col); }
    /// Converts the selection into normalized operator endpoints.
    #[must_use]
    pub fn range(&self) -> EditRange {
        let (mut start, mut end) = if (self.anchor.lnum, self.anchor.col) <= (self.cursor.lnum, self.cursor.col) { (self.anchor, self.cursor) } else { (self.cursor, self.anchor) };
        let kind = match self.kind { VisualKind::Character => MotionKind::CharacterWise, VisualKind::Line => { start.col = 0; MotionKind::LineWise }, VisualKind::Block => { if start.col > end.col { std::mem::swap(&mut start.col, &mut end.col); } MotionKind::BlockWise } };
        EditRange { start, end, kind, inclusive: true }
    }
}
