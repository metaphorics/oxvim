//! Mark positions and line-splice adjustment.

use std::collections::BTreeMap;

/// Stable caller-assigned mark identifier.
pub type MarkId = u64;

/// A byte-column position in a one-based line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// One-based line number.
    pub lnum: usize,
    /// Zero-based byte column.
    pub col: usize,
}

/// Registry of positions adjusted as logical lines are spliced.
#[derive(Clone, Debug, Default)]
pub struct Marks {
    positions: BTreeMap<MarkId, Position>,
}

impl Marks {
    /// Creates an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            positions: BTreeMap::new(),
        }
    }

    /// Inserts or replaces a mark.
    pub fn set(&mut self, id: MarkId, position: Position) -> Option<Position> {
        self.positions.insert(id, position)
    }

    /// Gets a mark.
    #[must_use]
    pub fn get(&self, id: MarkId) -> Option<Position> {
        self.positions.get(&id).copied()
    }

    /// Removes a mark.
    pub fn remove(&mut self, id: MarkId) -> Option<Position> {
        self.positions.remove(&id)
    }

    /// Iterates in mark-id order.
    pub fn iter(&self) -> impl Iterator<Item = (MarkId, Position)> + '_ {
        self.positions.iter().map(|(&id, &position)| (id, position))
    }

    /// Adjusts all marks for replacement of `old_count` lines at `start`.
    ///
    /// Marks before the splice stay fixed. Marks after it shift by the line
    /// delta. A mark on a replaced line follows its corresponding replacement
    /// line where possible; marks in deleted overflow clamp to the splice line
    /// at column zero. This is the line-level primitive used before any later
    /// byte-column `mb_splice` adjustment by the editor.
    pub fn splice(&mut self, start: usize, old_count: usize, new_count: usize) {
        let old_end = start.saturating_add(old_count);
        for position in self.positions.values_mut() {
            if position.lnum < start {
                continue;
            }
            if position.lnum >= old_end {
                position.lnum = shift(position.lnum, new_count, old_count);
                continue;
            }
            let relative = position.lnum - start;
            if relative < new_count {
                position.lnum = start + relative;
            } else {
                position.lnum = start;
                position.col = 0;
            }
        }
    }
}

fn shift(value: usize, added: usize, removed: usize) -> usize {
    if added >= removed {
        value.saturating_add(added - removed)
    } else {
        value.saturating_sub(removed - added).max(1)
    }
}
