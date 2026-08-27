//! Named marks, jump history, and per-buffer change history.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ox_text::{Marks, Position};
use ox_types::BufHandle;
use thiserror::Error;

/// Maximum number of entries retained by jump and change histories.
pub const HISTORY_CAPACITY: usize = 100;

const FIRST_LOCAL_MARK: char = 'a';
const LAST_LOCAL_MARK: char = 'z';
const FIRST_GLOBAL_MARK: char = 'A';
const LAST_GLOBAL_MARK: char = 'Z';
const FIRST_NUMBERED_MARK: char = '0';
const LAST_NUMBERED_MARK: char = '9';
const SPECIAL_LOCAL_MARKS: [char; 5] = ['\'', '`', '.', '^', ':'];

/// An invalid named-mark operation.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum MarkError {
    /// The name is not one of `a-z`, `'`, `` ` ``, `.`, `^`, or `:`.
    #[error("invalid local mark name '{0}'")]
    InvalidLocal(char),
    /// The name is not in `A-Z` or `0-9`.
    #[error("invalid global mark name '{0}'")]
    InvalidGlobal(char),
}

/// The buffer or file containing a global mark or jump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkTarget {
    /// A loaded or otherwise known editor buffer.
    Buffer(BufHandle),
    /// A file that need not currently have a buffer.
    File(PathBuf),
}

/// A position together with the buffer or file that owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkLocation {
    /// The buffer or file containing the position.
    pub target: MarkTarget,
    /// The position within the target.
    pub position: Position,
}

impl MarkLocation {
    /// Creates a location in a buffer.
    #[must_use]
    pub const fn in_buffer(buffer: BufHandle, position: Position) -> Self {
        Self {
            target: MarkTarget::Buffer(buffer),
            position,
        }
    }

    /// Creates a location in a file.
    #[must_use]
    pub fn in_file(file: impl Into<PathBuf>, position: Position) -> Self {
        Self {
            target: MarkTarget::File(file.into()),
            position,
        }
    }

    /// Returns the buffer when this location names one.
    #[must_use]
    pub const fn buffer(&self) -> Option<BufHandle> {
        match &self.target {
            MarkTarget::Buffer(buffer) => Some(*buffer),
            MarkTarget::File(_) => None,
        }
    }

    /// Returns the file when this location names one.
    #[must_use]
    pub fn file(&self) -> Option<&Path> {
        match &self.target {
            MarkTarget::Buffer(_) => None,
            MarkTarget::File(file) => Some(file.as_path()),
        }
    }
}

/// Buffer-local named marks.
///
/// Lowercase marks and the editor-maintained special marks use stable
/// identifiers in [`Marks`], so line splices inherit `ox-text`'s boundary and
/// clamping semantics.
#[derive(Clone, Debug, Default)]
pub struct LocalMarks {
    marks: Marks,
}

impl LocalMarks {
    /// Creates an empty local mark set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marks: Marks::new(),
        }
    }

    /// Inserts or replaces a local mark.
    pub fn set(
        &mut self,
        name: char,
        position: Position,
    ) -> Result<Option<Position>, MarkError> {
        let id = local_mark_id(name)?;
        Ok(self.marks.set(id, position))
    }

    /// Gets a local mark.
    pub fn get(&self, name: char) -> Result<Option<Position>, MarkError> {
        let id = local_mark_id(name)?;
        Ok(self.marks.get(id))
    }

    /// Removes a local mark.
    pub fn remove(&mut self, name: char) -> Result<Option<Position>, MarkError> {
        let id = local_mark_id(name)?;
        Ok(self.marks.remove(id))
    }

    /// Iterates over set marks in `a-z`, `'`, `` ` ``, `.`, `^`, `:` order.
    pub fn iter(&self) -> impl Iterator<Item = (char, Position)> + '_ {
        self.marks
            .iter()
            .filter_map(|(id, position)| local_mark_name(id).map(|name| (name, position)))
    }

    /// Adjusts all local marks for a logical-line splice.
    pub fn splice(&mut self, start: usize, old_count: usize, new_count: usize) {
        self.marks.splice(start, old_count, new_count);
    }
}

/// Global `A-Z` and numbered `0-9` marks.
#[derive(Clone, Debug, Default)]
pub struct GlobalMarks {
    marks: BTreeMap<char, MarkLocation>,
}

impl GlobalMarks {
    /// Creates an empty global mark set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marks: BTreeMap::new(),
        }
    }

    /// Inserts or replaces a global or numbered mark.
    pub fn set(
        &mut self,
        name: char,
        location: MarkLocation,
    ) -> Result<Option<MarkLocation>, MarkError> {
        validate_global_mark(name)?;
        Ok(self.marks.insert(name, location))
    }

    /// Gets a global or numbered mark.
    pub fn get(&self, name: char) -> Result<Option<&MarkLocation>, MarkError> {
        validate_global_mark(name)?;
        Ok(self.marks.get(&name))
    }

    /// Removes a global or numbered mark.
    pub fn remove(&mut self, name: char) -> Result<Option<MarkLocation>, MarkError> {
        validate_global_mark(name)?;
        Ok(self.marks.remove(&name))
    }

    /// Iterates in deterministic digit-then-uppercase character order.
    pub fn iter(&self) -> impl Iterator<Item = (char, &MarkLocation)> {
        self.marks.iter().map(|(&name, location)| (name, location))
    }

    /// Adjusts marks that refer to `buffer` for a logical-line splice.
    pub fn splice_buffer(
        &mut self,
        buffer: BufHandle,
        start: usize,
        old_count: usize,
        new_count: usize,
    ) {
        let mut positions = Marks::new();
        let mut names = Vec::new();

        for (&name, location) in &self.marks {
            if location.buffer() == Some(buffer) {
                let id = names.len() as u64;
                positions.set(id, location.position);
                names.push(name);
            }
        }
        positions.splice(start, old_count, new_count);

        for (id, name) in names.into_iter().enumerate() {
            if let (Some(position), Some(location)) =
                (positions.get(id as u64), self.marks.get_mut(&name))
            {
                location.position = position;
            }
        }
    }
}

/// A bounded, cursor-addressed history of jump locations.
///
/// The index is one past the newest entry after construction or [`push`](Self::push),
/// matching Neovim's `w_jumplistidx == w_jumplistlen` resting state.
#[derive(Clone, Debug, Default)]
pub struct Jumplist {
    entries: Vec<MarkLocation>,
    index: usize,
}

impl Jumplist {
    /// Creates an empty jumplist.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: 0,
        }
    }

    /// Returns the number of retained entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the jumplist is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the current jump index; `len()` denotes the resting state.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Returns all entries from oldest to newest.
    #[must_use]
    pub fn entries(&self) -> &[MarkLocation] {
        &self.entries
    }

    /// Adds a jump, discarding history newer than the current index.
    ///
    /// When full, the oldest entry is removed and the newest 100 are retained.
    pub fn push(&mut self, location: MarkLocation) {
        if self.index < self.entries.len() {
            self.entries.truncate(self.index.saturating_add(1));
        }
        if self.entries.len() == HISTORY_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(location);
        self.index = self.entries.len();
    }

    /// Moves one entry toward older jumps.
    pub fn backward(&mut self) -> Option<&MarkLocation> {
        if self.index == 0 {
            return None;
        }
        self.index -= 1;
        self.entries.get(self.index)
    }

    /// Moves one entry toward newer jumps.
    pub fn forward(&mut self) -> Option<&MarkLocation> {
        if self.entries.is_empty() || self.index >= self.entries.len().saturating_sub(1) {
            return None;
        }
        self.index += 1;
        self.entries.get(self.index)
    }

    /// Adjusts jumps that refer to `buffer` for a logical-line splice.
    pub fn splice_buffer(
        &mut self,
        buffer: BufHandle,
        start: usize,
        old_count: usize,
        new_count: usize,
    ) {
        splice_locations(&mut self.entries, buffer, start, old_count, new_count);
    }
}

#[derive(Clone, Debug, Default)]
struct ChangeHistory {
    entries: Vec<Position>,
    index: usize,
}

/// Bounded change histories, independently navigated per buffer.
#[derive(Clone, Debug, Default)]
pub struct Changelists {
    buffers: BTreeMap<BufHandle, ChangeHistory>,
}

impl Changelists {
    /// Creates an empty collection of changelists.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffers: BTreeMap::new(),
        }
    }

    /// Returns the number of retained changes for `buffer`.
    #[must_use]
    pub fn len(&self, buffer: BufHandle) -> usize {
        self.buffers
            .get(&buffer)
            .map_or(0, |history| history.entries.len())
    }

    /// Returns whether `buffer` has no retained changes.
    #[must_use]
    pub fn is_empty(&self, buffer: BufHandle) -> bool {
        self.len(buffer) == 0
    }

    /// Returns the current index for an existing buffer history.
    #[must_use]
    pub fn index(&self, buffer: BufHandle) -> Option<usize> {
        self.buffers.get(&buffer).map(|history| history.index)
    }

    /// Returns retained positions from oldest to newest.
    #[must_use]
    pub fn entries(&self, buffer: BufHandle) -> Option<&[Position]> {
        self.buffers
            .get(&buffer)
            .map(|history| history.entries.as_slice())
    }

    /// Appends a change and leaves the index one past the newest entry.
    ///
    /// Navigation does not branch change history: a later change always appends
    /// chronologically. At capacity, the oldest entry is discarded.
    pub fn push(&mut self, buffer: BufHandle, position: Position) {
        let history = self.buffers.entry(buffer).or_default();
        if history.entries.len() == HISTORY_CAPACITY {
            history.entries.remove(0);
        }
        history.entries.push(position);
        history.index = history.entries.len();
    }

    /// Moves one entry toward older changes in `buffer`.
    pub fn backward(&mut self, buffer: BufHandle) -> Option<Position> {
        let history = self.buffers.get_mut(&buffer)?;
        if history.index == 0 {
            return None;
        }
        history.index -= 1;
        history.entries.get(history.index).copied()
    }

    /// Moves one entry toward newer changes in `buffer`.
    pub fn forward(&mut self, buffer: BufHandle) -> Option<Position> {
        let history = self.buffers.get_mut(&buffer)?;
        if history.entries.is_empty()
            || history.index >= history.entries.len().saturating_sub(1)
        {
            return None;
        }
        history.index += 1;
        history.entries.get(history.index).copied()
    }

    /// Adjusts every retained change in `buffer` for a logical-line splice.
    pub fn splice_buffer(
        &mut self,
        buffer: BufHandle,
        start: usize,
        old_count: usize,
        new_count: usize,
    ) {
        let Some(history) = self.buffers.get_mut(&buffer) else {
            return;
        };
        splice_positions(&mut history.entries, start, old_count, new_count);
    }

    /// Removes all history and navigation state for `buffer`.
    pub fn remove_buffer(&mut self, buffer: BufHandle) -> bool {
        self.buffers.remove(&buffer).is_some()
    }
}

fn local_mark_id(name: char) -> Result<u64, MarkError> {
    if (FIRST_LOCAL_MARK..=LAST_LOCAL_MARK).contains(&name) {
        return Ok(u64::from(name as u32 - FIRST_LOCAL_MARK as u32));
    }
    SPECIAL_LOCAL_MARKS
        .iter()
        .position(|&special| special == name)
        .map(|index| 26 + index as u64)
        .ok_or(MarkError::InvalidLocal(name))
}

fn local_mark_name(id: u64) -> Option<char> {
    if id < 26 {
        return char::from_u32(FIRST_LOCAL_MARK as u32 + id as u32);
    }
    let special_index = usize::try_from(id.checked_sub(26)?).ok()?;
    SPECIAL_LOCAL_MARKS.get(special_index).copied()
}

fn validate_global_mark(name: char) -> Result<(), MarkError> {
    if (FIRST_GLOBAL_MARK..=LAST_GLOBAL_MARK).contains(&name)
        || (FIRST_NUMBERED_MARK..=LAST_NUMBERED_MARK).contains(&name)
    {
        Ok(())
    } else {
        Err(MarkError::InvalidGlobal(name))
    }
}

fn splice_locations(
    locations: &mut [MarkLocation],
    buffer: BufHandle,
    start: usize,
    old_count: usize,
    new_count: usize,
) {
    let mut positions = Marks::new();
    for (index, location) in locations.iter().enumerate() {
        if location.buffer() == Some(buffer) {
            positions.set(index as u64, location.position);
        }
    }
    positions.splice(start, old_count, new_count);
    for (index, location) in locations.iter_mut().enumerate() {
        if let Some(position) = positions.get(index as u64) {
            location.position = position;
        }
    }
}

fn splice_positions(
    positions: &mut [Position],
    start: usize,
    old_count: usize,
    new_count: usize,
) {
    let mut marks = Marks::new();
    for (index, &position) in positions.iter().enumerate() {
        marks.set(index as u64, position);
    }
    marks.splice(start, old_count, new_count);
    for (index, position) in positions.iter_mut().enumerate() {
        if let Some(adjusted) = marks.get(index as u64) {
            *position = adjusted;
        }
    }
}
