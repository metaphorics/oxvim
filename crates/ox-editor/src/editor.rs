//! The single-writer root for all editor state.

use std::collections::BTreeMap;

use ox_text::{Buffer, Position};
use ox_types::{BufHandle, TabHandle, WinHandle};
use thiserror::Error;

use crate::autocmd::Autocmds;
use crate::buffer::{BufferState, BufferStateError};
use crate::layout::{Geometry, Layout, LayoutError, TabpageState, WinConfig, WindowState};
use crate::mapping::Mappings;
use crate::marks::{Changelists, GlobalMarks, Jumplist, MarkError};
use crate::options::OptionStore;
use crate::register::{put_content, RegisterError, Registers};
use crate::typeahead::Typeahead;

/// Failures while mutating [`Editor`] state.
#[derive(Debug, Error)]
pub enum EditorError {
    /// A buffer handle is not live.
    #[error("unknown buffer {0:?}")]
    UnknownBuffer(BufHandle),
    /// A window handle is not live.
    #[error("unknown window {0:?}")]
    UnknownWindow(WinHandle),
    /// A tabpage handle is not live.
    #[error("unknown tabpage {0:?}")]
    UnknownTabpage(TabHandle),
    /// A displayed buffer cannot be wiped.
    #[error("cannot wipe buffer {buffer:?} attached to {windows} window(s)")]
    BufferInUse {
        /// Buffer requested for wiping.
        buffer: BufHandle,
        /// Number of windows displaying it.
        windows: usize,
    },
    /// A 32-bit editor handle space was exhausted.
    #[error("{0} handle space exhausted")]
    HandleExhausted(&'static str),
    /// A buffer operation failed.
    #[error(transparent)]
    Buffer(#[from] BufferStateError),
    /// A frame-tree operation failed.
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// A register operation failed.
    #[error(transparent)]
    Register(#[from] RegisterError),
    /// A named-mark operation failed.
    #[error(transparent)]
    Mark(#[from] MarkError),
}

/// What to do with an old buffer after its last window switches away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferRelease {
    /// Retain resident text as a hidden buffer.
    KeepLoaded,
    /// Release resident text and undo history.
    Unload,
}

/// All mutable editor state under a single `&mut self` discipline.
///
/// No state is process-global. Event-loop and RPC layers can serialize their
/// requests into calls on this root without introducing locks into the model.
pub struct Editor {
    /// Live buffers in monotonically allocated handle order.
    buffers: BTreeMap<BufHandle, BufferState>,
    /// Tabpage owning each live window handle.
    windows: BTreeMap<WinHandle, TabHandle>,
    /// Live tabpages and their tiled/floating layouts.
    tabpages: BTreeMap<TabHandle, TabpageState>,
    /// Global and scoped option values.
    options: OptionStore,
    /// Named, numbered, special, and provider-backed registers.
    registers: Registers,
    /// Global `A-Z` and numbered file marks.
    global_marks: GlobalMarks,
    /// Editor jump history.
    jumplist: Jumplist,
    /// Buffer-separated change histories.
    changelists: Changelists,
    /// Registered autocmds and augroups.
    autocmds: Autocmds,
    /// Mode-aware mappings and insert abbreviations.
    mappings: Mappings,
    /// Encoded pending input stack.
    typeahead: Typeahead,
    current_tab: Option<TabHandle>,
    next_buffer: i64,
    next_window: i64,
    next_tabpage: i64,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// Creates an empty editor. The first allocated handle of each kind is one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffers: BTreeMap::new(),
            windows: BTreeMap::new(),
            tabpages: BTreeMap::new(),
            options: OptionStore::new(),
            registers: Registers::new(),
            global_marks: GlobalMarks::new(),
            jumplist: Jumplist::new(),
            changelists: Changelists::new(),
            autocmds: Autocmds::new(),
            mappings: Mappings::new(),
            typeahead: Typeahead::new(),
            current_tab: None,
            next_buffer: 1,
            next_window: 1,
            next_tabpage: 1,
        }
    }

    /// Returns the current tabpage, if one has been created.
    #[must_use]
    pub const fn current_tabpage(&self) -> Option<TabHandle> {
        self.current_tab
    }

    /// Returns an immutable live buffer state.
    pub fn buffer(&self, buffer: BufHandle) -> Result<&BufferState, EditorError> {
        self.buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))
    }

    /// Returns an immutable live tabpage state.
    pub fn tabpage(&self, tab: TabHandle) -> Result<&TabpageState, EditorError> {
        self.tabpages
            .get(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))
    }

    /// Returns immutable viewport state for a live window.
    pub fn window(&self, window: WinHandle) -> Result<&WindowState, EditorError> {
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        Ok(self.tabpage(tab)?.window(window)?)
    }

    /// Changes a live window's cursor position.
    pub fn set_window_cursor(
        &mut self,
        window: WinHandle,
        position: Position,
    ) -> Result<(), EditorError> {
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        tabpage.window_mut(window)?.cursor = position;
        Ok(())
    }

    /// Changes the first displayed line of a live window.
    pub fn set_window_topline(
        &mut self,
        window: WinHandle,
        topline: usize,
    ) -> Result<(), EditorError> {
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        tabpage.window_mut(window)?.topline = topline.max(1);
        Ok(())
    }

    /// Switches the buffer displayed by a window and updates both attachment counts.
    pub fn set_window_buffer(
        &mut self,
        window: WinHandle,
        buffer: BufHandle,
        release: BufferRelease,
    ) -> Result<(), EditorError> {
        self.require_buffer(buffer)?;
        let old_buffer = self.window(window)?.buffer;
        if old_buffer == buffer {
            return Ok(());
        }
        if let Some(state) = self.buffers.get_mut(&buffer) {
            state.attach()?;
        }
        let tab = self
            .windows
            .get(&window)
            .copied()
            .ok_or(EditorError::UnknownWindow(window))?;
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        if let Err(error) = tabpage.window_mut(window).map(|state| state.buffer = buffer) {
            if let Some(state) = self.buffers.get_mut(&buffer) {
                state.detach(true);
            }
            return Err(error.into());
        }
        if let Some(state) = self.buffers.get_mut(&old_buffer) {
            state.detach(release == BufferRelease::KeepLoaded);
        }
        Ok(())
    }

    /// Returns option state.
    #[must_use]
    pub const fn options(&self) -> &OptionStore {
        &self.options
    }

    /// Returns mutable option state.
    pub const fn options_mut(&mut self) -> &mut OptionStore {
        &mut self.options
    }

    /// Returns register state.
    #[must_use]
    pub const fn registers(&self) -> &Registers {
        &self.registers
    }

    /// Returns mutable register state.
    pub const fn registers_mut(&mut self) -> &mut Registers {
        &mut self.registers
    }

    /// Sets a buffer-local named or special mark.
    pub fn set_local_mark(
        &mut self,
        buffer: BufHandle,
        name: char,
        position: Position,
    ) -> Result<Option<Position>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        Ok(state.marks.set(name, position)?)
    }

    /// Reads a buffer-local named or special mark.
    pub fn local_mark(
        &self,
        buffer: BufHandle,
        name: char,
    ) -> Result<Option<Position>, EditorError> {
        Ok(self.buffer(buffer)?.marks.get(name)?)
    }

    /// Returns buffer-separated change history.
    #[must_use]
    pub const fn changelists(&self) -> &Changelists {
        &self.changelists
    }

    /// Returns registered autocmd and augroup state.
    #[must_use]
    pub const fn autocmds(&self) -> &Autocmds {
        &self.autocmds
    }

    /// Returns mutable autocmd and augroup state.
    pub const fn autocmds_mut(&mut self) -> &mut Autocmds {
        &mut self.autocmds
    }

    /// Returns mapping and abbreviation state.
    #[must_use]
    pub const fn mappings(&self) -> &Mappings {
        &self.mappings
    }

    /// Returns mutable mapping and abbreviation state.
    pub const fn mappings_mut(&mut self) -> &mut Mappings {
        &mut self.mappings
    }

    /// Returns queued encoded input.
    #[must_use]
    pub const fn typeahead(&self) -> &Typeahead {
        &self.typeahead
    }

    /// Returns mutable queued encoded input.
    pub const fn typeahead_mut(&mut self) -> &mut Typeahead {
        &mut self.typeahead
    }

    /// Allocates a listed, loaded empty buffer.
    pub fn create_buffer(&mut self, listed: bool) -> Result<BufHandle, EditorError> {
        self.create_buffer_with(Buffer::new(), listed)
    }

    /// Allocates a listed or unlisted buffer around existing text.
    pub fn create_buffer_with(
        &mut self,
        text: Buffer,
        listed: bool,
    ) -> Result<BufHandle, EditorError> {
        let handle = allocate_buffer_handle(&mut self.next_buffer)?;
        self.buffers.insert(handle, BufferState::new(text, listed));
        Ok(handle)
    }

    /// Permanently removes an unattached buffer; its handle is never reused.
    pub fn wipe_buffer(&mut self, buffer: BufHandle) -> Result<BufferState, EditorError> {
        let state = self
            .buffers
            .get(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        if state.attachments != 0 {
            return Err(EditorError::BufferInUse {
                buffer,
                windows: state.attachments,
            });
        }
        self.changelists.remove_buffer(buffer);
        self.options.remove_buffer(buffer);
        self.autocmds.remove_buffer(buffer);
        self.mappings.remove_buffer(buffer);
        self.buffers
            .remove(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))
    }

    /// Releases resident text and undo state for an unattached buffer.
    pub fn unload_buffer(&mut self, buffer: BufHandle) -> Result<(), EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        state.unload()?;
        Ok(())
    }

    /// Creates a tabpage with one tiled window displaying `buffer`.
    pub fn create_tabpage(
        &mut self,
        buffer: BufHandle,
        geometry: Geometry,
    ) -> Result<TabHandle, EditorError> {
        self.require_buffer(buffer)?;
        let window = allocate_window_handle(&mut self.next_window)?;
        let tab = allocate_tab_handle(&mut self.next_tabpage)?;
        let state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        let layout = Layout::new(window, state, geometry)?;
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        self.windows.insert(window, tab);
        self.tabpages.insert(tab, TabpageState::new(layout));
        self.current_tab = Some(tab);
        Ok(tab)
    }

    /// Makes a live tabpage current.
    pub fn set_current_tabpage(&mut self, tab: TabHandle) -> Result<(), EditorError> {
        self.require_tabpage(tab)?;
        self.current_tab = Some(tab);
        Ok(())
    }

    /// Splits a tiled window vertically and displays `buffer` on the right.
    pub fn split_vertical(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Vertical)
    }

    /// Splits a tiled window horizontally and displays `buffer` below.
    pub fn split_horizontal(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
    ) -> Result<WinHandle, EditorError> {
        self.split_window(tab, target, buffer, SplitDirection::Horizontal)
    }

    /// Opens a floating window in `tab`.
    pub fn open_float(
        &mut self,
        tab: TabHandle,
        buffer: BufHandle,
        config: WinConfig,
    ) -> Result<WinHandle, EditorError> {
        self.require_buffer(buffer)?;
        self.require_tabpage(tab)?;
        let window = allocate_window_handle(&mut self.next_window)?;
        let state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        if let Err(error) = tabpage.add_float(window, state, config) {
            if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
                buffer_state.detach(true);
            }
            return Err(error.into());
        }
        self.windows.insert(window, tab);
        Ok(window)
    }

    /// Closes a tiled or floating window, applying the effective hidden policy.
    pub fn close_window(
        &mut self,
        tab: TabHandle,
        window: WinHandle,
        keep_buffer_loaded: bool,
    ) -> Result<WindowState, EditorError> {
        self.require_tabpage(tab)?;
        if self.windows.get(&window) != Some(&tab) {
            return Err(EditorError::UnknownWindow(window));
        }
        let canonical = self
            .tabpages
            .get(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?
            .window(window)?
            .clone();
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        let removed = if tabpage
            .floating_windows()
            .any(|candidate| candidate.window == window)
        {
            tabpage.remove_float(window)?.state
        } else {
            tabpage.close_tiled(window)?
        };
        self.windows.remove(&window);
        self.options.remove_window(window);
        if let Some(buffer_state) = self.buffers.get_mut(&canonical.buffer) {
            buffer_state.detach(keep_buffer_loaded);
        }
        Ok(removed)
    }

    /// Replaces buffer lines and adjusts every position-bearing subsystem.
    pub fn replace_buffer_lines(
        &mut self,
        buffer: BufHandle,
        start: usize,
        end: usize,
        lines: &[Vec<u8>],
        cursor_before: Position,
        cursor_after: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        let old_count = end.saturating_sub(start).saturating_add(1);
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let seq = state.replace_lines(
            start,
            end,
            lines,
            cursor_before,
            cursor_after,
            timestamp,
        )?;
        self.splice_positions(buffer, start, old_count, lines.len());
        self.changelists.push(buffer, cursor_after);
        Ok(seq)
    }

    /// Appends buffer lines and adjusts every position-bearing subsystem.
    pub fn append_buffer_lines(
        &mut self,
        buffer: BufHandle,
        after: usize,
        lines: &[Vec<u8>],
        cursor: Position,
        timestamp: i64,
    ) -> Result<u64, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let seq = state.append_lines(after, lines, cursor, timestamp)?;
        let start = after.saturating_add(1);
        self.splice_positions(buffer, start, 0, lines.len());
        self.changelists.push(
            buffer,
            Position {
                lnum: cursor.lnum.saturating_add(lines.len()),
                col: cursor.col,
            },
        );
        Ok(seq)
    }

    /// Undoes a buffer's most recent change, replaying its inverse through
    /// every position-bearing subsystem (marks, jump/change history, window
    /// cursors), matching the direct-mutation pipeline. Returns the undone
    /// header's sequence, or `None` at the oldest change.
    pub fn buffer_undo(&mut self, buffer: BufHandle) -> Result<Option<u64>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let Some(replayed) = state.undo()? else {
            return Ok(None);
        };
        self.finish_replay(buffer, replayed);
        Ok(Some(replayed.seq))
    }

    /// Redoes a buffer's next change, replaying its stored edit through every
    /// position-bearing subsystem. Returns the redone header's sequence, or
    /// `None` at the newest change.
    pub fn buffer_redo(&mut self, buffer: BufHandle) -> Result<Option<u64>, EditorError> {
        let state = self
            .buffers
            .get_mut(&buffer)
            .ok_or(EditorError::UnknownBuffer(buffer))?;
        let Some(replayed) = state.redo()? else {
            return Ok(None);
        };
        self.finish_replay(buffer, replayed);
        Ok(Some(replayed.seq))
    }

    fn finish_replay(&mut self, buffer: BufHandle, replayed: crate::buffer::ReplayedEdit) {
        self.splice_positions(buffer, replayed.start, replayed.old_count, replayed.new_count);
        self.changelists.push(buffer, replayed.cursor);
    }

    /// Puts a stored register through the buffer mutation pipeline.
    ///
    /// Returns false when the selected register has no retained content.
    pub fn put_register(
        &mut self,
        buffer: BufHandle,
        position: Position,
        name: char,
        timestamp: i64,
    ) -> Result<bool, EditorError> {
        let Some(content) = self.registers.get(name)?.cloned() else {
            return Ok(false);
        };
        let original = self.buffer(buffer)?.text()?.clone();
        let before = buffer_lines(&original)?;
        let mut resulting = original;
        put_content(&mut resulting, position, &content)?;
        let after = buffer_lines(&resulting)?;
        if before == after {
            return Ok(true);
        }

        let prefix = before
            .iter()
            .zip(&after)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = before[prefix..]
            .iter()
            .rev()
            .zip(after[prefix..].iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        let old_count = before.len().saturating_sub(prefix + suffix);
        let new_end = after.len().saturating_sub(suffix);
        let replacement = &after[prefix..new_end];
        if old_count == 0 {
            self.append_buffer_lines(buffer, prefix, replacement, position, timestamp)?;
        } else {
            self.replace_buffer_lines(
                buffer,
                prefix + 1,
                prefix + old_count,
                replacement,
                position,
                position,
                timestamp,
            )?;
        }
        Ok(true)
    }

    fn split_window(
        &mut self,
        tab: TabHandle,
        target: WinHandle,
        buffer: BufHandle,
        direction: SplitDirection,
    ) -> Result<WinHandle, EditorError> {
        self.require_buffer(buffer)?;
        self.require_tabpage(tab)?;
        let window = allocate_window_handle(&mut self.next_window)?;
        let state = WindowState::new(buffer, Position { lnum: 1, col: 0 });
        if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
            buffer_state.attach()?;
        }
        let tabpage = self
            .tabpages
            .get_mut(&tab)
            .ok_or(EditorError::UnknownTabpage(tab))?;
        let inserted = match direction {
            SplitDirection::Vertical => tabpage.split_vertical(target, window, state),
            SplitDirection::Horizontal => tabpage.split_horizontal(target, window, state),
        };
        if let Err(error) = inserted {
            if let Some(buffer_state) = self.buffers.get_mut(&buffer) {
                buffer_state.detach(true);
            }
            return Err(error.into());
        }
        self.windows.insert(window, tab);
        Ok(window)
    }

    fn splice_positions(
        &mut self,
        buffer: BufHandle,
        start: usize,
        old_count: usize,
        new_count: usize,
    ) {
        self.global_marks
            .splice_buffer(buffer, start, old_count, new_count);
        self.jumplist
            .splice_buffer(buffer, start, old_count, new_count);
        self.changelists
            .splice_buffer(buffer, start, old_count, new_count);
        let windows = &self.windows;
        let tabpages = &mut self.tabpages;
        for (&window, &tab) in windows {
            let Some(tabpage) = tabpages.get_mut(&tab) else {
                continue;
            };
            let Ok(state) = tabpage.window_mut(window) else {
                continue;
            };
            if state.buffer != buffer {
                continue;
            }
            splice_position(&mut state.cursor, start, old_count, new_count);
            let mut top = Position {
                lnum: state.topline,
                col: 0,
            };
            splice_position(&mut top, start, old_count, new_count);
            state.topline = top.lnum;
        }
    }

    fn require_buffer(&self, buffer: BufHandle) -> Result<(), EditorError> {
        if self.buffers.contains_key(&buffer) {
            Ok(())
        } else {
            Err(EditorError::UnknownBuffer(buffer))
        }
    }

    fn require_tabpage(&self, tab: TabHandle) -> Result<(), EditorError> {
        if self.tabpages.contains_key(&tab) {
            Ok(())
        } else {
            Err(EditorError::UnknownTabpage(tab))
        }
    }
}

fn splice_position(position: &mut Position, start: usize, old_count: usize, new_count: usize) {
    let old_end = start.saturating_add(old_count);
    if position.lnum < start {
        return;
    }
    if position.lnum >= old_end {
        position.lnum = if new_count >= old_count {
            position.lnum.saturating_add(new_count - old_count)
        } else {
            position.lnum.saturating_sub(old_count - new_count).max(1)
        };
        return;
    }
    let relative = position.lnum - start;
    if relative < new_count {
        position.lnum = start + relative;
    } else {
        position.lnum = start.max(1);
        position.col = 0;
    }
}

fn buffer_lines(buffer: &Buffer) -> Result<Vec<Vec<u8>>, BufferStateError> {
    (1..=buffer.line_count())
        .map(|line| buffer.line(line))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BufferStateError::from)
}

#[derive(Clone, Copy)]
enum SplitDirection {
    Vertical,
    Horizontal,
}

fn allocate_buffer_handle(next: &mut i64) -> Result<BufHandle, EditorError> {
    let value = *next;
    let handle = BufHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("buffer"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("buffer"))?;
    Ok(handle)
}

fn allocate_window_handle(next: &mut i64) -> Result<WinHandle, EditorError> {
    let value = *next;
    let handle = WinHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("window"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("window"))?;
    Ok(handle)
}

fn allocate_tab_handle(next: &mut i64) -> Result<TabHandle, EditorError> {
    let value = *next;
    let handle = TabHandle::try_from(value).map_err(|_| EditorError::HandleExhausted("tabpage"))?;
    *next = next
        .checked_add(1)
        .ok_or(EditorError::HandleExhausted("tabpage"))?;
    Ok(handle)
}
