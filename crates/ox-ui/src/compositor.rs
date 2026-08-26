//! Server-side grid layering modeled after Neovim's UI compositor.

use ox_editor::{
    extmark::ExtmarkHighlightMode, BufferStateError, Editor, EditorError, Extmark, LayoutError,
};
use ox_text::BufferError;
use ox_types::{OxStr, WinHandle};
use thiserror::Error;
use unicode_width::UnicodeWidthStr;

use crate::grid::{Cell, Grid, GridError};
use crate::hl::{HlError, HlEvent, HlState};

/// Fixed stacking priority of the message grid.
pub const MESSAGE_ZINDEX: u32 = 200;

/// Semantic layer kind used to resolve equal stacking priorities.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LayerKind {
    /// Tiled editor content.
    Window,
    /// Floating editor content.
    Float,
    /// Message and command-line content.
    Message,
}

#[derive(Clone, Copy)]
enum MessageLayers {
    Include,
    Exclude,
}

/// A positioned grid participating in composition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layer {
    /// Layer contents.
    pub grid: Grid,
    /// Editor window represented by this layer, when applicable.
    pub window: Option<WinHandle>,
    /// Top screen row.
    pub row: isize,
    /// Left screen column.
    pub col: isize,
    /// Stacking priority.
    pub zindex: u32,
    /// Percentage of the underlying color mixed into this layer.
    pub winblend: u8,
    /// Semantic layer kind.
    pub kind: LayerKind,
    /// Whether blank cells cover lower layers.
    pub opaque: bool,
    /// Cursor within the layer, if any.
    pub cursor: Option<(usize, usize)>,
}

impl Layer {
    /// Creates a positioned layer.
    #[must_use]
    pub const fn new(grid: Grid, row: isize, col: isize, zindex: u32, kind: LayerKind) -> Self {
        Self { grid, window: None, row, col, zindex, winblend: 0, kind, opaque: true, cursor: None }
    }
}

/// Result of one composition pass.
#[derive(Clone, Debug, PartialEq)]
pub struct ComposedScreen {
    /// Flattened default grid.
    pub grid: Grid,
    /// Cursor on the flattened grid.
    pub cursor: Option<(usize, usize)>,
    /// Highlight definitions synthesized by winblend.
    pub highlight_events: Vec<HlEvent>,
}

/// Compositor failures.
#[derive(Debug, Error)]
pub enum CompositorError {
    /// Grid operation failed.
    #[error(transparent)]
    Grid(#[from] GridError),
    /// Highlight lookup or allocation failed.
    #[error(transparent)]
    Highlight(#[from] HlError),
    /// Editor snapshot access failed.
    #[error(transparent)]
    Editor(#[from] EditorError),
    /// Editor layout access failed.
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// Buffer state access failed.
    #[error(transparent)]
    BufferState(#[from] BufferStateError),
    /// Buffer text access failed.
    #[error(transparent)]
    Buffer(#[from] BufferError),
    /// Active tabpage is unavailable.
    #[error("editor has no active tabpage")]
    NoActiveTabpage,
}

/// Ordered collection of grids rendered into the default grid.
#[derive(Clone, Debug)]
pub struct Compositor {
    width: usize,
    height: usize,
    layers: Vec<Layer>,
}

impl Compositor {
    /// Creates an empty compositor.
    #[must_use]
    pub const fn new(width: usize, height: usize) -> Self {
        Self { width, height, layers: Vec::new() }
    }

    /// Screen width.
    #[must_use]
    pub const fn width(&self) -> usize { self.width }

    /// Screen height.
    #[must_use]
    pub const fn height(&self) -> usize { self.height }

    /// Adds a layer. Message layers are always normalized to z-index 200.
    pub fn push_layer(&mut self, mut layer: Layer) {
        if layer.kind == LayerKind::Message { layer.zindex = MESSAGE_ZINDEX; }
        self.layers.push(layer);
    }

    /// Removes all layers.
    pub fn clear(&mut self) { self.layers.clear(); }

    /// Returns layers in insertion order.
    #[must_use]
    pub fn layers(&self) -> &[Layer] { &self.layers }

    /// Builds renderable window layers from the active editor tabpage.
    pub fn from_editor(
        editor: &Editor,
        width: usize,
        height: usize,
        highlights: &mut HlState,
    ) -> Result<Self, CompositorError> {
        let tab_handle = editor.current_tabpage().ok_or(CompositorError::NoActiveTabpage)?;
        let tab = editor.tabpage(tab_handle)?;
        let current_window = editor.current_window();
        let mut compositor = Self::new(width, height);
        for (ordinal, window) in tab.windows().into_iter().enumerate() {
            let state = editor.window(window)?;
            let geometry = tab.window_geometry(window)?;
            let config = tab.window_config(window)?;
            let grid_id = i64::try_from(ordinal + 2).unwrap_or(i64::MAX);
            let mut grid = Grid::new(grid_id, geometry.width, geometry.height)?;
            let buffer_state = editor.buffer(state.buffer)?;
            let buffer = buffer_state.text()?;
            let marks = buffer_state.extmarks.render_ordered();
            for screen_row in 0..geometry.height {
                let line_number = state.topline.saturating_add(screen_row);
                if line_number > buffer.line_count() { break; }
                let bytes = buffer.line(line_number)?;
                let line = String::from_utf8_lossy(&bytes);
                grid.write_text(screen_row, 0, &line, 0)?;
                apply_extmark_highlights(
                    &mut grid,
                    screen_row,
                    line_number.saturating_sub(1),
                    0,
                    0,
                    &line,
                    &marks,
                    highlights,
                )?;
            }
            let kind = if config.is_some() { LayerKind::Float } else { LayerKind::Window };
            let zindex = config.map_or(0, |config| config.zindex);
            let mut layer = Layer::new(
                grid,
                isize::try_from(geometry.row).unwrap_or(isize::MAX),
                isize::try_from(geometry.col).unwrap_or(isize::MAX),
                zindex,
                kind,
            );
            layer.window = Some(window);
            layer.cursor = (current_window == Some(window)).then(|| {
                (state.cursor.lnum.saturating_sub(state.topline), state.cursor.col)
            });
            compositor.push_layer(layer);
        }
        Ok(compositor)
    }

    /// Flattens layers in stable z-order and resolves the topmost visible cursor.
    pub fn compose(&self, highlights: &mut HlState) -> Result<ComposedScreen, CompositorError> {
        self.compose_with_policy(highlights, MessageLayers::Include)
    }

    /// Flattens all layers except the built-in message grid.
    pub fn compose_without_messages(
        &self,
        highlights: &mut HlState,
    ) -> Result<ComposedScreen, CompositorError> {
        self.compose_with_policy(highlights, MessageLayers::Exclude)
    }

    fn compose_with_policy(
        &self,
        highlights: &mut HlState,
        messages: MessageLayers,
    ) -> Result<ComposedScreen, CompositorError> {
        let mut output = Grid::new(1, self.width, self.height)?;
        let mut order: Vec<usize> = (0..self.layers.len())
            .filter(|&index| {
                matches!(messages, MessageLayers::Include)
                    || self.layers[index].kind != LayerKind::Message
            })
            .collect();
        order.sort_by_key(|&index| {
            let layer = &self.layers[index];
            (layer.kind == LayerKind::Message, layer.zindex, layer.kind, index)
        });
        let mut cursor = None;
        let mut highlight_events = Vec::new();
        for index in order {
            let layer = &self.layers[index];
            for source_row in 0..layer.grid.height() {
                let Some(target_row) = source_row.checked_add_signed(layer.row) else { continue };
                if target_row >= self.height { continue; }
                for source_col in 0..layer.grid.width() {
                    let Some(target_col) = source_col.checked_add_signed(layer.col) else { continue };
                    if target_col >= self.width { continue; }
                    let source = layer.grid.cell(source_row, source_col)?.clone();
                    if !layer.opaque && source == Cell::blank() { continue; }
                    let cell = if layer.winblend == 0 {
                        source
                    } else {
                        let beneath = output.cell(target_row, target_col)?;
                        let (id, event) = highlights.premix(source.hl_id, beneath.hl_id, layer.winblend)?;
                        if let Some(event) = event { highlight_events.push(event); }
                        Cell { hl_id: id, ..source }
                    };
                    output.set_cell(target_row, target_col, cell)?;
                }
            }
            if let Some((row, col)) = layer.cursor {
                if let (Some(row), Some(col)) = (row.checked_add_signed(layer.row), col.checked_add_signed(layer.col)) {
                    if row < self.height && col < self.width { cursor = Some((row, col)); }
                }
            }
        }
        Ok(ComposedScreen { grid: output, cursor, highlight_events })
    }

    /// Returns the grid id assigned to an editor window in a multigrid stream.
    #[must_use]
    pub fn window_grid(&self, window: WinHandle, editor: &Editor) -> Option<i64> {
        let tab = editor.current_tabpage().and_then(|handle| editor.tabpage(handle).ok())?;
        tab.windows()
            .iter()
            .position(|candidate| *candidate == window)
            .and_then(|index| i64::try_from(index + 2).ok())
    }
}

fn apply_extmark_highlights(
    grid: &mut Grid,
    screen_row: usize,
    buffer_row: usize,
    text_offset: usize,
    segment_cell_start: usize,
    line: &str,
    marks: &[&Extmark],
    highlights: &mut HlState,
) -> Result<(), CompositorError> {
    let segment_width = grid.width().saturating_sub(text_offset);
    for mark in marks {
        let start = mark.position();
        let Some(end) = mark.placement.end.map(|end| end.position) else { continue };
        if buffer_row < start.row || buffer_row > end.row { continue; }

        let start_byte = if buffer_row == start.row { start.column } else { 0 };
        let end_byte = if buffer_row == end.row { end.column } else { line.len() };
        let absolute_start = display_column(line, start_byte);
        let absolute_end = display_column(line, end_byte);
        if absolute_start >= segment_cell_start.saturating_add(segment_width)
            || absolute_end <= segment_cell_start
        {
            continue;
        }
        let start_col = text_offset.saturating_add(absolute_start.saturating_sub(segment_cell_start));
        let end_col = text_offset.saturating_add(absolute_end.saturating_sub(segment_cell_start));

        let Some(name) = mark.placement.attributes.highlight_group.as_deref() else { continue };
        let Some(group_id) = highlights.group_id(&OxStr::from(name)) else { continue };
        for col in start_col.min(grid.width())..end_col.min(grid.width()) {
            let mut cell = grid.cell(screen_row, col)?.clone();
            cell.hl_id = match mark.placement.attributes.highlight_mode {
                Some(ExtmarkHighlightMode::Combine) => highlights.combine(cell.hl_id, group_id)?.0,
                Some(ExtmarkHighlightMode::Blend) => highlights.blend(cell.hl_id, group_id)?.0,
                None | Some(ExtmarkHighlightMode::Replace) => group_id,
            };
            grid.set_cell(screen_row, col, cell)?;
        }
    }
    Ok(())
}

fn display_column(line: &str, byte: usize) -> usize {
    let mut boundary = byte.min(line.len());
    while !line.is_char_boundary(boundary) {
        boundary -= 1;
    }
    UnicodeWidthStr::width(&line[..boundary])
}
