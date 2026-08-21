//! Bridge from compositor and chrome state into per-capability redraw frames.

use std::collections::{BTreeMap, BTreeSet};

use ox_types::{Object, OxStr};
use thiserror::Error;

use crate::channel::{UiChannelError, UiChannels, UiEvent, UiOptions};
use crate::chrome::{ChromeState, ContentChunk};
use crate::compositor::{Compositor, CompositorError, Layer, LayerKind};
use crate::grid::{Grid, GridError, GridLine};
use crate::hl::{Highlight, HlEvent, HlState};

/// Emitter failures.
#[derive(Debug, Error)]
pub enum EmitterError {
    /// Composition failed.
    #[error(transparent)]
    Compositor(#[from] CompositorError),
    /// Channel batching failed.
    #[error(transparent)]
    Channel(#[from] UiChannelError),
    /// Grid construction failed.
    #[error(transparent)]
    Grid(#[from] GridError),
}

/// Stateful redraw bridge retaining the last grid sent to each channel.
#[derive(Clone, Debug, Default)]
pub struct Emitter {
    previous: BTreeMap<(u64, i64), Grid>,
    sent_highlights: BTreeMap<(u64, u64), Highlight>,
    sent_groups: BTreeMap<(u64, OxStr), u64>,
    initialized: BTreeSet<u64>,
}

impl Emitter {
    /// Creates an emitter with no per-channel history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            previous: BTreeMap::new(),
            sent_highlights: BTreeMap::new(),
            sent_groups: BTreeMap::new(),
            initialized: BTreeSet::new(),
        }
    }

    /// Drops retained grids for a detached channel.
    pub fn detach(&mut self, channel_id: u64) {
        self.previous.retain(|(id, _), _| *id != channel_id);
        self.sent_highlights.retain(|(id, _), _| *id != channel_id);
        self.sent_groups.retain(|(id, _), _| *id != channel_id);
        self.initialized.remove(&channel_id);
    }

    /// Emits one complete redraw transaction for every attached channel.
    pub fn redraw(
        &mut self,
        channels: &mut UiChannels,
        compositor: &Compositor,
        highlights: &mut HlState,
        chrome: &mut ChromeState,
    ) -> Result<BTreeMap<u64, Vec<u8>>, EmitterError> {
        if channels.is_empty() { return Ok(BTreeMap::new()); }
        let chrome_events = chrome.take_events();
        let mut initial_chrome_events = chrome.snapshot_events();
        for event in &chrome_events {
            if !initial_chrome_events.contains(event) { initial_chrome_events.push(event.clone()); }
        }
        let mut frames = BTreeMap::new();
        for (&channel_id, channel) in channels.iter_mut() {
            let first_redraw = self.initialized.insert(channel_id);
            channel.begin();
            let options = channel.options();
            if options.ext_multigrid {
                self.emit_highlights(channel_id, channel, highlights, options)?;
                let (width, height) = channel.size();
                let mut default_grid = Grid::new(1, width, height)?;
                if !options.ext_cmdline { apply_cmdline_fallback(&mut default_grid, chrome)?; }
                self.emit_grid(channel_id, channel, &default_grid)?;
                let float_compindex = float_compindexes(compositor);
                for (index, layer) in compositor.layers().iter().enumerate() {
                    if options.ext_messages && layer.kind == LayerKind::Message { continue; }
                    emit_position(channel, layer, float_compindex.get(&index).copied())?;
                    self.emit_grid(channel_id, channel, &layer.grid)?;
                    if let Some((row, col)) = layer.cursor {
                        channel.emit(UiEvent::new("grid_cursor_goto", vec![
                            Object::Integer(layer.grid.id()),
                            integer(row),
                            integer(col),
                        ]))?;
                    }
                }
            } else {
                let mut composed = if options.ext_messages {
                    compositor.compose_without_messages(highlights)?
                } else {
                    compositor.compose(highlights)?
                };
                self.emit_highlights(channel_id, channel, highlights, options)?;
                if !options.ext_messages { apply_message_fallback(&mut composed.grid, chrome)?; }
                if !options.ext_cmdline { apply_cmdline_fallback(&mut composed.grid, chrome)?; }
                self.emit_grid(channel_id, channel, &composed.grid)?;
                if let Some((row, col)) = composed.cursor {
                    channel.emit(UiEvent::new("grid_cursor_goto", vec![
                        Object::Integer(1),
                        integer(row),
                        integer(col),
                    ]))?;
                }
            }
            let routed_chrome = if first_redraw { &initial_chrome_events } else { &chrome_events };
            route_chrome(channel, options, routed_chrome)?;
            frames.insert(channel_id, channel.flush()?);
        }
        Ok(frames)
    }

    /// Emits a grid resize plus either a full initial image or minimal line diffs.
    pub fn emit_grid(
        &mut self,
        channel_id: u64,
        channel: &mut crate::channel::UiChannel,
        grid: &Grid,
    ) -> Result<(), UiChannelError> {
        let key = (channel_id, grid.id());
        let lines = if let Some(previous) = self.previous.get(&key) {
            if previous.width() == grid.width() && previous.height() == grid.height() {
                grid.diff(previous)
            } else {
                channel.emit(UiEvent::new("grid_resize", vec![
                    Object::Integer(grid.id()),
                    integer(grid.width()),
                    integer(grid.height()),
                ]))?;
                grid.full_lines()
            }
        } else {
            channel.emit(UiEvent::new("grid_resize", vec![
                Object::Integer(grid.id()),
                integer(grid.width()),
                integer(grid.height()),
            ]))?;
            grid.full_lines()
        };
        for line in lines { channel.emit(grid_line_event(grid.id(), line))?; }
        self.previous.insert(key, grid.clone());
        Ok(())
    }

    fn emit_highlights(
        &mut self,
        channel_id: u64,
        channel: &mut crate::channel::UiChannel,
        highlights: &HlState,
        options: UiOptions,
    ) -> Result<(), UiChannelError> {
        let definitions = highlights.definitions();
        for ((id, highlight), event) in highlights.iter().zip(definitions) {
            let key = (channel_id, id);
            if self.sent_highlights.get(&key) != Some(highlight) {
                emit_highlight(channel, event, options)?;
                self.sent_highlights.insert(key, highlight.clone());
            }
        }
        for (name, id) in highlights.groups() {
            let key = (channel_id, name.clone());
            if self.sent_groups.get(&key) != Some(&id) {
                channel.emit(UiEvent::new(
                    "hl_group_set",
                    vec![Object::String(name.clone()), Object::Integer(i64::try_from(id).unwrap_or(i64::MAX))],
                ))?;
                self.sent_groups.insert(key, id);
            }
        }
        Ok(())
    }
}

fn emit_position(
    channel: &mut crate::channel::UiChannel,
    layer: &Layer,
    compindex: Option<usize>,
) -> Result<(), UiChannelError> {
    let window = layer.window.map_or(Object::Nil, Object::Window);
    match layer.kind {
        LayerKind::Window => channel.emit(UiEvent::new("win_pos", vec![
            Object::Integer(layer.grid.id()),
            window,
            signed(layer.row),
            signed(layer.col),
            integer(layer.grid.width()),
            integer(layer.grid.height()),
        ])),
        LayerKind::Float => channel.emit(UiEvent::new("win_float_pos", vec![
            Object::Integer(layer.grid.id()),
            window,
            Object::String(OxStr::from("NW")),
            Object::Integer(1),
            Object::Float(layer.row as f64),
            Object::Float(layer.col as f64),
            Object::Boolean(false),
            Object::Integer(i64::from(layer.zindex)),
            Object::Integer(i64::try_from(compindex.unwrap_or(0)).unwrap_or(i64::MAX)),
            signed(layer.row),
            signed(layer.col),
        ])),
        LayerKind::Message => channel.emit(UiEvent::new("msg_set_pos", vec![
            Object::Integer(layer.grid.id()),
            signed(layer.row),
            Object::Boolean(false),
            Object::String(OxStr::from(" ")),
            Object::Integer(i64::from(crate::compositor::MESSAGE_ZINDEX)),
            Object::Integer(0),
        ])),
    }
}

/// Computes the 1-based compositor index of every float layer, in stacking
/// order. Mirrors Neovim's `comp_index`: the position of a float grid among
/// the layered grids above the default grid, not the UI channel id. Two floats
/// with equal z-index keep distinct, stable indexes in insertion order.
fn float_compindexes(compositor: &Compositor) -> BTreeMap<usize, usize> {
    let mut floats: Vec<(u32, usize)> = compositor.layers().iter().enumerate()
        .filter(|(_, layer)| layer.kind == LayerKind::Float)
        .map(|(index, layer)| (layer.zindex, index))
        .collect();
    floats.sort();
    floats.into_iter().enumerate().map(|(rank, (_, index))| (index, rank + 1)).collect()
}

fn route_chrome(
    channel: &mut crate::channel::UiChannel,
    options: UiOptions,
    events: &[UiEvent],
) -> Result<(), UiChannelError> {
    for event in events {
        let name = event.name.to_string_lossy();
        let supported = if name.starts_with("msg_") {
            options.ext_messages
        } else if name.starts_with("cmdline_") {
            options.ext_cmdline
        } else if name.starts_with("popupmenu_") {
            options.ext_popupmenu
        } else {
            true
        };
        if supported { channel.emit(event.clone())?; }
    }
    Ok(())
}

fn apply_message_fallback(grid: &mut Grid, chrome: &ChromeState) -> Result<(), GridError> {
    let Some(message) = &chrome.message else { return Ok(()) };
    write_chunks(grid, &message.content)
}

fn apply_cmdline_fallback(grid: &mut Grid, chrome: &ChromeState) -> Result<(), GridError> {
    let Some(cmdline) = &chrome.cmdline else { return Ok(()) };
    write_chunks(grid, &cmdline.content)
}

fn write_chunks(grid: &mut Grid, chunks: &[ContentChunk]) -> Result<(), GridError> {
    if grid.height() == 0 { return Ok(()); }
    let row = grid.height() - 1;
    let mut col = 0;
    for chunk in chunks {
        if col >= grid.width() { break; }
        let text = chunk.text.to_string_lossy();
        col = grid.write_text(row, col, &text, chunk.hl_id)?;
    }
    Ok(())
}

fn emit_highlight(
    channel: &mut crate::channel::UiChannel,
    event: HlEvent,
    options: UiOptions,
) -> Result<(), UiChannelError> {
    let mut args = event.args;
    if !options.ext_hlstate && event.name == "hl_attr_define" && args.len() == 4 {
        args[3] = Object::Array(Vec::new());
    }
    channel.emit(UiEvent::new(event.name, args))
}

fn grid_line_event(grid: i64, line: GridLine) -> UiEvent {
    UiEvent::new("grid_line", vec![
        Object::Integer(grid),
        integer(line.row),
        integer(line.start_col),
        Object::Array(line.cells),
        Object::Boolean(line.wrap),
    ])
}

fn integer(value: usize) -> Object { Object::Integer(i64::try_from(value).unwrap_or(i64::MAX)) }
fn signed(value: isize) -> Object { Object::Integer(i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })) }
