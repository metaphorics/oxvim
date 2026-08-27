//! Server-side grid layering modeled after Neovim's UI compositor.

use ox_editor::{
    extmark::ExtmarkHighlightMode, BufferStateError, Editor, EditorError, Extmark, Geometry,
    LayoutError,
};
use ox_text::BufferError;
use ox_types::{OxStr, WinHandle};
use thiserror::Error;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::grid::{Cell, Grid, GridError};
use crate::hl::{Highlight, HlAttrs, HlError, HlEvent, HlState};

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
        Self { grid, window: None, row, col, zindex, winblend: 0, kind, opaque: true, cursor: None, watched_extmarks: Vec::new(), statusline: None }
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
    pub fn from_editor(editor: &Editor, width: usize, height: usize, highlights: &mut HlState) -> Result<Self, CompositorError> {
        Self::from_editor_with_namespaces(editor, width, height, highlights, |namespace| namespace)
    }

    /// Builds renderable layers and translates extmark namespaces for UI events.
    pub fn from_editor_with_namespaces(
        editor: &Editor,
        width: usize,
        height: usize,
        highlights: &mut HlState,
        public_namespace: impl Fn(u32) -> u32,
    ) -> Result<Self, CompositorError> {
        let tab_handle = editor.current_tabpage().ok_or(CompositorError::NoActiveTabpage)?;
        let tab = editor.tabpage(tab_handle)?;
        let current_window = editor.current_window();
        let non_text = Highlight {
            rgb: HlAttrs { foreground: Some(0x0000ff), bold: true, ..HlAttrs::default() },
            cterm: HlAttrs { foreground: Some(12), bold: true, fg_indexed: true, ..HlAttrs::default() },
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
                        foreground: Some(0x00008b),
                        background: Some(0x808080),
                        ..HlAttrs::default()
                    },
                    ..Highlight::default()
                },
            )?,
        };
        let (statusline_id, _) = highlights.intern(Highlight {
            rgb: HlAttrs { bold: true, reverse: true, ..HlAttrs::default() },
            cterm: HlAttrs { bold: true, reverse: true, ..HlAttrs::default() },
            cterm_explicit: true,
            ..Highlight::default()
        })?;
        let (statusline_nc_id, _) = highlights.intern(Highlight {
            rgb: HlAttrs { reverse: true, ..HlAttrs::default() },
            cterm: HlAttrs { reverse: true, ..HlAttrs::default() },
            cterm_explicit: true,
            ..Highlight::default()
        })?;
        let mut compositor = Self::new(width, height);
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
                    isize::try_from(geometry.row).unwrap_or(isize::MAX),
                    geometry.height.max(1),
                )
            } else {
                tiled_window_grid_geometry(geometry, height, tiled_split)
            };
            let mut grid = Grid::new(grid_id, geometry.width, grid_height)?;
            let buffer_state = editor.buffer(state.buffer)?;
            let buffer = buffer_state.text()?;
            let marks = buffer_state.extmarks.render_ordered();
            let sign_slots = marks
                .iter()
                .filter(|mark| mark.placement.attributes.sign_text.is_some())
                .fold(vec![0usize; buffer.line_count()], |mut rows, mark| {
                    let start = mark.position().row.min(rows.len());
                    let end = mark
                        .placement
                        .end
                        .map_or(start, |end| end.position.row.min(rows.len().saturating_sub(1)));
                    if start < rows.len() {
                        for count in &mut rows[start..=end.max(start)] {
                            *count = count.saturating_add(1);
                        }
                    }
                    rows
                })
                .into_iter()
                .max()
                .unwrap_or(0)
                .min(3);
            let sign_width = sign_slots.saturating_mul(2);
            let text_height = grid_height;
            let text_width = geometry.width.saturating_sub(sign_width).max(1);
            let mut screen_row = 0;
            let mut line_number = state.topline;
            let mut watched_extmarks = Vec::new();
            while screen_row < text_height {
                if line_number > buffer.line_count() {
                    let mut filler = String::with_capacity(geometry.width);
                    filler.push('~');
                    filler.extend(std::iter::repeat_n(' ', geometry.width.saturating_sub(1)));
                    grid.write_text(screen_row, 0, &filler, non_text_id)?;
                    screen_row += 1;
                    line_number += 1;
                    continue;
                }
                let bytes = buffer.line(line_number)?;
                let line = String::from_utf8_lossy(&bytes);
                let wrapped = wrapped_segments(&line, text_width);
                let line_start_row = screen_row;
                let available_rows = text_height.saturating_sub(screen_row);
                let truncated = wrapped.len() > available_rows;
                for (segment, segment_cell_start) in wrapped.iter().take(text_height - screen_row) {
                    if sign_width != 0 {
                        grid.write_text(screen_row, 0, &" ".repeat(sign_width), sign_id)?;
                        for (slot, mark) in marks
                            .iter()
                            .rev()
                            .filter(|mark| {
                                let start = mark.position().row;
                                let end = mark.placement.end.map_or(start, |end| end.position.row);
                                start <= line_number.saturating_sub(1)
                                    && line_number.saturating_sub(1) <= end
                                    && mark.placement.attributes.sign_text.is_some()
                            })
                            .take(sign_slots)
                            .enumerate()
                        {
                            let attributes = &mark.placement.attributes;
                            let mut text = attributes.sign_text.as_deref().unwrap_or_default().chars().take(2).collect::<String>();
                            text.extend(std::iter::repeat_n(' ', 2usize.saturating_sub(UnicodeWidthStr::width(text.as_str()))));
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
                        &line,
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
                    mark.placement.attributes.ui_watched
                        && mark.position().row == line_number.saturating_sub(1)
                }) {
                    let draw_col = if matches!(
                        mark.placement.attributes.virtual_text_position,
                        ox_editor::extmark::ExtmarkVirtualTextPosition::Overlay
                    ) {
                        display_column(&line, mark.position().column)
                    } else {
                        UnicodeWidthStr::width(line.as_ref()).saturating_add(1)
                    };
                    let row = line_start_row.saturating_add(draw_col / text_width);
                    if row < text_height {
                        watched_extmarks.push(WatchedExtmark {
                            namespace: public_namespace(mark.namespace.get()),
                            mark: mark.id.get(),
                            row,
                            col: sign_width.saturating_add(draw_col % text_width),
                            buffer_row: mark.position().row,
                        });
                    }
                }
                line_number += 1;
            }
            let kind = if is_float { LayerKind::Float } else { LayerKind::Window };
            let zindex = config.map_or(0, |config| config.zindex);
            let mut layer = Layer::new(
                grid,
                layer_row,
                isize::try_from(geometry.col).unwrap_or(isize::MAX),
                zindex,
                kind,
            );
            layer.window = Some(window);
            layer.watched_extmarks = watched_extmarks;
            if !is_float && tiled_split {
                let name = if buffer_state.name().as_bytes().is_empty() {
                    "[No Name]".to_owned()
                } else {
                    String::from_utf8_lossy(buffer_state.name().as_bytes()).into_owned()
                };
                let modified = if buffer_state.modified { " [+]" } else { "" };
                let mut statusline = format!("{name}{modified}");
                statusline.extend(std::iter::repeat_n(' ', geometry.width.saturating_sub(statusline.len())));
                let hl_id = if current_window == Some(window) { statusline_id } else { statusline_nc_id };
                layer.statusline = Some((statusline, hl_id));
            }
            layer.cursor = (current_window == Some(window)).then(|| {
                let before_cursor = (state.topline..state.cursor.lnum)
                    .filter_map(|lnum| buffer.line(lnum).ok())
                    .map(|bytes| {
                        let line = String::from_utf8_lossy(&bytes);
                        wrapped_segments(&line, text_width).len()
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
            compositor.push_layer(layer);
        }
        let message_row = height.saturating_sub(1);
        let message = Grid::new(3, width, 1)?;
        compositor.push_layer(Layer::new(
            message,
            isize::try_from(message_row).unwrap_or(isize::MAX),
            0,
            MESSAGE_ZINDEX,
            LayerKind::Message,
        ));
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
        tab.windows().iter().any(|candidate| *candidate == window).then(|| window_grid_id(window))
    }
}

/// Even grid ids are reserved for window handles; odd ids stay with
/// synthetic grids (default=1, messages=3).
fn window_grid_id(window: WinHandle) -> i64 {
    let handle = i64::from(window);
    let ordinal = if handle >= 1000 { handle - 999 } else { handle };
    ordinal.saturating_mul(2)
}

/// Content rectangle for a tiled window: one statusline under each split
/// window, and the last screen row reserved for the message grid.
fn tiled_window_grid_geometry(geometry: Geometry, screen_height: usize, tiled_split: bool) -> (isize, usize) {
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
        let mut end_col = text_offset.saturating_add(absolute_end.saturating_sub(segment_cell_start));
        if mark.placement.attributes.highlight_eol && buffer_row == end.row {
            end_col = grid.width();
        }

        let attributes = &mark.placement.attributes;
        let group_names = std::iter::once(attributes.highlight_group.as_deref())
            .chain(attributes.additional_highlight_groups.iter().map(|name| Some(name.as_str())));
        let mut mark_id = 0;
        for name in group_names.flatten() {
            let Some(group_id) = highlights.group_id(&OxStr::from(name)) else { continue };
            mark_id = highlights.combine(mark_id, group_id)?.0;
        }
        if mark_id == 0 { continue; }
        for col in start_col.min(grid.width())..end_col.min(grid.width()) {
            let mut cell = grid.cell(screen_row, col)?.clone();
            cell.hl_id = match attributes.highlight_mode {
                Some(ExtmarkHighlightMode::Combine) => highlights.combine(cell.hl_id, mark_id)?.0,
                Some(ExtmarkHighlightMode::Blend) => highlights.blend(cell.hl_id, mark_id)?.0,
                None | Some(ExtmarkHighlightMode::Replace) => mark_id,
            };
            grid.set_cell(screen_row, col, cell)?;
        }
    }
    Ok(())
}

fn wrapped_segments(line: &str, width: usize) -> Vec<(String, usize)> {
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut segment_width = 0usize;
    let mut cell_start = 0usize;
    for character in line.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0).max(1);
        if segment_width != 0 && segment_width.saturating_add(character_width) > width {
            segments.push((std::mem::take(&mut segment), cell_start));
            cell_start = cell_start.saturating_add(segment_width);
            segment_width = 0;
        }
        segment.push(character);
        segment_width = segment_width.saturating_add(character_width);
    }
    segments.push((segment, cell_start));
    segments
}

fn display_column(line: &str, byte: usize) -> usize {
    let mut boundary = byte.min(line.len());
    while !line.is_char_boundary(boundary) {
        boundary -= 1;
    }
    UnicodeWidthStr::width(&line[..boundary])
}
