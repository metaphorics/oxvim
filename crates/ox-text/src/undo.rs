//! Explicit sequence-numbered branching undo history.

use thiserror::Error;

/// Cursor saved with an undo entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// One-based line.
    pub lnum: usize,
    /// Zero-based byte column.
    pub col: usize,
}

/// One line-range replacement and its inverse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineEdit {
    /// One-based first affected line.
    pub start: usize,
    /// Lines present before the edit.
    pub before: Vec<Vec<u8>>,
    /// Lines present after the edit.
    pub after: Vec<Vec<u8>>,
    /// Cursor before the edit.
    pub cursor_before: Cursor,
    /// Cursor after the edit.
    pub cursor_after: Cursor,
}

/// A recorded undo header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoEntry {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// Edit represented by this header.
    pub edit: LineEdit,
}

/// Direction and entry returned to the buffer owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoStep {
    /// Apply `before` in place of `after`.
    Undo(UndoEntry),
    /// Apply `after` in place of `before`.
    Redo(UndoEntry),
}

/// Undo-tree navigation errors.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum UndoError {
    /// No older state exists.
    #[error("already at the oldest change")]
    AtOldest,
    /// No newer branch exists.
    #[error("already at the newest change")]
    AtNewest,
    /// A branch index was invalid.
    #[error("redo branch {requested} is outside 0..{available}")]
    Branch {
        /// Requested zero-based branch.
        requested: usize,
        /// Available branch count.
        available: usize,
    },
    /// The requested sequence does not exist.
    #[error("undo sequence {0} does not exist")]
    UnknownSequence(u64),
}

#[derive(Clone, Debug)]
struct Node {
    entry: Option<UndoEntry>,
    parent: Option<usize>,
    children: Vec<usize>,
    preferred_child: Option<usize>,
}

/// An explicit branch-preserving undo tree.
#[derive(Clone, Debug)]
pub struct UndoTree {
    nodes: Vec<Node>,
    current: usize,
    next_seq: u64,
}

impl Default for UndoTree {
    fn default() -> Self {
        Self::new()
    }
}

impl UndoTree {
    /// Creates a tree at sequence zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                entry: None,
                parent: None,
                children: Vec::new(),
                preferred_child: None,
            }],
            current: 0,
            next_seq: 1,
        }
    }

    /// Returns the current sequence, or zero at the root.
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.nodes[self.current]
            .entry
            .as_ref()
            .map_or(0, |entry| entry.seq)
    }

    /// Records an edit as a child of the current state.
    pub fn record(&mut self, edit: LineEdit, timestamp: i64) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let node_index = self.nodes.len();
        let child_index = self.nodes[self.current].children.len();
        self.nodes.push(Node {
            entry: Some(UndoEntry {
                seq,
                timestamp,
                edit,
            }),
            parent: Some(self.current),
            children: Vec::new(),
            preferred_child: None,
        });
        self.nodes[self.current].children.push(node_index);
        self.nodes[self.current].preferred_child = Some(child_index);
        self.current = node_index;
        seq
    }

    /// Moves to the parent state.
    pub fn undo(&mut self) -> Result<UndoStep, UndoError> {
        let parent = self.nodes[self.current]
            .parent
            .ok_or(UndoError::AtOldest)?;
        let entry = self.nodes[self.current]
            .entry
            .clone()
            .ok_or(UndoError::AtOldest)?;
        if let Some(index) = self.nodes[parent]
            .children
            .iter()
            .position(|&child| child == self.current)
        {
            self.nodes[parent].preferred_child = Some(index);
        }
        self.current = parent;
        Ok(UndoStep::Undo(entry))
    }

    /// Reapplies the preferred child branch.
    pub fn redo(&mut self) -> Result<UndoStep, UndoError> {
        let branch = self.nodes[self.current]
            .preferred_child
            .unwrap_or_else(|| self.nodes[self.current].children.len().saturating_sub(1));
        self.redo_branch(branch)
    }

    /// Reapplies a selected zero-based child branch.
    pub fn redo_branch(&mut self, branch: usize) -> Result<UndoStep, UndoError> {
        let available = self.nodes[self.current].children.len();
        let child = *self.nodes[self.current]
            .children
            .get(branch)
            .ok_or(if available == 0 {
                UndoError::AtNewest
            } else {
                UndoError::Branch {
                    requested: branch,
                    available,
                }
            })?;
        self.nodes[self.current].preferred_child = Some(branch);
        self.current = child;
        let entry = self.nodes[child]
            .entry
            .clone()
            .ok_or(UndoError::AtNewest)?;
        Ok(UndoStep::Redo(entry))
    }

    /// Returns child sequence numbers in branch order.
    #[must_use]
    pub fn branches(&self) -> Vec<u64> {
        self.nodes[self.current]
            .children
            .iter()
            .filter_map(|&child| self.nodes[child].entry.as_ref().map(|entry| entry.seq))
            .collect()
    }

    /// Navigates to an arbitrary sequence, returning edits in application order.
    pub fn undo_to_seq(&mut self, seq: u64) -> Result<Vec<UndoStep>, UndoError> {
        let target = if seq == 0 {
            0
        } else {
            self.nodes
                .iter()
                .position(|node| node.entry.as_ref().is_some_and(|entry| entry.seq == seq))
                .ok_or(UndoError::UnknownSequence(seq))?
        };
        let current_path = self.path_to_root(self.current);
        let target_path = self.path_to_root(target);
        let common = current_path
            .iter()
            .find(|node| target_path.contains(node))
            .copied()
            .unwrap_or(0);
        let mut steps = Vec::new();
        while self.current != common {
            steps.push(self.undo()?);
        }
        let mut down: Vec<usize> = target_path
            .into_iter()
            .take_while(|&node| node != common)
            .collect();
        down.reverse();
        for child in down {
            let branch = self.nodes[self.current]
                .children
                .iter()
                .position(|&candidate| candidate == child)
                .ok_or(UndoError::UnknownSequence(seq))?;
            steps.push(self.redo_branch(branch)?);
        }
        Ok(steps)
    }

    fn path_to_root(&self, mut node: usize) -> Vec<usize> {
        let mut path = Vec::new();
        loop {
            path.push(node);
            if let Some(parent) = self.nodes[node].parent {
                node = parent;
            } else {
                break;
            }
        }
        path
    }
}
