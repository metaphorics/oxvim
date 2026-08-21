//! Headless storage and composition for Neovim redraw grids.
//!
//! This module deliberately owns only protocol state. It does not perform RPC
//! I/O and it does not write to a terminal.
#![allow(missing_docs)]

use std::collections::HashMap;

use ox_rpc::RedrawEvent;
use ox_types::{Dict, Object, OxStr};
use thiserror::Error;

/// A grid identifier from the UI protocol.
pub type GridId = i64;
/// A window handle represented as its stable wire integer.
pub type WindowId = i64;
/// A highlight identifier from `hl_attr_define`.
pub type HighlightId = i64;

/// One terminal cell. `text` is retained byte-for-byte as received from Nvim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    pub text: OxStr,
    pub highlight_id: HighlightId,
    pub blend_underlay: Option<HighlightId>,
    pub blend_percentage: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: OxStr::from(" "),
            highlight_id: 0,
            blend_underlay: None,
            blend_percentage: 0,
        }
    }
}

/// A rectangular protocol grid in row-major order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grid {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    wrapped_rows: Vec<bool>,
}

impl Grid {
    fn new(width: usize, height: usize) -> Result<Self, ScreenError> {
        let len = width
            .checked_mul(height)
            .ok_or(ScreenError::GridSizeOverflow { width, height })?;
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(len)
            .map_err(|_| ScreenError::GridAllocationFailed { width, height })?;
        cells.resize(len, Cell::default());
        let mut wrapped_rows = Vec::new();
        wrapped_rows
            .try_reserve_exact(height)
            .map_err(|_| ScreenError::GridAllocationFailed { width, height })?;
        wrapped_rows.resize(height, false);
        Ok(Self {
            width,
            height,
            cells,
            wrapped_rows,
        })
    }

    /// Grid width in terminal cells.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Grid height in terminal cells.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// All cells in row-major order.
    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// A cell at `(row, column)`, if it lies inside the grid.
    #[must_use]
    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        self.index(row, column).and_then(|index| self.cells.get(index))
    }

    /// Whether a row was marked as wrapping into the following row.
    #[must_use]
    pub fn row_wraps(&self, row: usize) -> Option<bool> {
        self.wrapped_rows.get(row).copied()
    }

    fn index(&self, row: usize, column: usize) -> Option<usize> {
        if row >= self.height || column >= self.width {
            return None;
        }
        row.checked_mul(self.width)?.checked_add(column)
    }

    fn resize(&mut self, width: usize, height: usize) -> Result<(), ScreenError> {
        let mut replacement = Self::new(width, height)?;
        let copy_height = self.height.min(height);
        let copy_width = self.width.min(width);
        for row in 0..copy_height {
            for column in 0..copy_width {
                let Some(source) = self.index(row, column) else {
                    continue;
                };
                let Some(destination) = replacement.index(row, column) else {
                    continue;
                };
                if let (Some(source_cell), Some(destination_cell)) =
                    (self.cells.get(source), replacement.cells.get_mut(destination))
                {
                    destination_cell.clone_from(source_cell);
                }
            }
            if let (Some(source_wrap), Some(destination_wrap)) = (
                self.wrapped_rows.get(row),
                replacement.wrapped_rows.get_mut(row),
            ) {
                *destination_wrap = *source_wrap;
            }
        }
        *self = replacement;
        Ok(())
    }

    fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.wrapped_rows.fill(false);
    }
}

/// The fully composed terminal-sized grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposedGrid {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
}

impl ComposedGrid {
    /// Composed width in terminal cells.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Composed height in terminal cells.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// All composed cells in row-major order.
    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// A composed cell at `(row, column)`.
    #[must_use]
    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        if row >= self.height || column >= self.width {
            return None;
        }
        row.checked_mul(self.width)
            .and_then(|base| base.checked_add(column))
            .and_then(|index| self.cells.get(index))
    }

    /// Concatenate the byte-exact cell contents, inserting `\n` between rows.
    #[must_use]
    pub fn render_to_bytes(&self) -> Vec<u8> {
        let line_breaks = self.height.saturating_sub(1);
        let capacity = self.cells.len().saturating_add(line_breaks);
        let mut output = Vec::with_capacity(capacity);
        for row in 0..self.height {
            for column in 0..self.width {
                if let Some(cell) = self.cell(row, column) {
                    output.extend_from_slice(cell.text.as_bytes());
                }
            }
            if row + 1 < self.height {
                output.push(b'\n');
            }
        }
        output
    }

    /// Render the composed grid for headless tests and diagnostics.
    ///
    /// UI grid text is specified by the protocol as UTF-8. If a nonconforming
    /// peer sends invalid bytes, the owned [`Cell`] and [`Self::render_to_bytes`]
    /// still retain them exactly; only this string-oriented diagnostic view is
    /// lossily decoded.
    #[must_use]
    pub fn render_to_string(&self) -> String {
        String::from_utf8_lossy(&self.render_to_bytes()).into_owned()
    }
}

/// Cursor position within a protocol grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub grid: GridId,
    pub row: usize,
    pub column: usize,
}

/// Cursor geometry selected by `mode_info_set` and `mode_change`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CursorShape {
    Block,
    Horizontal,
    Vertical,
    /// A future server cursor shape retained rather than guessed.
    Unknown(OxStr),
}

/// Cursor properties for one server mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModeInfo {
    pub cursor_shape: Option<CursorShape>,
    pub cell_percentage: Option<u8>,
    pub blink_wait: Option<u64>,
    pub blink_on: Option<u64>,
    pub blink_off: Option<u64>,
    pub attr_id: Option<HighlightId>,
    pub attr_id_lmap: Option<HighlightId>,
    pub short_name: Option<OxStr>,
    pub name: Option<OxStr>,
}

/// The active mode and its resolved cursor properties.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveMode {
    pub name: OxStr,
    pub index: usize,
    pub info: ModeInfo,
}

/// Raw highlight definitions needed by a terminal renderer or theme mapper.
#[derive(Clone, Debug, PartialEq)]
pub struct HighlightDefinition {
    pub rgb: Dict,
    pub cterm: Dict,
    pub info: Vec<Object>,
}

/// A window's content viewport in buffer coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    pub window: WindowId,
    pub top_line: i64,
    pub bottom_line: i64,
    pub cursor_line: i64,
    pub cursor_column: i64,
    pub line_count: i64,
    pub scroll_delta: i64,
}

/// Grid cells outside the window's buffer viewport.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ViewportMargins {
    pub window: WindowId,
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
}

/// How a multigrid window participates in terminal composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowKind {
    Normal,
    Floating,
    External,
}

/// Placement and protocol metadata for a window grid.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowPlacement {
    pub grid: GridId,
    pub window: WindowId,
    pub kind: WindowKind,
    pub row: i64,
    pub column: i64,
    pub width: usize,
    pub height: usize,
    pub hidden: bool,
    pub mouse_enabled: bool,
    pub z_index: i64,
    pub composition_index: i64,
    pub anchor: Option<OxStr>,
    pub anchor_grid: Option<GridId>,
    pub anchor_row: Option<f64>,
    pub anchor_column: Option<f64>,
    sequence: u64,
}

/// Result of routing one redraw event through the screen store.
#[derive(Clone, Debug, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    /// The event belongs to another client-owned surface or a newer protocol.
    Unknown(RedrawEvent),
}

/// A malformed or inconsistent redraw event.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ScreenError {
    #[error("redraw event entry must be an array")]
    EventEntryNotArray,
    #[error("redraw event name must be a byte string")]
    EventNameNotString,
    #[error("redraw event {event:?} argument set {argset} must be an array")]
    EventArgumentsNotArray { event: OxStr, argset: usize },
    #[error("{event} expects {expected} arguments, received {actual}")]
    WrongArity {
        event: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("{event} argument {index} ({field}) must be {expected}")]
    WrongType {
        event: &'static str,
        index: usize,
        field: &'static str,
        expected: &'static str,
    },
    #[error("{event} argument {index} ({field}) is outside the supported range: {value}")]
    OutOfRange {
        event: &'static str,
        index: usize,
        field: &'static str,
        value: i64,
    },
    #[error("grid dimensions {width}x{height} overflow addressable storage")]
    GridSizeOverflow { width: usize, height: usize },
    #[error("grid dimensions {width}x{height} cannot be allocated")]
    GridAllocationFailed { width: usize, height: usize },
    #[error("{event} references unknown grid {grid}")]
    UnknownGrid { event: &'static str, grid: GridId },
    #[error("{event} position ({row}, {column}) is outside grid {grid}")]
    GridPositionOutOfBounds {
        event: &'static str,
        grid: GridId,
        row: usize,
        column: usize,
    },
    #[error("grid_line cell tuple {tuple} must contain one to three items")]
    InvalidCellTupleArity { tuple: usize },
    #[error("grid_line cell tuple {tuple} has invalid repeat count {repeat}")]
    InvalidCellRepeat { tuple: usize, repeat: i64 },
    #[error("grid_scroll region [{top}, {bottom}) x [{left}, {right}) is outside grid {grid}")]
    InvalidScrollRegion {
        grid: GridId,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
    },
    #[error("mode_change index {index} has no mode_info_set entry")]
    UnknownModeIndex { index: usize },
    #[error("mode_info_set mode {mode} field {field:?} must be {expected}")]
    InvalidModeField {
        mode: usize,
        field: OxStr,
        expected: &'static str,
    },
}

/// Stateful receiver for grid-related redraw events.
#[derive(Clone, Debug)]
pub struct Screen {
    terminal_grid: GridId,
    grids: HashMap<GridId, Grid>,
    windows: HashMap<GridId, WindowPlacement>,
    viewports: HashMap<GridId, Viewport>,
    margins: HashMap<GridId, ViewportMargins>,
    highlights: HashMap<HighlightId, HighlightDefinition>,
    cursor: Option<Cursor>,
    cursor_style_enabled: bool,
    modes: Vec<ModeInfo>,
    active_mode: Option<ActiveMode>,
    next_sequence: u64,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    /// Create a screen whose terminal grid is Neovim's default grid `1`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_terminal_grid(1)
    }

    /// Create a screen with an explicit terminal/root grid identifier.
    #[must_use]
    pub fn with_terminal_grid(terminal_grid: GridId) -> Self {
        Self {
            terminal_grid,
            grids: HashMap::new(),
            windows: HashMap::new(),
            viewports: HashMap::new(),
            margins: HashMap::new(),
            highlights: HashMap::new(),
            cursor: None,
            cursor_style_enabled: false,
            modes: Vec::new(),
            active_mode: None,
            next_sequence: 0,
        }
    }

    /// Look up an uncomposed protocol grid.
    #[must_use]
    pub fn grid(&self, grid: GridId) -> Option<&Grid> {
        self.grids.get(&grid)
    }

    /// Iterate over all uncomposed protocol grids.
    pub fn grids(&self) -> impl Iterator<Item = (GridId, &Grid)> {
        self.grids.iter().map(|(id, grid)| (*id, grid))
    }

    /// Look up window placement by grid id.
    #[must_use]
    pub fn window(&self, grid: GridId) -> Option<&WindowPlacement> {
        self.windows.get(&grid)
    }

    /// Look up the latest viewport for a window grid.
    #[must_use]
    pub fn viewport(&self, grid: GridId) -> Option<&Viewport> {
        self.viewports.get(&grid)
    }

    /// Look up the latest viewport margins for a window grid.
    #[must_use]
    pub fn viewport_margins(&self, grid: GridId) -> Option<ViewportMargins> {
        self.margins.get(&grid).copied()
    }

    /// Look up a highlight definition by id.
    #[must_use]
    pub fn highlight(&self, id: HighlightId) -> Option<&HighlightDefinition> {
        self.highlights.get(&id)
    }

    /// Current cursor position before multigrid projection.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    /// Whether the server requested mode-specific cursor styling.
    #[must_use]
    pub const fn cursor_style_enabled(&self) -> bool {
        self.cursor_style_enabled
    }

    /// Mode definitions from the latest `mode_info_set`.
    #[must_use]
    pub fn modes(&self) -> &[ModeInfo] {
        &self.modes
    }

    /// Active mode and its resolved cursor information.
    #[must_use]
    pub fn active_mode(&self) -> Option<&ActiveMode> {
        self.active_mode.as_ref()
    }

    /// Apply one already-decoded RPC redraw event.
    pub fn apply_event(&mut self, event: &RedrawEvent) -> Result<ApplyOutcome, ScreenError> {
        let known = is_screen_event(&event.name);
        if !known {
            return Ok(ApplyOutcome::Unknown(event.clone()));
        }
        for arguments in &event.argsets {
            self.apply_known(&event.name, arguments)?;
        }
        Ok(ApplyOutcome::Applied)
    }

    /// Parse and apply one wire event entry: `[name, argset, argset, ...]`.
    pub fn apply_redraw_object(&mut self, entry: &Object) -> Result<ApplyOutcome, ScreenError> {
        let Object::Array(parts) = entry else {
            return Err(ScreenError::EventEntryNotArray);
        };
        let Some(Object::String(name)) = parts.first() else {
            return Err(ScreenError::EventNameNotString);
        };
        let mut argsets = Vec::with_capacity(parts.len().saturating_sub(1));
        for (argset, part) in parts.iter().enumerate().skip(1) {
            let Object::Array(arguments) = part else {
                return Err(ScreenError::EventArgumentsNotArray {
                    event: name.clone(),
                    argset: argset - 1,
                });
            };
            argsets.push(arguments.clone());
        }
        self.apply_event(&RedrawEvent {
            name: name.clone(),
            argsets,
        })
    }

    /// Parse and apply every event entry in a redraw batch array.
    pub fn apply_redraw_batch(
        &mut self,
        batch: &Object,
    ) -> Result<Vec<ApplyOutcome>, ScreenError> {
        let Object::Array(entries) = batch else {
            return Err(ScreenError::EventEntryNotArray);
        };
        entries
            .iter()
            .map(|entry| self.apply_redraw_object(entry))
            .collect()
    }

    /// Compose all visible internal windows over the terminal grid.
    pub fn composed_grid(&self) -> Result<ComposedGrid, ScreenError> {
        let root = self
            .grids
            .get(&self.terminal_grid)
            .ok_or(ScreenError::UnknownGrid {
                event: "compose",
                grid: self.terminal_grid,
            })?;
        let mut composed = ComposedGrid {
            width: root.width,
            height: root.height,
            cells: root.cells.clone(),
        };

        let mut normal: Vec<&WindowPlacement> = self
            .windows
            .values()
            .filter(|window| {
                !window.hidden
                    && window.kind == WindowKind::Normal
                    && window.grid != self.terminal_grid
            })
            .collect();
        normal.sort_by_key(|window| window.sequence);
        for window in normal {
            self.composite_window(&mut composed, window);
        }

        let mut floating: Vec<&WindowPlacement> = self
            .windows
            .values()
            .filter(|window| !window.hidden && window.kind == WindowKind::Floating)
            .collect();
        floating.sort_by_key(|window| (window.composition_index, window.sequence));
        for window in floating {
            self.composite_window(&mut composed, window);
        }
        Ok(composed)
    }

    /// Project the cursor into terminal coordinates, returning `None` for a
    /// hidden, external, or fully clipped window.
    #[must_use]
    pub fn composed_cursor(&self) -> Option<(usize, usize)> {
        let cursor = self.cursor?;
        if cursor.grid == self.terminal_grid {
            let root = self.grids.get(&self.terminal_grid)?;
            return root.cell(cursor.row, cursor.column).map(|_| (cursor.row, cursor.column));
        }
        let window = self.windows.get(&cursor.grid)?;
        if window.hidden
            || window.kind == WindowKind::External
            || cursor.row >= window.height
            || cursor.column >= window.width
        {
            return None;
        }
        let row = add_coordinate(window.row, cursor.row)?;
        let column = add_coordinate(window.column, cursor.column)?;
        let root = self.grids.get(&self.terminal_grid)?;
        root.cell(row, column).map(|_| (row, column))
    }

    /// Headless string rendering of the composed terminal grid.
    pub fn render_to_string(&self) -> Result<String, ScreenError> {
        self.composed_grid().map(|grid| grid.render_to_string())
    }

    fn composite_window(&self, target: &mut ComposedGrid, window: &WindowPlacement) {
        let Some(source) = self.grids.get(&window.grid) else {
            return;
        };
        let copy_height = source.height.min(window.height);
        let copy_width = source.width.min(window.width);
        for source_row in 0..copy_height {
            let Some(target_row) = add_coordinate(window.row, source_row) else {
                continue;
            };
            if target_row >= target.height {
                continue;
            }
            for source_column in 0..copy_width {
                let Some(target_column) = add_coordinate(window.column, source_column) else {
                    continue;
                };
                if target_column >= target.width {
                    continue;
                }
                let Some(source_index) = source.index(source_row, source_column) else {
                    continue;
                };
                let Some(target_index) = target_row
                    .checked_mul(target.width)
                    .and_then(|base| base.checked_add(target_column))
                else {
                    continue;
                };
                if let (Some(source_cell), Some(target_cell)) =
                    (source.cells.get(source_index), target.cells.get_mut(target_index))
                {
                    let underlay = target_cell.highlight_id;
                    target_cell.clone_from(source_cell);
                    if window.kind == WindowKind::Floating {
                        let blend = self
                            .highlights
                            .get(&source_cell.highlight_id)
                            .and_then(|highlight| dict_integer(&highlight.rgb, b"blend"))
                            .and_then(|value| u8::try_from(value).ok())
                            .unwrap_or(0)
                            .min(100);
                        if blend > 0 {
                            target_cell.blend_underlay = Some(underlay);
                            target_cell.blend_percentage = blend;
                        }
                    }
                }
            }
        }
    }

    fn apply_known(&mut self, name: &OxStr, args: &[Object]) -> Result<(), ScreenError> {
        if name.as_bytes() == b"grid_resize" {
            self.grid_resize(args)
        } else if name.as_bytes() == b"grid_destroy" {
            self.grid_destroy(args)
        } else if name.as_bytes() == b"grid_clear" {
            self.grid_clear(args)
        } else if name.as_bytes() == b"grid_line" {
            self.grid_line(args)
        } else if name.as_bytes() == b"grid_scroll" {
            self.grid_scroll(args)
        } else if name.as_bytes() == b"grid_cursor_goto" {
            self.grid_cursor_goto(args)
        } else if name.as_bytes() == b"win_pos" {
            self.win_pos(args)
        } else if name.as_bytes() == b"win_float_pos" {
            self.win_float_pos(args)
        } else if name.as_bytes() == b"win_external_pos" {
            self.win_external_pos(args)
        } else if name.as_bytes() == b"win_hide" {
            self.win_hide(args)
        } else if name.as_bytes() == b"win_close" {
            self.win_close(args)
        } else if name.as_bytes() == b"win_viewport" {
            self.win_viewport(args)
        } else if name.as_bytes() == b"win_viewport_margins" {
            self.win_viewport_margins(args)
        } else if name.as_bytes() == b"hl_attr_define" {
            self.hl_attr_define(args)
        } else if name.as_bytes() == b"mode_info_set" {
            self.mode_info_set(args)
        } else {
            self.mode_change(args)
        }
    }

    fn grid_resize(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("grid_resize", args, 3)?;
        let grid = integer(args, 0, "grid_resize", "grid")?;
        let width = unsigned(args, 1, "grid_resize", "width")?;
        let height = unsigned(args, 2, "grid_resize", "height")?;
        if let Some(existing) = self.grids.get_mut(&grid) {
            existing.resize(width, height)?;
        } else {
            self.grids.insert(grid, Grid::new(width, height)?);
        }
        if let Some(window) = self.windows.get_mut(&grid) {
            window.width = width;
            window.height = height;
        }
        Ok(())
    }

    fn grid_destroy(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("grid_destroy", args, 1)?;
        let grid = integer(args, 0, "grid_destroy", "grid")?;
        self.grids.remove(&grid);
        self.windows.remove(&grid);
        self.viewports.remove(&grid);
        self.margins.remove(&grid);
        if self.cursor.is_some_and(|cursor| cursor.grid == grid) {
            self.cursor = None;
        }
        Ok(())
    }

    fn grid_clear(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("grid_clear", args, 1)?;
        let grid = integer(args, 0, "grid_clear", "grid")?;
        let target = self.grids.get_mut(&grid).ok_or(ScreenError::UnknownGrid {
            event: "grid_clear",
            grid,
        })?;
        target.clear();
        Ok(())
    }

    fn grid_line(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("grid_line", args, 5)?;
        let grid_id = integer(args, 0, "grid_line", "grid")?;
        let row = unsigned(args, 1, "grid_line", "row")?;
        let mut column = unsigned(args, 2, "grid_line", "column")?;
        let Object::Array(tuples) = argument(args, 3, "grid_line", "cells", "an array")? else {
            return Err(wrong_type("grid_line", 3, "cells", "an array"));
        };
        let wrap = boolean(args, 4, "grid_line", "wrap")?;
        let grid = self.grids.get_mut(&grid_id).ok_or(ScreenError::UnknownGrid {
            event: "grid_line",
            grid: grid_id,
        })?;
        if row >= grid.height || column > grid.width {
            return Err(ScreenError::GridPositionOutOfBounds {
                event: "grid_line",
                grid: grid_id,
                row,
                column,
            });
        }

        let mut current_highlight = 0;
        for (tuple_index, tuple) in tuples.iter().enumerate() {
            let Object::Array(parts) = tuple else {
                return Err(wrong_type(
                    "grid_line",
                    3,
                    "cells tuple",
                    "an array",
                ));
            };
            if !(1..=3).contains(&parts.len()) {
                return Err(ScreenError::InvalidCellTupleArity { tuple: tuple_index });
            }
            let text = match parts.first() {
                Some(Object::String(text)) => text.clone(),
                _ => return Err(wrong_type("grid_line", 3, "cell text", "a byte string")),
            };
            if let Some(value) = parts.get(1) {
                current_highlight = object_integer(value).ok_or_else(|| {
                    wrong_type("grid_line", 3, "cell highlight", "an integer")
                })?;
            }
            let repeat = match parts.get(2) {
                Some(value) => object_integer(value).ok_or_else(|| {
                    wrong_type("grid_line", 3, "cell repeat", "an integer")
                })?,
                None => 1,
            };
            if repeat <= 0 {
                return Err(ScreenError::InvalidCellRepeat {
                    tuple: tuple_index,
                    repeat,
                });
            }
            let repeat = usize::try_from(repeat).map_err(|_| ScreenError::InvalidCellRepeat {
                tuple: tuple_index,
                repeat,
            })?;
            for _ in 0..repeat {
                if column >= grid.width {
                    break;
                }
                let Some(index) = grid.index(row, column) else {
                    break;
                };
                if let Some(cell) = grid.cells.get_mut(index) {
                    *cell = Cell {
                        text: text.clone(),
                        highlight_id: current_highlight,
                        blend_underlay: None,
                        blend_percentage: 0,
                    };
                }
                column = column.saturating_add(1);
            }
        }
        if let Some(row_wrap) = grid.wrapped_rows.get_mut(row) {
            *row_wrap = wrap;
        }
        Ok(())
    }

    fn grid_scroll(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("grid_scroll", args, 7)?;
        let grid_id = integer(args, 0, "grid_scroll", "grid")?;
        let top = unsigned(args, 1, "grid_scroll", "top")?;
        let bottom = unsigned(args, 2, "grid_scroll", "bottom")?;
        let left = unsigned(args, 3, "grid_scroll", "left")?;
        let right = unsigned(args, 4, "grid_scroll", "right")?;
        let rows = integer(args, 5, "grid_scroll", "rows")?;
        let columns = integer(args, 6, "grid_scroll", "columns")?;
        let grid = self.grids.get_mut(&grid_id).ok_or(ScreenError::UnknownGrid {
            event: "grid_scroll",
            grid: grid_id,
        })?;
        if top > bottom
            || left > right
            || bottom > grid.height
            || right > grid.width
        {
            return Err(ScreenError::InvalidScrollRegion {
                grid: grid_id,
                top,
                bottom,
                left,
                right,
            });
        }
        let old_cells = grid.cells.clone();
        let old_wraps = grid.wrapped_rows.clone();
        for destination_row in top..bottom {
            for destination_column in left..right {
                let source_row = offset_index(destination_row, rows);
                let source_column = offset_index(destination_column, columns);
                let replacement = match (source_row, source_column) {
                    (Some(source_row), Some(source_column))
                        if (top..bottom).contains(&source_row)
                            && (left..right).contains(&source_column) =>
                    {
                        grid.index(source_row, source_column)
                            .and_then(|index| old_cells.get(index))
                            .cloned()
                            .unwrap_or_default()
                    }
                    _ => Cell::default(),
                };
                if let Some(index) = grid.index(destination_row, destination_column) {
                    if let Some(cell) = grid.cells.get_mut(index) {
                        *cell = replacement;
                    }
                }
            }
        }
        if left == 0 && right == grid.width && columns == 0 {
            for destination_row in top..bottom {
                let replacement = offset_index(destination_row, rows)
                    .filter(|source_row| (top..bottom).contains(source_row))
                    .and_then(|source_row| old_wraps.get(source_row))
                    .copied()
                    .unwrap_or(false);
                if let Some(wrap) = grid.wrapped_rows.get_mut(destination_row) {
                    *wrap = replacement;
                }
            }
        }
        Ok(())
    }

    fn grid_cursor_goto(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("grid_cursor_goto", args, 3)?;
        let grid_id = integer(args, 0, "grid_cursor_goto", "grid")?;
        let row = unsigned(args, 1, "grid_cursor_goto", "row")?;
        let column = unsigned(args, 2, "grid_cursor_goto", "column")?;
        let grid = self.grids.get(&grid_id).ok_or(ScreenError::UnknownGrid {
            event: "grid_cursor_goto",
            grid: grid_id,
        })?;
        if grid.cell(row, column).is_none() {
            return Err(ScreenError::GridPositionOutOfBounds {
                event: "grid_cursor_goto",
                grid: grid_id,
                row,
                column,
            });
        }
        self.cursor = Some(Cursor {
            grid: grid_id,
            row,
            column,
        });
        Ok(())
    }

    fn win_pos(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("win_pos", args, 6)?;
        let grid = integer(args, 0, "win_pos", "grid")?;
        self.require_grid("win_pos", grid)?;
        let window = window_id(args, 1, "win_pos")?;
        let row = integer(args, 2, "win_pos", "start_row")?;
        let column = integer(args, 3, "win_pos", "start_column")?;
        let width = unsigned(args, 4, "win_pos", "width")?;
        let height = unsigned(args, 5, "win_pos", "height")?;
        let sequence = self.sequence();
        self.windows.insert(
            grid,
            WindowPlacement {
                grid,
                window,
                kind: WindowKind::Normal,
                row,
                column,
                width,
                height,
                hidden: false,
                mouse_enabled: false,
                z_index: 0,
                composition_index: 0,
                anchor: None,
                anchor_grid: None,
                anchor_row: None,
                anchor_column: None,
                sequence,
            },
        );
        Ok(())
    }

    fn win_float_pos(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("win_float_pos", args, 11)?;
        let grid = integer(args, 0, "win_float_pos", "grid")?;
        let dimensions = self.require_grid("win_float_pos", grid)?;
        let window = window_id(args, 1, "win_float_pos")?;
        let anchor = string(args, 2, "win_float_pos", "anchor")?.clone();
        let anchor_grid = integer(args, 3, "win_float_pos", "anchor_grid")?;
        let anchor_row = number(args, 4, "win_float_pos", "anchor_row")?;
        let anchor_column = number(args, 5, "win_float_pos", "anchor_column")?;
        let mouse_enabled = boolean(args, 6, "win_float_pos", "mouse_enabled")?;
        let z_index = integer(args, 7, "win_float_pos", "z_index")?;
        let composition_index = integer(args, 8, "win_float_pos", "composition_index")?;
        let row = integer(args, 9, "win_float_pos", "screen_row")?;
        let column = integer(args, 10, "win_float_pos", "screen_column")?;
        let sequence = self.sequence();
        self.windows.insert(
            grid,
            WindowPlacement {
                grid,
                window,
                kind: WindowKind::Floating,
                row,
                column,
                width: dimensions.0,
                height: dimensions.1,
                hidden: false,
                mouse_enabled,
                z_index,
                composition_index,
                anchor: Some(anchor),
                anchor_grid: Some(anchor_grid),
                anchor_row: Some(anchor_row),
                anchor_column: Some(anchor_column),
                sequence,
            },
        );
        Ok(())
    }

    fn win_external_pos(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("win_external_pos", args, 2)?;
        let grid = integer(args, 0, "win_external_pos", "grid")?;
        let dimensions = self.require_grid("win_external_pos", grid)?;
        let window = window_id(args, 1, "win_external_pos")?;
        let sequence = self.sequence();
        self.windows.insert(
            grid,
            WindowPlacement {
                grid,
                window,
                kind: WindowKind::External,
                row: 0,
                column: 0,
                width: dimensions.0,
                height: dimensions.1,
                hidden: false,
                mouse_enabled: false,
                z_index: 0,
                composition_index: 0,
                anchor: None,
                anchor_grid: None,
                anchor_row: None,
                anchor_column: None,
                sequence,
            },
        );
        Ok(())
    }

    fn win_hide(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("win_hide", args, 1)?;
        let grid = integer(args, 0, "win_hide", "grid")?;
        if let Some(window) = self.windows.get_mut(&grid) {
            window.hidden = true;
        }
        Ok(())
    }

    fn win_close(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("win_close", args, 1)?;
        let grid = integer(args, 0, "win_close", "grid")?;
        self.windows.remove(&grid);
        self.viewports.remove(&grid);
        self.margins.remove(&grid);
        Ok(())
    }

    fn win_viewport(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("win_viewport", args, 8)?;
        let grid = integer(args, 0, "win_viewport", "grid")?;
        self.require_grid("win_viewport", grid)?;
        self.viewports.insert(
            grid,
            Viewport {
                window: window_id(args, 1, "win_viewport")?,
                top_line: integer(args, 2, "win_viewport", "top_line")?,
                bottom_line: integer(args, 3, "win_viewport", "bottom_line")?,
                cursor_line: integer(args, 4, "win_viewport", "cursor_line")?,
                cursor_column: integer(args, 5, "win_viewport", "cursor_column")?,
                line_count: integer(args, 6, "win_viewport", "line_count")?,
                scroll_delta: integer(args, 7, "win_viewport", "scroll_delta")?,
            },
        );
        Ok(())
    }

    fn win_viewport_margins(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("win_viewport_margins", args, 6)?;
        let grid = integer(args, 0, "win_viewport_margins", "grid")?;
        self.require_grid("win_viewport_margins", grid)?;
        let window = window_id(args, 1, "win_viewport_margins")?;
        self.margins.insert(
            grid,
            ViewportMargins {
                window,
                top: unsigned(args, 2, "win_viewport_margins", "top")?,
                bottom: unsigned(args, 3, "win_viewport_margins", "bottom")?,
                left: unsigned(args, 4, "win_viewport_margins", "left")?,
                right: unsigned(args, 5, "win_viewport_margins", "right")?,
            },
        );
        Ok(())
    }

    fn hl_attr_define(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("hl_attr_define", args, 4)?;
        let id = integer(args, 0, "hl_attr_define", "id")?;
        let Object::Dict(rgb) = argument(args, 1, "hl_attr_define", "rgb", "a dictionary")? else {
            return Err(wrong_type("hl_attr_define", 1, "rgb", "a dictionary"));
        };
        let Object::Dict(cterm) = argument(args, 2, "hl_attr_define", "cterm", "a dictionary")? else {
            return Err(wrong_type("hl_attr_define", 2, "cterm", "a dictionary"));
        };
        let Object::Array(info) = argument(args, 3, "hl_attr_define", "info", "an array")? else {
            return Err(wrong_type("hl_attr_define", 3, "info", "an array"));
        };
        self.highlights.insert(
            id,
            HighlightDefinition {
                rgb: rgb.clone(),
                cterm: cterm.clone(),
                info: info.clone(),
            },
        );
        Ok(())
    }

    fn mode_info_set(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("mode_info_set", args, 2)?;
        let enabled = boolean(args, 0, "mode_info_set", "cursor_style_enabled")?;
        let Object::Array(mode_objects) =
            argument(args, 1, "mode_info_set", "mode_info", "an array")?
        else {
            return Err(wrong_type("mode_info_set", 1, "mode_info", "an array"));
        };
        let modes = mode_objects
            .iter()
            .enumerate()
            .map(|(index, mode)| parse_mode_info(index, mode))
            .collect::<Result<Vec<_>, _>>()?;
        self.cursor_style_enabled = enabled;
        self.modes = modes;
        self.active_mode = None;
        Ok(())
    }

    fn mode_change(&mut self, args: &[Object]) -> Result<(), ScreenError> {
        expect_arity("mode_change", args, 2)?;
        let name = string(args, 0, "mode_change", "mode")?.clone();
        let index = unsigned(args, 1, "mode_change", "mode_index")?;
        let info = self
            .modes
            .get(index)
            .cloned()
            .ok_or(ScreenError::UnknownModeIndex { index })?;
        self.active_mode = Some(ActiveMode { name, index, info });
        Ok(())
    }

    fn require_grid(
        &self,
        event: &'static str,
        grid: GridId,
    ) -> Result<(usize, usize), ScreenError> {
        self.grids
            .get(&grid)
            .map(|value| (value.width, value.height))
            .ok_or(ScreenError::UnknownGrid { event, grid })
    }

    fn sequence(&mut self) -> u64 {
        let current = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        current
    }
}

fn is_screen_event(name: &OxStr) -> bool {
    const NAMES: [&[u8]; 16] = [
        b"grid_resize",
        b"grid_destroy",
        b"grid_clear",
        b"grid_line",
        b"grid_scroll",
        b"grid_cursor_goto",
        b"win_pos",
        b"win_float_pos",
        b"win_external_pos",
        b"win_hide",
        b"win_close",
        b"win_viewport",
        b"win_viewport_margins",
        b"hl_attr_define",
        b"mode_info_set",
        b"mode_change",
    ];
    NAMES.iter().any(|candidate| name.as_bytes() == *candidate)
}

fn expect_arity(
    event: &'static str,
    args: &[Object],
    expected: usize,
) -> Result<(), ScreenError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(ScreenError::WrongArity {
            event,
            expected,
            actual: args.len(),
        })
    }
}

fn argument<'a>(
    args: &'a [Object],
    index: usize,
    event: &'static str,
    field: &'static str,
    expected: &'static str,
) -> Result<&'a Object, ScreenError> {
    args.get(index)
        .ok_or(ScreenError::WrongType {
            event,
            index,
            field,
            expected,
        })
}

fn wrong_type(
    event: &'static str,
    index: usize,
    field: &'static str,
    expected: &'static str,
) -> ScreenError {
    ScreenError::WrongType {
        event,
        index,
        field,
        expected,
    }
}

fn object_integer(value: &Object) -> Option<i64> {
    match value {
        Object::Integer(value) => Some(*value),
        _ => None,
    }
}

fn integer(
    args: &[Object],
    index: usize,
    event: &'static str,
    field: &'static str,
) -> Result<i64, ScreenError> {
    object_integer(argument(args, index, event, field, "an integer")?)
        .ok_or_else(|| wrong_type(event, index, field, "an integer"))
}

fn unsigned(
    args: &[Object],
    index: usize,
    event: &'static str,
    field: &'static str,
) -> Result<usize, ScreenError> {
    let value = integer(args, index, event, field)?;
    usize::try_from(value).map_err(|_| ScreenError::OutOfRange {
        event,
        index,
        field,
        value,
    })
}

fn boolean(
    args: &[Object],
    index: usize,
    event: &'static str,
    field: &'static str,
) -> Result<bool, ScreenError> {
    match argument(args, index, event, field, "a boolean")? {
        Object::Boolean(value) => Ok(*value),
        _ => Err(wrong_type(event, index, field, "a boolean")),
    }
}

fn string<'a>(
    args: &'a [Object],
    index: usize,
    event: &'static str,
    field: &'static str,
) -> Result<&'a OxStr, ScreenError> {
    match argument(args, index, event, field, "a byte string")? {
        Object::String(value) => Ok(value),
        _ => Err(wrong_type(event, index, field, "a byte string")),
    }
}

fn number(
    args: &[Object],
    index: usize,
    event: &'static str,
    field: &'static str,
) -> Result<f64, ScreenError> {
    match argument(args, index, event, field, "a number")? {
        Object::Float(value) => Ok(*value),
        Object::Integer(value) => Ok(*value as f64),
        _ => Err(wrong_type(event, index, field, "a number")),
    }
}

fn window_id(
    args: &[Object],
    index: usize,
    event: &'static str,
) -> Result<WindowId, ScreenError> {
    match argument(args, index, event, "window", "a window handle")? {
        Object::Window(handle) => Ok(i64::from(*handle)),
        Object::Integer(handle) if *handle >= 0 => Ok(*handle),
        Object::Integer(handle) => Err(ScreenError::OutOfRange {
            event,
            index,
            field: "window",
            value: *handle,
        }),
        _ => Err(wrong_type(event, index, "window", "a window handle")),
    }
}

fn parse_mode_info(index: usize, value: &Object) -> Result<ModeInfo, ScreenError> {
    let Object::Dict(fields) = value else {
        return Err(ScreenError::InvalidModeField {
            mode: index,
            field: OxStr::from("mode_info"),
            expected: "a dictionary",
        });
    };
    let lookup = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key.as_bytes() == name.as_bytes())
            .map(|(_, value)| value)
    };
    let optional_integer = |name: &str| -> Result<Option<i64>, ScreenError> {
        match lookup(name) {
            None => Ok(None),
            Some(Object::Integer(value)) => Ok(Some(*value)),
            Some(_) => Err(ScreenError::InvalidModeField {
                mode: index,
                field: OxStr::from(name),
                expected: "an integer",
            }),
        }
    };
    let optional_u64 = |name: &str| -> Result<Option<u64>, ScreenError> {
        optional_integer(name)?.map_or(Ok(None), |value| {
            u64::try_from(value)
                .map(Some)
                .map_err(|_| ScreenError::InvalidModeField {
                    mode: index,
                    field: OxStr::from(name),
                    expected: "a non-negative integer",
                })
        })
    };
    let optional_string = |name: &str| -> Result<Option<OxStr>, ScreenError> {
        match lookup(name) {
            None => Ok(None),
            Some(Object::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(ScreenError::InvalidModeField {
                mode: index,
                field: OxStr::from(name),
                expected: "a byte string",
            }),
        }
    };
    let cursor_shape = optional_string("cursor_shape")?.map(|shape| {
        if shape.as_bytes() == b"block" {
            CursorShape::Block
        } else if shape.as_bytes() == b"horizontal" {
            CursorShape::Horizontal
        } else if shape.as_bytes() == b"vertical" {
            CursorShape::Vertical
        } else {
            CursorShape::Unknown(shape)
        }
    });
    let cell_percentage = optional_integer("cell_percentage")?
        .map(|value| {
            u8::try_from(value).map_err(|_| ScreenError::InvalidModeField {
                mode: index,
                field: OxStr::from("cell_percentage"),
                expected: "an integer from 0 through 100",
            })
        })
        .transpose()?;
    if cell_percentage.is_some_and(|percentage| percentage > 100) {
        return Err(ScreenError::InvalidModeField {
            mode: index,
            field: OxStr::from("cell_percentage"),
            expected: "an integer from 0 through 100",
        });
    }
    Ok(ModeInfo {
        cursor_shape,
        cell_percentage,
        blink_wait: optional_u64("blinkwait")?,
        blink_on: optional_u64("blinkon")?,
        blink_off: optional_u64("blinkoff")?,
        attr_id: optional_integer("attr_id")?,
        attr_id_lmap: optional_integer("attr_id_lm")?,
        short_name: optional_string("short_name")?,
        name: optional_string("name")?,
    })
}

fn offset_index(index: usize, offset: i64) -> Option<usize> {
    let base = i128::try_from(index).ok()?;
    let shifted = base.checked_add(i128::from(offset))?;
    usize::try_from(shifted).ok()
}

fn add_coordinate(origin: i64, offset: usize) -> Option<usize> {
    let offset = i128::try_from(offset).ok()?;
    let sum = i128::from(origin).checked_add(offset)?;
    usize::try_from(sum).ok()
}

fn dict_integer(dict: &ox_types::Dict, key: &[u8]) -> Option<i64> {
    dict.iter().find_map(|(candidate, value)| {
        if candidate.as_bytes() != key {
            return None;
        }
        match value {
            Object::Integer(value) => Some(*value),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use ox_types::WinHandle;

    fn event(name: &str, args: Vec<Object>) -> RedrawEvent {
        RedrawEvent {
            name: OxStr::from(name),
            argsets: vec![args],
        }
    }

    fn apply(screen: &mut Screen, name: &str, args: Vec<Object>) {
        screen.apply_event(&event(name, args)).unwrap();
    }

    fn resize(screen: &mut Screen, grid: i64, width: i64, height: i64) {
        apply(
            screen,
            "grid_resize",
            vec![
                Object::Integer(grid),
                Object::Integer(width),
                Object::Integer(height),
            ],
        );
    }

    fn tuple(text: &str, highlight: Option<i64>, repeat: Option<i64>) -> Object {
        let mut values = vec![Object::String(OxStr::from(text))];
        if let Some(highlight) = highlight {
            values.push(Object::Integer(highlight));
        }
        if let Some(repeat) = repeat {
            values.push(Object::Integer(repeat));
        }
        Object::Array(values)
    }

    fn line(screen: &mut Screen, grid: i64, row: i64, cells: Vec<Object>) {
        apply(
            screen,
            "grid_line",
            vec![
                Object::Integer(grid),
                Object::Integer(row),
                Object::Integer(0),
                Object::Array(cells),
                Object::Boolean(false),
            ],
        );
    }

    fn rendered_grid(screen: &Screen, grid: i64) -> String {
        let target = screen.grid(grid).unwrap();
        let composed = ComposedGrid {
            width: target.width,
            height: target.height,
            cells: target.cells.clone(),
        };
        composed.render_to_string()
    }

    #[test]
    fn grid_line_expands_repeats_and_inherits_highlight() {
        let mut screen = Screen::new();
        resize(&mut screen, 1, 6, 1);
        line(
            &mut screen,
            1,
            0,
            vec![
                tuple("a", Some(7), Some(2)),
                tuple("b", None, None),
                tuple("c", Some(7), Some(3)),
            ],
        );
        assert_eq!(rendered_grid(&screen, 1), "aabccc");
        let grid = screen.grid(1).unwrap();
        assert_eq!(grid.cell(0, 2).unwrap().highlight_id, 7);
    }

    #[test]
    fn grid_scroll_moves_both_axes_and_blanks_vacated_cells() {
        let mut screen = Screen::new();
        resize(&mut screen, 1, 4, 3);
        line(&mut screen, 1, 0, vec![tuple("a", Some(1), Some(4))]);
        line(&mut screen, 1, 1, vec![tuple("b", Some(2), Some(4))]);
        line(&mut screen, 1, 2, vec![tuple("c", Some(3), Some(4))]);
        apply(
            &mut screen,
            "grid_scroll",
            vec![
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(3),
                Object::Integer(0),
                Object::Integer(4),
                Object::Integer(1),
                Object::Integer(1),
            ],
        );
        assert_eq!(rendered_grid(&screen, 1), "bbb \nccc \n    ");
    }

    #[test]
    fn cursor_and_mode_change_resolve_cursor_shape() {
        let mut screen = Screen::new();
        resize(&mut screen, 1, 5, 2);
        apply(
            &mut screen,
            "grid_cursor_goto",
            vec![Object::Integer(1), Object::Integer(1), Object::Integer(3)],
        );
        let fields = Dict(vec![
            (OxStr::from("cursor_shape"), Object::String(OxStr::from("vertical"))),
            (OxStr::from("cell_percentage"), Object::Integer(25)),
            (OxStr::from("name"), Object::String(OxStr::from("insert"))),
        ]);
        apply(
            &mut screen,
            "mode_info_set",
            vec![Object::Boolean(true), Object::Array(vec![Object::Dict(fields)])],
        );
        apply(
            &mut screen,
            "mode_change",
            vec![Object::String(OxStr::from("insert")), Object::Integer(0)],
        );
        assert_eq!(screen.cursor(), Some(Cursor { grid: 1, row: 1, column: 3 }));
        assert_eq!(screen.active_mode().unwrap().info.cursor_shape, Some(CursorShape::Vertical));
    }

    #[test]
    fn multigrid_overlap_uses_composition_order_and_clips_edges() {
        let mut screen = Screen::new();
        resize(&mut screen, 1, 5, 3);
        line(&mut screen, 1, 0, vec![tuple(".", Some(0), Some(5))]);
        line(&mut screen, 1, 1, vec![tuple(".", Some(0), Some(5))]);
        line(&mut screen, 1, 2, vec![tuple(".", Some(0), Some(5))]);
        resize(&mut screen, 2, 4, 2);
        line(&mut screen, 2, 0, vec![tuple("a", Some(1), Some(4))]);
        line(&mut screen, 2, 1, vec![tuple("A", Some(1), Some(4))]);
        resize(&mut screen, 3, 3, 2);
        line(&mut screen, 3, 0, vec![tuple("b", Some(2), Some(3))]);
        line(&mut screen, 3, 1, vec![tuple("B", Some(2), Some(3))]);
        let window_two = WinHandle::try_from(2).unwrap();
        let window_three = WinHandle::try_from(3).unwrap();
        apply(
            &mut screen,
            "win_float_pos",
            vec![
                Object::Integer(2), Object::Window(window_two), Object::String(OxStr::from("NW")),
                Object::Integer(1), Object::Float(0.0), Object::Float(0.0), Object::Boolean(false),
                Object::Integer(50), Object::Integer(10), Object::Integer(1), Object::Integer(2),
            ],
        );
        apply(
            &mut screen,
            "win_float_pos",
            vec![
                Object::Integer(3), Object::Window(window_three), Object::String(OxStr::from("NW")),
                Object::Integer(1), Object::Float(0.0), Object::Float(0.0), Object::Boolean(false),
                Object::Integer(40), Object::Integer(11), Object::Integer(0), Object::Integer(3),
            ],
        );
        assert_eq!(screen.render_to_string().unwrap(), "...bb\n..aBB\n..AAA");
    }

    #[test]
    fn headless_render_preserves_utf8_and_row_boundaries() {
        let mut screen = Screen::new();
        resize(&mut screen, 1, 2, 2);
        line(&mut screen, 1, 0, vec![tuple("λ", Some(4), None), tuple("x", None, None)]);
        line(&mut screen, 1, 1, vec![tuple("界", Some(5), None), tuple("", None, None)]);
        assert_eq!(screen.render_to_string().unwrap(), "λx\n界");
        assert_eq!(screen.composed_grid().unwrap().render_to_bytes(), "λx\n界".as_bytes());
    }

    #[test]
    fn unknown_event_is_returned_unchanged_for_upper_layer() {
        let mut screen = Screen::new();
        let unknown = event("popupmenu_show", vec![Object::Integer(1)]);
        assert_eq!(screen.apply_event(&unknown).unwrap(), ApplyOutcome::Unknown(unknown));
    }
}
