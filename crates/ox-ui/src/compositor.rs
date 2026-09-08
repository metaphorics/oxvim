//! Server-side grid layering modeled after Neovim's UI compositor.

use ox_editor::{
    BufferStateError, Editor, EditorError, Extmark, Geometry, LayoutError,
    extmark::ExtmarkHighlightMode,
};
use ox_text::BufferError;
use ox_types::{OxStr, WinHandle};
use thiserror::Error;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::grid::{Grid, GridError};
use crate::hl::{Highlight, HlAttrs, HlError, HlEvent, HlState};
use ox_editor::terminal_screen::{CellFlags, TermColor};

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
    /// Visible UI-watched extmarks in grid coordinates.
    pub watched_extmarks: Vec<WatchedExtmark>,
    /// Statusline text and highlight rendered on the default grid.
    pub statusline: Option<(String, u64)>,
}

/// One `win_extmark` payload produced while drawing a window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatchedExtmark {
    /// Public namespace identifier.
    pub namespace: u32,
    /// Namespace-local mark identifier.
    pub mark: u32,
    /// Draw row within the window grid.
    pub row: usize,
    /// Draw column within the window grid.
    pub col: usize,
    /// Buffer row used to distinguish viewport scrolling from mark movement.
    pub buffer_row: usize,
}

impl Layer {
    /// Creates a positioned layer.
    #[must_use]
    pub const fn new(grid: Grid, row: isize, col: isize, zindex: u32, kind: LayerKind) -> Self {
        Self {
            grid,
            window: None,
            row,
            col,
            zindex,
            winblend: 0,
            kind,
            opaque: true,
            cursor: None,
            watched_extmarks: Vec::new(),
            statusline: None,
        }
    }

    /// Re-points a retained layer at a new frame, keeping the grid and
    /// `watched_extmarks` allocations. Applies the same message z-index
    /// normalization as [`Compositor::push_layer`], because the refresh path
    /// pushes layers directly.
    pub fn reset(&mut self, row: isize, col: isize, zindex: u32, kind: LayerKind) {
        self.window = None;
        self.row = row;
        self.col = col;
        self.zindex = if kind == LayerKind::Message {
            MESSAGE_ZINDEX
        } else {
            zindex
        };
        self.winblend = 0;
        self.kind = kind;
        self.opaque = true;
        self.cursor = None;
        self.watched_extmarks.clear();
        self.statusline = None;
    }
}

/// Result of one composition pass. The composed pixels live in the caller's
/// output grid; this carries only what composition derives.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComposeOutcome {
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
    /// Resolved tabline runs for the top row: (column, text, hl id).
    /// Refreshed on every `refresh_from_editor`; empty means no tabline.
    tabline_row: Vec<(usize, String, u64)>,
    /// Whether the previous refresh painted a tabline row; drives the
    /// one-frame blank that clears stale cells on a 1-to-0 transition.
    tabline_was_shown: bool,
}

impl Compositor {
    /// Creates an empty compositor.
    #[must_use]
    pub const fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            layers: Vec::new(),
            tabline_row: Vec::new(),
            tabline_was_shown: false,
        }
    }

    /// Screen width.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Screen height.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Adds a layer. Message layers are always normalized to z-index 200.
    pub fn push_layer(&mut self, mut layer: Layer) {
        if layer.kind == LayerKind::Message {
            layer.zindex = MESSAGE_ZINDEX;
        }
        self.layers.push(layer);
    }

    /// Removes all layers.
    pub fn clear(&mut self) {
        self.layers.clear();
        self.tabline_row.clear();
    }

    /// Returns layers in insertion order.
    #[must_use]
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Rebuilds the layer stack from the active editor tabpage in place,
    /// reusing every retained layer whose grid id survives into this frame.
    ///
    /// Replaces the former `from_editor` constructor: the compositor is now
    /// long-lived render state owned by the caller, matching upstream's
    /// per-window `win_grid_alloc`, which reallocates only when the size
    /// changed.
    ///
    /// # Errors
    /// Returns [`CompositorError::NoActiveTabpage`] when no tabpage is active, or
    /// editor, buffer, grid, and highlight errors from reading the snapshot,
    /// building window grids, or interning highlights.
    #[expect(
        clippy::too_many_lines,
        reason = "window rendering is an order-sensitive pass over editor state"
    )]
    pub fn refresh_from_editor(
        &mut self,
        editor: &Editor,
        width: usize,
        height: usize,
        highlights: &mut HlState,
    ) -> Result<(), CompositorError> {
        let tab_handle = editor
            .current_tabpage()
            .ok_or(CompositorError::NoActiveTabpage)?;
        let tab = editor.tabpage(tab_handle)?;
        let current_window = editor.current_window();
        let non_text = Highlight {
            rgb: HlAttrs {
                foreground: Some(0x00_00_ff),
                bold: true,
                ..HlAttrs::default()
            },
            cterm: HlAttrs {
                foreground: Some(12),
                bold: true,
                fg_indexed: true,
                ..HlAttrs::default()
            },
            cterm_explicit: true,
            ..Highlight::default()
        };
        let (non_text_id, _) = highlights.intern(non_text)?;
        let sign_id = match highlights.group_id(&OxStr::from("SignColumn")) {
            Some(id) => id,
            None => highlights.define_group(
                "SignColumn",
                Highlight {
                    rgb: HlAttrs {
                        foreground: Some(0x00_00_8b),
                        background: Some(0x80_80_80),
                        ..HlAttrs::default()
                    },
                    ..Highlight::default()
                },
            )?,
        };
        let (statusline_id, _) = highlights.intern(Highlight {
            rgb: HlAttrs {
                bold: true,
                reverse: true,
                ..HlAttrs::default()
            },
            cterm: HlAttrs {
                bold: true,
                reverse: true,
                ..HlAttrs::default()
            },
            cterm_explicit: true,
            ..Highlight::default()
        })?;
        let (statusline_nc_id, _) = highlights.intern(Highlight {
            rgb: HlAttrs {
                reverse: true,
                ..HlAttrs::default()
            },
            cterm: HlAttrs {
                reverse: true,
                ..HlAttrs::default()
            },
            cterm_explicit: true,
            ..Highlight::default()
        })?;
        // The tabline reserves the top rows of the default grid
        // (`tabline_height`, window.c:7416-7429); windows shift down by it.
        let tabline = crate::tabline::tabline_layout(editor, width);
        let tabline_top = tabline.height;
        self.tabline_was_shown = !self.tabline_row.is_empty();
        self.tabline_row = resolve_tabline_row(&tabline.cells, highlights)?;
        let mut retired: Vec<Layer> = std::mem::take(&mut self.layers);
        self.width = width;
        self.height = height;
        let windows = tab.windows();
        let tiled_count = tab.layout().window_count();
        let tiled_split = tiled_count > 1;
        for window in windows {
            let state = editor.window(window)?;
            let geometry = tab.window_geometry(window)?;
            let config = tab.window_config(window)?;
            let is_float = config.is_some();
            let grid_id = window_grid_id(window);
            let (layer_row, grid_height) = if is_float {
                (
                    isize::try_from(geometry.row + tabline_top).unwrap_or(isize::MAX),
                    geometry.height.max(1),
                )
            } else {
                let (row, grid_height) = tiled_window_grid_geometry(
                    geometry,
                    height.saturating_sub(tabline_top),
                    tiled_split,
                );
                (
                    row.saturating_add(isize::try_from(tabline_top).unwrap_or(isize::MAX)),
                    grid_height,
                )
            };
            let kind = if is_float {
                LayerKind::Float
            } else {
                LayerKind::Window
            };
            let zindex = config.map_or(0, |config| config.zindex);
            let mut layer = match take_layer(&mut retired, grid_id) {
                Some(layer) => layer,
                None => Layer::new(
                    Grid::new(grid_id, geometry.width, grid_height)?,
                    0,
                    0,
                    0,
                    LayerKind::Window,
                ),
            };
            layer.grid.reshape(geometry.width, grid_height)?;
            layer.reset(
                layer_row,
                isize::try_from(geometry.col).unwrap_or(isize::MAX),
                zindex,
                kind,
            );
            let mut grid = layer.grid;
            let buffer_state = editor.buffer(state.buffer)?;
            let buffer = buffer_state.text()?;
            let is_terminal = editor.is_terminal_buffer(state.buffer);
            let marks = buffer_state.extmarks.render_ordered();
            let line_count = buffer.line_count();
            // Sparse sign-coverage sweep: the deepest overlap of sign row
            // ranges decides the slot count, without allocating a counter
            // per buffer line on every redraw.
            let mut sign_starts: Vec<usize> = Vec::new();
            let mut sign_ends: Vec<usize> = Vec::new();
            for mark in marks
                .iter()
                .filter(|mark| mark.placement.attributes.sign_text.is_some())
            {
                let start = mark.position().row;
                if start >= line_count {
                    continue;
                }
                let end = mark
                    .placement
                    .end
                    .map_or(start, |end| end.position.row)
                    .min(line_count.saturating_sub(1));
                sign_starts.push(start);
                sign_ends.push(end.max(start));
            }
            sign_starts.sort_unstable();
            sign_ends.sort_unstable();
            let mut sign_slots = 0usize;
            let mut live = 0usize;
            let mut expired = 0usize;
            for &start in &sign_starts {
                while expired < sign_ends.len() && sign_ends[expired] < start {
                    live -= 1;
                    expired += 1;
                }
                live += 1;
                sign_slots = sign_slots.max(live);
            }
            let sign_slots = sign_slots.min(3);
            let sign_width = sign_slots.saturating_mul(2);
            let text_height = grid_height;
            let text_width = geometry.width.saturating_sub(sign_width).max(1);
            // Bin sign marks by buffer row once per redraw so each drawn
            // segment looks up its line instead of rescanning every mark.
            let mut sign_marks_by_row: std::collections::HashMap<usize, Vec<usize>> =
                std::collections::HashMap::new();
            if sign_width != 0 {
                let first_row = state.topline.saturating_sub(1);
                let last_row = first_row.saturating_add(text_height.saturating_sub(1));
                for (index, mark) in marks.iter().enumerate().rev() {
                    if mark.placement.attributes.sign_text.is_none() {
                        continue;
                    }
                    let start = mark.position().row;
                    let end = mark.placement.end.map_or(start, |end| end.position.row);
                    for row in start.max(first_row)..=end.min(last_row) {
                        sign_marks_by_row.entry(row).or_default().push(index);
                    }
                }
            }
            let mut screen_row = 0;
            let mut line_number = state.topline;
            let mut watched_extmarks = Vec::new();
            while screen_row < text_height {
                if is_terminal && line_number > buffer.line_count() {
                    screen_row += 1;
                    line_number += 1;
                    continue;
                }
                if line_number > buffer.line_count() {
                    grid.put(screen_row, 0, "~", non_text_id, 1)?;
                    // Neovim's screen:expect matches the NonText attribute
                    // (`{1:…}`) spanning the entire fill line, not just the
                    // `~` glyph. Fill the remaining columns with the same
                    // highlight so the row compares equal to the upstream grid.
                    if text_width > 1 {
                        grid.set_hl_span(screen_row, 1, text_width, non_text_id)?;
                    }
                    screen_row += 1;
                    line_number += 1;
                    continue;
                }
                let bytes = buffer.line(line_number)?;
                let line_text = String::from_utf8_lossy(&bytes);
                let wrapped = wrapped_segments(&line_text, text_width);
                let line_start_row = screen_row;
                let available_rows = text_height.saturating_sub(screen_row);
                let truncated = wrapped.len() > available_rows;
                for (segment, segment_cell_start) in wrapped.iter().take(text_height - screen_row) {
                    if sign_width != 0 {
                        grid.set_hl_span(screen_row, 0, sign_width, sign_id)?;
                        let binned = sign_marks_by_row
                            .get(&line_number.saturating_sub(1))
                            .into_iter()
                            .flatten()
                            .take(sign_slots);
                        for (slot, mark_index) in binned.enumerate() {
                            let attributes = &marks[*mark_index].placement.attributes;
                            let mut text = attributes
                                .sign_text
                                .as_deref()
                                .unwrap_or_default()
                                .chars()
                                .take(2)
                                .collect::<String>();
                            text.extend(std::iter::repeat_n(
                                ' ',
                                2usize.saturating_sub(UnicodeWidthStr::width(text.as_str())),
                            ));
                            let hl_id = attributes
                                .sign_highlight_group
                                .as_deref()
                                .and_then(|name| highlights.group_id(&OxStr::from(name)))
                                .unwrap_or(sign_id);
                            grid.write_text(screen_row, slot * 2, &text, hl_id)?;
                        }
                    }
                    grid.write_text(screen_row, sign_width, segment, 0)?;
                    apply_extmark_highlights(
                        &mut grid,
                        screen_row,
                        line_number.saturating_sub(1),
                        sign_width,
                        *segment_cell_start,
                        &line_text,
                        &marks,
                        highlights,
                    )?;
                    screen_row += 1;
                }
                if truncated && screen_row != 0 {
                    grid.write_text(
                        screen_row - 1,
                        geometry.width.saturating_sub(3),
                        "@@@",
                        non_text_id,
                    )?;
                }
                for mark in marks.iter().filter(|mark| {
                    mark.placement
                        .attributes
                        .flags
                        .contains(ox_editor::ExtmarkFlags::UI_WATCHED)
                        && mark.position().row == line_number.saturating_sub(1)
                }) {
                    let draw_col = if matches!(
                        mark.placement.attributes.virtual_text_position,
                        ox_editor::extmark::ExtmarkVirtualTextPosition::Overlay
                    ) {
                        display_column(&line_text, mark.position().column)
                    } else {
                        UnicodeWidthStr::width(line_text.as_ref()).saturating_add(1)
                    };
                    let row = line_start_row.saturating_add(draw_col / text_width);
                    if row < text_height {
                        watched_extmarks.push(WatchedExtmark {
                            namespace: mark.namespace.get(),
                            mark: mark.id.get(),
                            row,
                            col: sign_width.saturating_add(draw_col % text_width),
                            buffer_row: mark.position().row,
                        });
                    }
                }
                line_number += 1;
            }
            layer.grid = grid;
            layer.window = Some(window);
            layer.watched_extmarks = watched_extmarks;
            if !is_float && tiled_split {
                let name = if buffer_state.name().as_bytes().is_empty() {
                    "[No Name]".to_owned()
                } else {
                    String::from_utf8_lossy(buffer_state.name().as_bytes()).into_owned()
                };
                let modified = if buffer_state
                    .flags
                    .contains(ox_editor::BufferFlags::MODIFIED)
                {
                    " [+]"
                } else {
                    ""
                };
                let mut statusline = format!("{name}{modified}");
                statusline.extend(std::iter::repeat_n(
                    ' ',
                    geometry.width.saturating_sub(statusline.len()),
                ));
                let hl_id = if current_window == Some(window) {
                    statusline_id
                } else {
                    statusline_nc_id
                };
                layer.statusline = Some((statusline, hl_id));
            }
            layer.cursor = (current_window == Some(window)).then(|| {
                let before_cursor = (state.topline..state.cursor.lnum)
                    .filter_map(|lnum| buffer.line(lnum).ok())
                    .map(|bytes| {
                        let line_text = String::from_utf8_lossy(&bytes);
                        wrapped_segments(&line_text, text_width).len()
                    })
                    .sum::<usize>();
                let cursor_line = buffer.line(state.cursor.lnum).unwrap_or_default();
                let cursor_line = String::from_utf8_lossy(&cursor_line);
                let cursor_col = display_column(&cursor_line, state.cursor.col);
                (
                    before_cursor.saturating_add(cursor_col / text_width),
                    sign_width.saturating_add(cursor_col % text_width),
                )
            });
            self.layers.push(layer);
        }
        let mut message = match take_layer(&mut retired, 3) {
            Some(layer) => layer,
            None => Layer::new(
                Grid::new(3, width, 1)?,
                0,
                0,
                MESSAGE_ZINDEX,
                LayerKind::Message,
            ),
        };
        message.grid.reshape(width, 1)?;
        message.reset(
            isize::try_from(height.saturating_sub(1)).unwrap_or(isize::MAX),
            0,
            MESSAGE_ZINDEX,
            LayerKind::Message,
        );
        self.layers.push(message);
        Ok(())
    }

    /// Flattens layers into `output` in stable z-order and resolves the topmost
    /// visible cursor. `output` is reshaped to this compositor's dimensions and
    /// fully rewritten; its id is preserved.
    ///
    /// # Errors
    /// Grid errors from reshaping or writing `output`, and highlight errors
    /// from blending `winblend` layers.
    pub fn compose_into(
        &self,
        output: &mut Grid,
        highlights: &mut HlState,
    ) -> Result<ComposeOutcome, CompositorError> {
        self.compose_into_with_policy(output, highlights, MessageLayers::Include)
    }

    /// [`Self::compose_into`] excluding the built-in message grid.
    ///
    /// # Errors
    /// As [`Self::compose_into`].
    pub fn compose_into_without_messages(
        &self,
        output: &mut Grid,
        highlights: &mut HlState,
    ) -> Result<ComposeOutcome, CompositorError> {
        self.compose_into_with_policy(output, highlights, MessageLayers::Exclude)
    }

    fn compose_into_with_policy(
        &self,
        output: &mut Grid,
        highlights: &mut HlState,
        messages: MessageLayers,
    ) -> Result<ComposeOutcome, CompositorError> {
        output.reshape(self.width, self.height)?;
        let mut order: Vec<usize> = (0..self.layers.len())
            .filter(|&index| {
                matches!(messages, MessageLayers::Include)
                    || self.layers[index].kind != LayerKind::Message
            })
            .collect();
        order.sort_by_key(|&index| {
            let layer = &self.layers[index];
            (
                layer.kind == LayerKind::Message,
                layer.zindex,
                layer.kind,
                index,
            )
        });
        let mut cursor = None;
        let mut highlight_events = Vec::new();
        // Painted before the layers: a 1-to-0 transition blanks row 0 so
        // the window layers repaint it in the same frame.
        self.paint_tabline_row(output)?;
        for index in order {
            let layer = &self.layers[index];
            for source_row in 0..layer.grid.height() {
                let Some(target_row) = source_row.checked_add_signed(layer.row) else {
                    continue;
                };
                if target_row >= self.height {
                    continue;
                }
                for source_col in 0..layer.grid.width() {
                    let Some(target_col) = source_col.checked_add_signed(layer.col) else {
                        continue;
                    };
                    if target_col >= self.width {
                        continue;
                    }
                    let source = layer.grid.cell(source_row, source_col)?;
                    if !layer.opaque && source.is_blank() {
                        continue;
                    }
                    let hl_id = if layer.winblend == 0 {
                        source.hl_id
                    } else {
                        // Copy the underlying hl out first so the shared borrow of
                        // `output` ends before `highlights` is borrowed mutably.
                        let beneath = output.hl_at(target_row, target_col)?;
                        let (id, event) =
                            highlights.premix(source.hl_id, beneath, layer.winblend)?;
                        if let Some(event) = event {
                            highlight_events.push(event);
                        }
                        id
                    };
                    output.write_cell(
                        target_row,
                        target_col,
                        source.text.as_bytes(),
                        hl_id,
                        source.width,
                    )?;
                }
            }
            if let Some((statusline, hl_id)) = &layer.statusline
                && let (Some(row), Some(col)) = (
                    layer.grid.height().checked_add_signed(layer.row),
                    0usize.checked_add_signed(layer.col),
                )
                && row < self.height
                && col < self.width
            {
                output.write_text(row, col, statusline, *hl_id)?;
            }
            if let Some((row, col)) = layer.cursor
                && let (Some(row), Some(col)) = (
                    row.checked_add_signed(layer.row),
                    col.checked_add_signed(layer.col),
                )
                && row < self.height
                && col < self.width
            {
                cursor = Some((row, col));
            }
        }
        Ok(ComposeOutcome {
            cursor,
            highlight_events,
        })
    }

    /// Paints the tabline runs onto the default grid's top row. A 1-to-0
    /// tabline transition blanks the row once so `emit_grid`'s diff clears
    /// the stale cells; a steady no-tabline frame leaves row 0 to the
    /// window layers, which own it whenever no tabline is shown.
    /// # Errors
    ///
    /// Returns [`CompositorError::Grid`] when the default grid write falls
    /// outside the composed area.
    pub fn paint_tabline_row(&self, output: &mut Grid) -> Result<(), CompositorError> {
        if self.width == 0 {
            return Ok(());
        }
        if self.tabline_row.is_empty() {
            if self.tabline_was_shown {
                output.write_text(0, 0, &" ".repeat(self.width), 0)?;
            }
            return Ok(());
        }
        for (col, text, hl_id) in &self.tabline_row {
            if *col < self.width {
                output.write_text(0, *col, text, *hl_id)?;
            }
        }
        Ok(())
    }

    /// Returns the grid id assigned to an editor window in a multigrid stream.
    #[must_use]
    pub fn window_grid(&self, window: WinHandle, editor: &Editor) -> Option<i64> {
        let tab = editor
            .current_tabpage()
            .and_then(|handle| editor.tabpage(handle).ok())?;
        tab.windows()
            .contains(&window)
            .then(|| window_grid_id(window))
    }
}

/// Even grid ids are reserved for window handles; odd ids stay with
/// synthetic grids (default=1, messages=3).
fn window_grid_id(window: WinHandle) -> i64 {
    let handle = i64::from(window);
    let ordinal = if handle >= 1000 { handle - 999 } else { handle };
    ordinal.saturating_mul(2)
}

/// Removes the retained layer for `id`, if one survived the previous frame.
fn take_layer(retired: &mut Vec<Layer>, id: i64) -> Option<Layer> {
    retired
        .iter()
        .position(|layer| layer.grid.id() == id)
        .map(|index| retired.swap_remove(index))
}

/// Resolves tabline cell runs to concrete highlight ids: `TabLine`,
/// `TabLineSel`, and `TabLineFill` resolve through their groups (defined
/// with the reference binary's defaults when absent), and the window-count
/// cell composes `Title` over the enclosing tab's attr
/// (`statusline.c:671`).
fn resolve_tabline_row(
    cells: &[crate::tabline::TablineCell],
    highlights: &mut HlState,
) -> Result<Vec<(usize, String, u64)>, CompositorError> {
    use crate::tabline::TablineHl;
    let mut tab_id = 0u64;
    let mut resolved = Vec::with_capacity(cells.len());
    for cell in cells {
        let name = cell.hl.group_name();
        let id = match highlights.group_id(&OxStr::from(name)) {
            Some(id) => id,
            None => highlights.define_group(name, cell.hl.default_highlight())?,
        };
        let id = match cell.hl {
            TablineHl::Tab | TablineHl::TabSel => {
                tab_id = id;
                id
            }
            TablineHl::Fill => id,
            TablineHl::Count => highlights.combine(tab_id, id)?.0,
        };
        resolved.push((cell.col, cell.text.clone(), id));
    }
    Ok(resolved)
}

/// Content rectangle for a tiled window: one statusline under each split
/// window, and the last screen row reserved for the message grid.
fn tiled_window_grid_geometry(
    geometry: Geometry,
    screen_height: usize,
    tiled_split: bool,
) -> (isize, usize) {
    let work_bottom = screen_height.saturating_sub(1);
    let statusline = usize::from(tiled_split);
    let frame_end = geometry.row.saturating_add(geometry.height);
    let content_end = frame_end.min(work_bottom).saturating_sub(statusline);
    let grid_height = content_end.saturating_sub(geometry.row).max(1);
    (
        isize::try_from(geometry.row).unwrap_or(isize::MAX),
        grid_height,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the coordinates and highlight state define one indivisible extmark render operation"
)]
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
        let Some(end) = mark.placement.end.map(|end| end.position) else {
            continue;
        };
        if buffer_row < start.row || buffer_row > end.row {
            continue;
        }

        let start_byte = if buffer_row == start.row {
            start.column
        } else {
            0
        };
        let end_byte = if buffer_row == end.row {
            end.column
        } else {
            line.len()
        };
        let absolute_start = display_column(line, start_byte);
        let absolute_end = display_column(line, end_byte);
        if absolute_start >= segment_cell_start.saturating_add(segment_width)
            || absolute_end <= segment_cell_start
        {
            continue;
        }
        let start_col =
            text_offset.saturating_add(absolute_start.saturating_sub(segment_cell_start));
        let mut end_col =
            text_offset.saturating_add(absolute_end.saturating_sub(segment_cell_start));
        if mark
            .placement
            .attributes
            .flags
            .contains(ox_editor::ExtmarkFlags::HIGHLIGHT_EOL)
            && buffer_row == end.row
        {
            end_col = grid.width();
        }

        let attributes = &mark.placement.attributes;
        let group_names = std::iter::once(attributes.highlight_group.as_deref()).chain(
            attributes
                .additional_highlight_groups
                .iter()
                .map(|name| Some(name.as_str())),
        );
        let mut mark_id = 0;
        for name in group_names.flatten() {
            let Some(group_id) = highlights.group_id(&OxStr::from(name)) else {
                continue;
            };
            mark_id = highlights.combine(mark_id, group_id)?.0;
        }
        if mark_id == 0
            && let Some(pen) = attributes.terminal_pen
        {
            mark_id = terminal_pen_group(highlights, pen)?;
        }
        if mark_id == 0 {
            continue;
        }
        for col in start_col.min(grid.width())..end_col.min(grid.width()) {
            let current = grid.hl_at(screen_row, col)?;
            let hl_id = match attributes.highlight_mode {
                Some(ExtmarkHighlightMode::Combine) => highlights.combine(current, mark_id)?.0,
                Some(ExtmarkHighlightMode::Blend) => highlights.blend(current, mark_id)?.0,
                None | Some(ExtmarkHighlightMode::Replace) => mark_id,
            };
            grid.set_hl(screen_row, col, hl_id)?;
        }
    }
    Ok(())
}
/// Resolves one terminal pen to a highlight group (`hl_get_term_attr`,
/// `terminal.c:1432-1442`): the SGR state becomes the group definition,
/// cached under a stable name so repeated rows share one group.
fn terminal_pen_group(
    highlights: &mut HlState,
    pen: ox_editor::terminal_screen::CellAttrs,
) -> Result<u64, CompositorError> {
    let name = format!(
        "TermPen{}{}{:04x}",
        terminal_pen_color(pen.foreground),
        terminal_pen_color(pen.background),
        pen.flags.without(CellFlags::WIDE).bits(),
    );
    if let Some(id) = highlights.group_id(&OxStr::from(name.as_str())) {
        return Ok(id);
    }
    let (fg_rgb, fg_index) = terminal_pen_channel(pen.foreground);
    let (bg_rgb, bg_index) = terminal_pen_channel(pen.background);
    let flags = pen.flags;
    let rgb = HlAttrs {
        foreground: fg_rgb,
        background: bg_rgb,
        fg_indexed: fg_index.is_some(),
        bg_indexed: bg_index.is_some(),
        bold: flags.contains(CellFlags::BOLD),
        italic: flags.contains(CellFlags::ITALIC),
        underline: flags.contains(CellFlags::UNDERLINE),
        undercurl: flags.contains(CellFlags::UNDERCURL),
        underdouble: flags.contains(CellFlags::UNDERDOUBLE),
        reverse: flags.contains(CellFlags::REVERSE),
        strikethrough: flags.contains(CellFlags::STRIKETHROUGH),
        blink: flags.contains(CellFlags::BLINK),
        dim: flags.contains(CellFlags::FAINT),
        ..HlAttrs::default()
    };
    let indexed = fg_index.is_some() || bg_index.is_some();
    let cterm = HlAttrs {
        foreground: fg_index,
        background: bg_index,
        fg_indexed: indexed,
        bg_indexed: indexed,
        bold: rgb.bold,
        italic: rgb.italic,
        underline: rgb.underline,
        undercurl: rgb.undercurl,
        underdouble: rgb.underdouble,
        reverse: rgb.reverse,
        strikethrough: rgb.strikethrough,
        blink: rgb.blink,
        dim: rgb.dim,
        ..HlAttrs::default()
    };
    let id = highlights.define_group(
        OxStr::from(name.as_str()),
        Highlight {
            rgb,
            cterm,
            cterm_explicit: indexed,
            default_flag: false,
            info: Vec::new(),
        },
    )?;
    Ok(id)
}

/// Encodes one pen channel for the cache name.
fn terminal_pen_color(color: TermColor) -> String {
    match color {
        TermColor::Default => "d".to_owned(),
        TermColor::Indexed(index) => format!("i{index:02x}"),
        TermColor::Rgb(red, green, blue) => format!("r{red:02x}{green:02x}{blue:02x}"),
    }
}

/// Splits one pen channel into its RGB and indexed representations.
fn terminal_pen_channel(color: TermColor) -> (Option<u32>, Option<u32>) {
    match color {
        TermColor::Default => (None, None),
        TermColor::Indexed(index) => (Some(u32::from(index)), Some(u32::from(index))),
        TermColor::Rgb(red, green, blue) => (
            Some((u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)),
            None,
        ),
    }
}

fn wrapped_segments(line: &str, width: usize) -> Vec<(&str, usize)> {
    let mut segments = Vec::new();
    let mut segment_start = 0usize;
    let mut segment_width = 0usize;
    let mut cell_start = 0usize;
    for (byte_offset, character) in line.char_indices() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0).max(1);
        if segment_width != 0 && segment_width.saturating_add(character_width) > width {
            segments.push((&line[segment_start..byte_offset], cell_start));
            cell_start = cell_start.saturating_add(segment_width);
            segment_width = 0;
            segment_start = byte_offset;
        }
        segment_width = segment_width.saturating_add(character_width);
    }
    segments.push((&line[segment_start..], cell_start));
    segments
}

fn display_column(line: &str, byte: usize) -> usize {
    let mut boundary = byte.min(line.len());
    while !line.is_char_boundary(boundary) {
        boundary -= 1;
    }
    UnicodeWidthStr::width(&line[..boundary])
}
