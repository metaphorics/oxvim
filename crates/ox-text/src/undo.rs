//! Explicit sequence-numbered branching undo history.
//!
//! One node is one undo *header*, and a header carries the list of edits
//! that belong to one undo block. Upstream groups every change made without
//! returning to the main loop into a single header: `u_savecommon` only
//! allocates a new header when `b_u_synced` is true and otherwise pushes
//! another entry onto the open one (`undo.c:388-500`), clearing the flag on
//! the way out (`undo.c:616`). `u_sync` closes the open header
//! (`undo.c:2704-2717`), and `may_sync_undo` calls it when the editor
//! consumes a typed key (`input.c:1255,1300-1306`).
//!
//! [`UndoTree`] owns that flag so no caller can forget it: [`UndoTree::record`]
//! joins the open header, [`UndoTree::sync`] closes it, and every navigation
//! entry point closes it the way `u_undoredo` does (`undo.c:1665`).

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
///
/// `edits` are in the order they were applied, so undoing walks them in
/// reverse and redoing walks them forwards. Upstream keeps the same list on
/// `uh_entry` (`undo.c:610-611`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoEntry {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// Edits this header groups, in application order.
    pub edits: Vec<LineEdit>,
}

impl UndoEntry {
    /// Cursor to restore when this header is undone: the position before its
    /// first edit, which is upstream's header-level `uh_cursor`.
    #[must_use]
    pub fn cursor_before(&self) -> Cursor {
        self.edits.first().map_or(Cursor::default(), |edit| edit.cursor_before)
    }

    /// Cursor to restore when this header is redone: the position after its
    /// last edit.
    #[must_use]
    pub fn cursor_after(&self) -> Cursor {
        self.edits.last().map_or(Cursor::default(), |edit| edit.cursor_after)
    }
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
    /// `:undojoin` was used after an undo, which upstream refuses because
    /// the header it would join is the one an undo moved off
    /// (`ex_undojoin`, `undo.c:2805-2807`).
    #[error("undojoin is not allowed after undo")]
    JoinAfterUndo,
}

#[derive(Clone, Debug)]
struct Node {
    entry: Option<UndoEntry>,
    parent: Option<usize>,
    children: Vec<usize>,
    preferred_child: Option<usize>,
}

/// Upstream-compatible link coordinates for one undo header.
///
/// Mirrors the four `uh_*` sequence links plus the fields Neovim stores
/// per header. `next` points to the older (parent) header, `prev` to the
/// newer continuation along the active branch; `alt_next`/`alt_prev` chain
/// the inactive sibling branches. Zero means "no link".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderRecord {
    /// Header sequence number.
    pub seq: u64,
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// The edits this header groups, in application order.
    pub edits: Vec<LineEdit>,
    /// Link to the older header.
    pub next: u64,
    /// Link to the newer active header.
    pub prev: u64,
    /// Link to the first inactive sibling header.
    pub alt_next: u64,
    /// Link to the previous inactive sibling header.
    pub alt_prev: u64,
}

/// One header as `undotree()` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoTreeNode {
    /// Header sequence number.
    pub seq: u64,
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// Whether this is the newest header on the active branch
    /// (`b_u_newhead`).
    pub newhead: bool,
    /// Whether this is the header an undo moved off (`b_u_curhead`).
    pub curhead: bool,
    /// Inactive branches hanging off this header.
    pub alt: Vec<UndoTreeNode>,
}

/// Tree-wide fields required by the persistent-undo top-level header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UndoSummary {
    /// Sequence of the oldest header on the active branch.
    pub oldhead: u64,
    /// Sequence of the newest header on the active branch.
    pub newhead: u64,
    /// Sequence of the current header, or zero when at the newest state.
    pub curhead: u64,
    /// Highest sequence number ever allocated.
    pub seq_last: u64,
    /// Current sequence number.
    pub seq_cur: u64,
    /// Timestamp of the current state.
    pub time_cur: i64,
}

/// An explicit branch-preserving undo tree.
#[derive(Clone, Debug)]
pub struct UndoTree {
    nodes: Vec<Node>,
    current: usize,
    next_seq: u64,
    /// Upstream's `b_u_synced`: true when the newest header is closed, so the
    /// next recorded edit starts a new one (`buffer_defs.h:499`).
    synced: bool,
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
            synced: true,
        }
    }

    /// Whether the newest header is closed (upstream `b_u_synced`).
    #[must_use]
    pub const fn is_synced(&self) -> bool {
        self.synced
    }

    /// Closes the open header so the next edit starts a new undo block.
    ///
    /// This is `u_sync` (`undo.c:2704-2717`). It is idempotent, which is why
    /// upstream can call it from every boundary without counting.
    pub const fn sync(&mut self) {
        self.synced = true;
    }

    /// Number of edits grouped into the current header, zero at the root.
    ///
    /// Together with [`UndoTree::current_seq`] this identifies the buffer
    /// state even while a header is still open and growing.
    #[must_use]
    pub fn current_block_len(&self) -> usize {
        self.nodes[self.current]
            .entry
            .as_ref()
            .map_or(0, |entry| entry.edits.len())
    }

    /// Reopens the newest header so the next edit joins it (`:undojoin`,
    /// `ex_undojoin`, `undo.c:2800-2816`).
    ///
    /// Nothing recorded yet, or an already-open header, is a silent no-op
    /// upstream; only a `:undojoin` that follows an undo is an error.
    pub fn undojoin(&mut self) -> Result<(), UndoError> {
        if !self.synced {
            return Ok(());
        }
        if self.nodes[self.current].entry.is_none() {
            // `b_u_newhead == NULL`: nothing changed before, or an undo walked
            // all the way back to the original state.
            return if self.next_seq > 1 {
                Err(UndoError::JoinAfterUndo)
            } else {
                Ok(())
            };
        }
        if self.active_child(self.current).is_some() {
            // `b_u_curhead != NULL`: an undo left a header ahead of us.
            return Err(UndoError::JoinAfterUndo);
        }
        self.synced = false;
        Ok(())
    }

    /// Returns the current sequence, or zero at the root.
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.nodes[self.current]
            .entry
            .as_ref()
            .map_or(0, |entry| entry.seq)
    }

    /// Records an edit, starting a new header or joining the open one.
    ///
    /// Returns the sequence of the header the edit landed in, which is the
    /// existing one whenever a block is still open.
    pub fn record(&mut self, edit: LineEdit, timestamp: i64) -> u64 {
        if !self.synced {
            if let Some(entry) = self.nodes[self.current].entry.as_mut() {
                entry.edits.push(edit);
                return entry.seq;
            }
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let node_index = self.nodes.len();
        let child_index = self.nodes[self.current].children.len();
        self.nodes.push(Node {
            entry: Some(UndoEntry {
                seq,
                timestamp,
                edits: vec![edit],
            }),
            parent: Some(self.current),
            children: Vec::new(),
            preferred_child: None,
        });
        self.nodes[self.current].children.push(node_index);
        self.nodes[self.current].preferred_child = Some(child_index);
        self.current = node_index;
        self.synced = false;
        seq
    }

    /// Moves to the parent state, closing the open block first.
    ///
    /// `u_undo` syncs before undoing so an open block is undone whole
    /// (`undo.c:1825-1828`), and `u_undoredo` leaves the tree synced
    /// (`undo.c:1665`); both live here so the flag cannot be left stale.
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
        self.synced = true;
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
        self.synced = true;
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

    /// The child of `node` that continues the active branch.
    ///
    /// Prefers the recorded preferred child; when navigation left none set
    /// (only possible for an untouched root), the most recently created
    /// child is the active one, matching upstream's incremental builder.
    fn active_child(&self, node: usize) -> Option<usize> {
        match self.nodes[node].preferred_child {
            Some(index) => self.nodes[node].children.get(index).copied(),
            None => self.nodes[node].children.last().copied(),
        }
    }

    fn seq_of(&self, node: usize) -> u64 {
        self.nodes[node]
            .entry
            .as_ref()
            .map_or(0, |entry| entry.seq)
    }

    /// The alternate-branch sibling links, by node index.
    ///
    /// `u_addbranch` hangs every inactive sibling off the active child:
    /// `alt_next` of the active child is the newest inactive sibling, and each
    /// inactive sibling's `alt_next` is the next older one. `alt_prev` walks
    /// back the same chain.
    fn alt_links(&self) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
        let mut alt_next = vec![None; self.nodes.len()];
        let mut alt_prev = vec![None; self.nodes.len()];
        for parent in 0..self.nodes.len() {
            let mut inactive: Vec<usize> = self.nodes[parent]
                .children
                .iter()
                .copied()
                .filter(|&child| Some(child) != self.active_child(parent))
                .collect();
            if inactive.is_empty() {
                continue;
            }
            // Newest (highest index) inactive sibling first.
            inactive.sort_unstable();
            inactive.reverse();
            if let Some(active) = self.active_child(parent) {
                alt_next[active] = Some(inactive[0]);
            }
            for (slot, &sibling) in inactive.iter().enumerate() {
                alt_next[sibling] = inactive.get(slot + 1).copied();
                alt_prev[sibling] = if slot == 0 {
                    self.active_child(parent)
                } else {
                    Some(inactive[slot - 1])
                };
            }
        }
        (alt_next, alt_prev)
    }

    /// Computes upstream-compatible header records for every node with an
    /// entry. Link semantics follow Neovim's `u_addbranch`:
    /// `next` is the parent (older) header, `prev` is the active child
    /// (newer) header, and non-active siblings chain through
    /// `alt_next`/`alt_prev` from the active child.
    #[must_use]
    pub fn header_records(&self) -> Vec<HeaderRecord> {
        let mut records = Vec::with_capacity(self.nodes.len().saturating_sub(1));
        let (alt_next_of, alt_prev_of) = self.alt_links();

        for (index, node) in self.nodes.iter().enumerate() {
            let Some(entry) = node.entry.as_ref() else {
                continue;
            };
            let next = node.parent.map_or(0, |parent| self.seq_of(parent));
            let prev = self
                .active_child(index)
                .map_or(0, |child| self.seq_of(child));
            records.push(HeaderRecord {
                seq: entry.seq,
                timestamp: entry.timestamp,
                edits: entry.edits.clone(),
                next,
                prev,
                alt_next: alt_next_of[index].map_or(0, |node| self.seq_of(node)),
                alt_prev: alt_prev_of[index].map_or(0, |node| self.seq_of(node)),
            });
        }
        records
    }

    /// The header list `undotree()` reports, oldest first along the active
    /// branch with inactive branches nested under `alt`.
    ///
    /// This is `u_eval_tree` (`undo.c:3193-3221`): it starts at `b_u_oldhead`,
    /// walks `uh_prev` (the newer header on the branch), and recurses into
    /// `uh_alt_next` for the alternates.
    #[must_use]
    pub fn entries(&self) -> Vec<UndoTreeNode> {
        let (alt_next_of, _) = self.alt_links();
        let newhead = {
            let mut node = self.current;
            while let Some(child) = self.active_child(node) {
                node = child;
            }
            node
        };
        let curhead = self.active_child(self.current);
        self.eval_chain(self.active_child(0), &alt_next_of, newhead, curhead)
    }

    fn eval_chain(
        &self,
        start: Option<usize>,
        alt_next_of: &[Option<usize>],
        newhead: usize,
        curhead: Option<usize>,
    ) -> Vec<UndoTreeNode> {
        let mut out = Vec::new();
        let mut cursor = start;
        while let Some(node) = cursor {
            let Some(entry) = self.nodes[node].entry.as_ref() else {
                break;
            };
            out.push(UndoTreeNode {
                seq: entry.seq,
                timestamp: entry.timestamp,
                newhead: node == newhead,
                curhead: curhead == Some(node),
                alt: self.eval_chain(alt_next_of[node], alt_next_of, newhead, curhead),
            });
            cursor = self.active_child(node);
        }
        out
    }

    /// Computes the top-level header fields for serialization.
    #[must_use]
    pub fn summary(&self) -> UndoSummary {
        let oldhead = self.active_child(0).map_or(0, |child| self.seq_of(child));
        // Walk the active-child chain from the current node to its leaf.
        let mut newhead = self.current;
        while let Some(child) = self.active_child(newhead) {
            newhead = child;
        }
        let curhead = self
            .active_child(self.current)
            .map_or(0, |child| self.seq_of(child));
        UndoSummary {
            oldhead,
            newhead: self.seq_of(newhead),
            curhead,
            seq_last: self.next_seq.saturating_sub(1),
            seq_cur: self.current_seq(),
            time_cur: self.nodes[self.current]
                .entry
                .as_ref()
                .map_or(0, |entry| entry.timestamp),
        }
    }
}
