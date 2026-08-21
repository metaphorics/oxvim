//! Dense UI grid storage and minimal `grid_line` diff generation.

use ox_types::{Object, OxStr};
use thiserror::Error;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A rendered screen cell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    /// Grapheme displayed in this cell. Continuation cells use an empty string.
    pub text: OxStr,
    /// Highlight attribute identifier.
    pub hl_id: u64,
    /// Number of screen columns occupied by the grapheme (`0` for continuation).
    pub width: u8,
}

impl Cell {
    /// Constructs a one-column cell.
    #[must_use]
    pub fn new(text: impl Into<OxStr>, hl_id: u64) -> Self {
        Self { text: text.into(), hl_id, width: 1 }
    }

    /// Constructs a cell with an explicit display width.
    #[must_use]
    pub fn with_width(text: impl Into<OxStr>, hl_id: u64, width: u8) -> Self {
        Self { text: text.into(), hl_id, width }
    }

    /// The default blank cell.
    #[must_use]
    pub fn blank() -> Self {
        Self::new(" ", 0)
    }
}

/// A minimal changed span suitable for one `grid_line` call.
#[derive(Clone, Debug, PartialEq)]
pub struct GridLine {
    /// Zero-based row.
    pub row: usize,
    /// Zero-based first changed column.
    pub start_col: usize,
    /// Public `[text, hl_id?, repeat?]` tuples.
    pub cells: Vec<Object>,
    /// Whether this physical row wraps into the next row.
    pub wrap: bool,
}

/// Grid mutation failures.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum GridError {
    /// The requested coordinate is outside the grid.
    #[error("grid coordinate ({row}, {col}) is outside {height}x{width}")]
    OutOfBounds {
        /// Requested row.
        row: usize,
        /// Requested column.
        col: usize,
        /// Grid width.
        width: usize,
        /// Grid height.
        height: usize,
    },
    /// A grapheme width is not representable by this grid.
    #[error("cell width {width} is invalid at column {col} for grid width {grid_width}")]
    InvalidWidth {
        /// Requested display width.
        width: u8,
        /// Starting column.
        col: usize,
        /// Available grid width.
        grid_width: usize,
    },
    /// Grid dimensions overflow addressable storage.
    #[error("grid dimensions {width}x{height} overflow")]
    DimensionOverflow {
        /// Requested width.
        width: usize,
        /// Requested height.
        height: usize,
    },
}

/// Dense row-major server-side screen grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grid {
    id: i64,
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    wraps: Vec<bool>,
}

impl Grid {
    /// Creates a blank grid.
    pub fn new(id: i64, width: usize, height: usize) -> Result<Self, GridError> {
        let len = width.checked_mul(height).ok_or(GridError::DimensionOverflow { width, height })?;
        Ok(Self {
            id,
            width,
            height,
            cells: vec![Cell::blank(); len],
            wraps: vec![false; height],
        })
    }

    /// Stable UI grid identifier.
    #[must_use]
    pub const fn id(&self) -> i64 { self.id }

    /// Width in screen cells.
    #[must_use]
    pub const fn width(&self) -> usize { self.width }

    /// Height in rows.
    #[must_use]
    pub const fn height(&self) -> usize { self.height }

    /// Reads a cell.
    pub fn cell(&self, row: usize, col: usize) -> Result<&Cell, GridError> {
        let index = self.index(row, col)?;
        Ok(&self.cells[index])
    }

    /// Writes one cell without synthesizing continuation cells.
    pub fn set_cell(&mut self, row: usize, col: usize, cell: Cell) -> Result<(), GridError> {
        if cell.width > 1 && col.checked_add(usize::from(cell.width)).is_none_or(|end| end > self.width) {
            return Err(GridError::InvalidWidth { width: cell.width, col, grid_width: self.width });
        }
        let index = self.index(row, col)?;
        self.cells[index] = cell;
        Ok(())
    }

    /// Writes a grapheme and clears its continuation columns.
    pub fn put(
        &mut self,
        row: usize,
        col: usize,
        text: impl Into<OxStr>,
        hl_id: u64,
        width: u8,
    ) -> Result<(), GridError> {
        if width == 0 || col.checked_add(usize::from(width)).is_none_or(|end| end > self.width) {
            return Err(GridError::InvalidWidth { width, col, grid_width: self.width });
        }
        for target_col in (col..col + usize::from(width)).rev() {
            self.clear_grapheme_at(row, target_col)?;
        }
        self.set_cell(row, col, Cell::with_width(text, hl_id, width))?;
        for continuation in 1..usize::from(width) {
            self.set_cell(row, col + continuation, Cell::with_width("", hl_id, 0))?;
        }
        Ok(())
    }

    /// Writes terminal-width-aware Unicode clusters until the row boundary.
    pub fn write_text(
        &mut self,
        row: usize,
        start_col: usize,
        text: &str,
        hl_id: u64,
    ) -> Result<usize, GridError> {
        if start_col >= self.width {
            self.index(row, start_col)?;
        }
        let mut byte = 0;
        let mut col = start_col;
        while byte < text.len() && col < self.width {
            let end = cluster_end(text, byte);
            let cluster = &text[byte..end];
            let width = UnicodeWidthStr::width(cluster).max(1);
            if col.checked_add(width).is_none_or(|end_col| end_col > self.width) { break; }
            let width = u8::try_from(width).unwrap_or(u8::MAX);
            self.put(row, col, cluster, hl_id, width)?;
            col += usize::from(width);
            byte = end;
        }
        Ok(col)
    }

    /// Marks whether a row wraps into the following row.
    pub fn set_wrap(&mut self, row: usize, wrap: bool) -> Result<(), GridError> {
        if row >= self.height {
            return Err(GridError::OutOfBounds { row, col: 0, width: self.width, height: self.height });
        }
        self.wraps[row] = wrap;
        Ok(())
    }

    /// Clears all cells and wrapping flags.
    pub fn clear(&mut self) {
        self.cells.fill(Cell::blank());
        self.wraps.fill(false);
    }

    /// Resizes while preserving the overlapping top-left region.
    pub fn resize(&mut self, width: usize, height: usize) -> Result<(), GridError> {
        let len = width.checked_mul(height).ok_or(GridError::DimensionOverflow { width, height })?;
        let mut cells = vec![Cell::blank(); len];
        let copy_width = self.width.min(width);
        let copy_height = self.height.min(height);
        for row in 0..copy_height {
            let old = row * self.width;
            let new = row * width;
            cells[new..new + copy_width].clone_from_slice(&self.cells[old..old + copy_width]);
        }
        self.cells = cells;
        self.wraps.resize(height, false);
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Scrolls a rectangular region. Newly exposed cells are blank.
    pub fn scroll(
        &mut self,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
        rows: isize,
        cols: isize,
    ) -> Result<(), GridError> {
        if top > bottom || left > right || bottom > self.height || right > self.width {
            return Err(GridError::OutOfBounds {
                row: bottom,
                col: right,
                width: self.width,
                height: self.height,
            });
        }
        let old = self.cells.clone();
        for row in top..bottom {
            for col in left..right {
                let source_row = row.checked_add_signed(rows);
                let source_col = col.checked_add_signed(cols);
                let cell = match (source_row, source_col) {
                    (Some(source_row), Some(source_col))
                        if (top..bottom).contains(&source_row) && (left..right).contains(&source_col) =>
                    {
                        old[source_row * self.width + source_col].clone()
                    }
                    _ => Cell::blank(),
                };
                self.cells[row * self.width + col] = cell;
            }
        }
        Ok(())
    }

    /// Produces one minimal changed span per changed row.
    #[must_use]
    pub fn diff(&self, previous: &Self) -> Vec<GridLine> {
        let mut lines = Vec::new();
        let mut continue_wrap = false;
        for row in 0..self.height {
            let current = self.row(row);
            let old = previous.row(row);
            if current.is_empty() {
                continue_wrap = false;
                continue;
            }
            let common = current.len().min(old.len());
            let changed_first = (0..common)
                .find(|&col| current[col] != old[col])
                .or_else(|| (current.len() > common).then_some(common));
            let wrap_changed = self.wraps.get(row) != previous.wraps.get(row);
            let first = if continue_wrap {
                Some(0)
            } else {
                changed_first.or_else(|| wrap_changed.then_some(current.len() - 1))
            };
            let Some(first) = first else {
                continue_wrap = false;
                continue;
            };
            let changed_last = (first..common)
                .rev()
                .find(|&col| current[col] != old[col])
                .unwrap_or_else(|| if current.len() > common { current.len() - 1 } else { first });
            let last = if self.wraps[row] { current.len() - 1 } else { changed_last };
            lines.push(GridLine {
                row,
                start_col: first,
                cells: encode_cells(&current[first..=last]),
                wrap: self.wraps[row],
            });
            continue_wrap = self.wraps[row];
        }
        lines
    }

    /// Encodes every non-empty row, used for a newly attached UI.
    #[must_use]
    pub fn full_lines(&self) -> Vec<GridLine> {
        (0..self.height)
            .filter_map(|row| {
                let cells = self.row(row);
                (!cells.is_empty()).then(|| GridLine {
                    row,
                    start_col: 0,
                    cells: encode_cells(cells),
                    wrap: self.wraps[row],
                })
            })
            .collect()
    }

    /// Copies a clipped source rectangle into this grid.
    pub fn blit(&mut self, source: &Self, row_offset: isize, col_offset: isize) {
        for source_row in 0..source.height {
            let Some(target_row) = source_row.checked_add_signed(row_offset) else { continue };
            if target_row >= self.height { continue; }
            for source_col in 0..source.width {
                let Some(target_col) = source_col.checked_add_signed(col_offset) else { continue };
                if target_col >= self.width { continue; }
                self.cells[target_row * self.width + target_col] =
                    source.cells[source_row * source.width + source_col].clone();
            }
        }
    }

    fn row(&self, row: usize) -> &[Cell] {
        if row >= self.height { return &[]; }
        let start = row * self.width;
        &self.cells[start..start + self.width]
    }

    fn index(&self, row: usize, col: usize) -> Result<usize, GridError> {
        if row >= self.height || col >= self.width {
            return Err(GridError::OutOfBounds { row, col, width: self.width, height: self.height });
        }
        Ok(row * self.width + col)
    }

    fn clear_grapheme_at(&mut self, row: usize, col: usize) -> Result<(), GridError> {
        let index = self.index(row, col)?;
        let mut lead_col = col;
        while lead_col > 0 && self.cells[row * self.width + lead_col].width == 0 {
            lead_col -= 1;
        }
        let lead = row * self.width + lead_col;
        let old_width = usize::from(self.cells[lead].width.max(1));
        for clear_col in lead_col..lead_col.saturating_add(old_width).min(self.width) {
            self.cells[row * self.width + clear_col] = Cell::blank();
        }
        if index != lead {
            self.cells[index] = Cell::blank();
        }
        Ok(())
    }
}

fn cluster_end(text: &str, start: usize) -> usize {
    let mut chars = text[start..].char_indices();
    let Some((_, first)) = chars.next() else { return start };
    let mut end = start + first.len_utf8();
    let mut previous_was_joiner = first == '‍';
    let mut regional_count = usize::from(is_regional_indicator(first));
    for (offset, character) in chars {
        let regional_pair = is_regional_indicator(character) && regional_count % 2 == 1;
        let joins = UnicodeWidthChar::width(character).unwrap_or(0) == 0
            || previous_was_joiner
            || regional_pair;
        if !joins { break; }
        end = start + offset + character.len_utf8();
        previous_was_joiner = character == '‍';
        regional_count = if is_regional_indicator(character) { regional_count + 1 } else { 0 };
    }
    end
}

fn is_regional_indicator(character: char) -> bool {
    ('🇦'..='🇿').contains(&character)
}

fn encode_cells(cells: &[Cell]) -> Vec<Object> {
    let mut encoded = Vec::new();
    let mut index = 0;
    let mut previous_hl = None;
    while index < cells.len() {
        let cell = &cells[index];
        let mut repeat = 1usize;
        while index + repeat < cells.len() && cells[index + repeat] == *cell {
            repeat += 1;
        }
        let mut tuple = vec![Object::String(cell.text.clone())];
        let include_hl = previous_hl != Some(cell.hl_id) || repeat > 1;
        if include_hl {
            tuple.push(Object::Integer(i64::try_from(cell.hl_id).unwrap_or(i64::MAX)));
        }
        if repeat > 1 {
            tuple.push(Object::Integer(i64::try_from(repeat).unwrap_or(i64::MAX)));
        }
        encoded.push(Object::Array(tuple));
        previous_hl = Some(cell.hl_id);
        index += repeat;
    }
    encoded
}
