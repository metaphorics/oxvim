//! Bridge from compositor and chrome state into per-capability redraw frames.

use std::collections::{BTreeMap, BTreeSet};

use ox_types::{Object, OxStr};
use thiserror::Error;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::channel::{UiChannelError, UiChannels, UiEvent, UiOptions};
use crate::chrome::{ChromeState, ContentChunk};
use crate::compositor::{Compositor, CompositorError, Layer, LayerKind, WatchedExtmark};
use crate::grid::{Grid, GridError, GridLine};
use crate::hl::{Highlight, HlAttrs, HlError, HlEvent, HlInfo, HlState};

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
    /// Highlight interning failed.
    #[error(transparent)]
    Hl(#[from] HlError),
    /// A float position coordinate is outside the i32 screen coordinate range.
    #[error("grid coordinate {0} is outside the i32 screen coordinate range")]
    Position(isize),
}

/// One redraw pass: encoded frames per UI channel plus the semantic event
/// stream (for `vim.ui_attach` callbacks).
pub struct RedrawOutput(pub BTreeMap<u64, Vec<u8>>, pub Vec<UiEvent>);

impl RedrawOutput {
    fn default_empty(events: Vec<UiEvent>) -> Self {
        Self(BTreeMap::new(), events)
    }
}

/// Stateful redraw bridge retaining the last grid sent to each channel.
#[derive(Clone, Debug, Default)]
pub struct Emitter {
    previous: BTreeMap<(u64, i64), Grid>,
    /// Per-channel default-grid (id 1) buffer, retained across redraws so the
    /// steady state never materializes a fresh screen. Ownership moves to the
    /// caller for the duration of one redraw so `emit_grid` can borrow `self`
    /// mutably.
    scratch: BTreeMap<u64, Grid>,
    sent_highlights: BTreeMap<(u64, u64), Highlight>,
    sent_groups: BTreeMap<(u64, OxStr), u64>,
    watched_extmarks: BTreeMap<(u64, i64), Vec<WatchedExtmark>>,
    initialized: BTreeSet<u64>,
}

impl Emitter {
    /// Creates an emitter with no per-channel history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            previous: BTreeMap::new(),
            scratch: BTreeMap::new(),
            sent_highlights: BTreeMap::new(),
            sent_groups: BTreeMap::new(),
            watched_extmarks: BTreeMap::new(),
            initialized: BTreeSet::new(),
        }
    }

    /// Drops retained grids for a detached channel.
    pub fn detach(&mut self, channel_id: u64) {
        self.previous.retain(|(id, _), _| *id != channel_id);
        self.sent_highlights.retain(|(id, _), _| *id != channel_id);
        self.sent_groups.retain(|(id, _), _| *id != channel_id);
        self.watched_extmarks.retain(|(id, _), _| *id != channel_id);
        self.initialized.remove(&channel_id);
        self.scratch.remove(&channel_id);
    }

    /// Takes this channel's retained default grid, unshaped. The consumer
    /// reshapes it: the multigrid branch to the channel size, `compose_into` to
    /// the compositor size.
    fn take_scratch(&mut self, channel_id: u64) -> Result<Grid, GridError> {
        match self.scratch.remove(&channel_id) {
            Some(grid) => Ok(grid),
            None => Grid::new(1, 0, 0),
        }
    }

    fn restore_scratch(&mut self, channel_id: u64, grid: Grid) {
        self.scratch.insert(channel_id, grid);
    }

    /// Emits one complete redraw transaction for every attached channel.
    ///
    /// # Errors
    ///
    /// Returns [`EmitterError::Compositor`] if composition fails,
    /// [`EmitterError::Grid`] if a composed grid cannot be built,
    /// [`EmitterError::Position`] if a float layer position falls outside the i32
    /// coordinate range, or [`EmitterError::Channel`] if a channel cannot emit or
    /// flush its redraw batch.
    #[expect(
        clippy::too_many_lines,
        reason = "redraw event ordering is an indivisible protocol transaction"
    )]
    pub fn redraw(
        &mut self,
        channels: &mut UiChannels,
        compositor: &Compositor,
        highlights: &mut HlState,
        chrome: &mut ChromeState,
    ) -> Result<RedrawOutput, EmitterError> {
        if channels.is_empty() {
            // No RPC UI attached; semantic events still surface to Lua
            // callbacks (vim.ui_attach works without a remote UI).
            return Ok(RedrawOutput::default_empty(chrome.take_events()));
        }
        let chrome_events = chrome.take_events();
        let mut initial_chrome_events = chrome.snapshot_events();
        for event in &chrome_events {
            if !initial_chrome_events.contains(event) {
                initial_chrome_events.push(event.clone());
            }
        }
        let mut frames = BTreeMap::new();
        for (&channel_id, channel) in channels.iter_mut() {
            let first_redraw = self.initialized.insert(channel_id);
            channel.begin();
            let options = channel.options();
            ensure_chrome_highlights(highlights)?;
            if first_redraw {
                emit_startup_metadata(channel, options)?;
            }
            if options.ext_multigrid {
                self.emit_highlights(channel_id, channel, highlights, options)?;
                let (width, height) = channel.size();
                let mut default_grid = self.take_scratch(channel_id)?;
                default_grid.reshape(width, height)?;
                for layer in compositor.layers() {
                    if let Some((statusline, hl_id)) = &layer.statusline {
                        let row = usize::try_from(layer.row)
                            .unwrap_or(0)
                            .saturating_add(layer.grid.height());
                        if row < height {
                            default_grid.write_text(
                                row,
                                usize::try_from(layer.col).unwrap_or(0),
                                statusline,
                                *hl_id,
                            )?;
                        }
                    }
                }
                compositor.paint_tabline_row(&mut default_grid)?;
                let cmdline_cursor = if options.ext_cmdline {
                    None
                } else {
                    apply_cmdline_fallback(&mut default_grid, highlights, chrome)?
                };
                self.emit_grid(channel_id, channel, &default_grid)?;
                let float_compindex = float_compindexes(compositor);
                for (index, layer) in compositor.layers().iter().enumerate() {
                    if options.ext_messages && layer.kind == LayerKind::Message {
                        continue;
                    }
                    emit_position(channel, layer, float_compindex.get(&index).copied())?;
                    self.emit_grid(channel_id, channel, &layer.grid)?;
                    if let Some((row, col)) = layer.cursor {
                        channel.emit(UiEvent::new(
                            "grid_cursor_goto",
                            vec![Object::Integer(layer.grid.id()), integer(row), integer(col)],
                        ))?;
                    }
                }
                if let Some((row, col)) = cmdline_cursor {
                    channel.emit(UiEvent::new(
                        "grid_cursor_goto",
                        vec![Object::Integer(1), integer(row), integer(col)],
                    ))?;
                }
                self.restore_scratch(channel_id, default_grid);
            } else {
                let mut output = self.take_scratch(channel_id)?;
                let composed = if options.ext_messages {
                    compositor.compose_into_without_messages(&mut output, highlights)?
                } else {
                    compositor.compose_into(&mut output, highlights)?
                };
                self.emit_highlights(channel_id, channel, highlights, options)?;
                let message_cursor = if options.ext_messages {
                    None
                } else {
                    apply_message_fallback(&mut output, highlights, chrome)?
                };
                let cmdline_cursor = if options.ext_cmdline {
                    None
                } else {
                    apply_cmdline_fallback(&mut output, highlights, chrome)?
                };
                if !options.ext_popupmenu {
                    apply_popupmenu_fallback(&mut output, highlights, chrome)?;
                }
                self.emit_grid(channel_id, channel, &output)?;
                if let Some((row, col)) = cmdline_cursor.or(message_cursor).or(composed.cursor) {
                    channel.emit(UiEvent::new(
                        "grid_cursor_goto",
                        vec![Object::Integer(1), integer(row), integer(col)],
                    ))?;
                }
                self.restore_scratch(channel_id, output);
            }
            let current_grids = compositor
                .layers()
                .iter()
                .map(|layer| layer.grid.id())
                .collect::<BTreeSet<_>>();
            let previous_grids = self
                .watched_extmarks
                .keys()
                .filter_map(|(id, grid)| (*id == channel_id).then_some(*grid))
                .collect::<BTreeSet<_>>();
            let layout_changed = current_grids != previous_grids;
            for layer in compositor.layers() {
                let key = (channel_id, layer.grid.id());
                let previous = self
                    .watched_extmarks
                    .get(&key)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let changed = layout_changed
                    || (previous != layer.watched_extmarks.as_slice()
                        && !watched_scroll_only(previous, &layer.watched_extmarks));
                if changed && let Some(window) = layer.window {
                    for mark in &layer.watched_extmarks {
                        channel.emit(UiEvent::new(
                            "win_extmark",
                            vec![
                                Object::Integer(layer.grid.id()),
                                Object::Window(window),
                                Object::Integer(i64::from(mark.namespace)),
                                Object::Integer(i64::from(mark.mark)),
                                integer(mark.row),
                                integer(mark.col),
                            ],
                        ))?;
                    }
                }
                self.watched_extmarks
                    .insert(key, layer.watched_extmarks.clone());
            }
            self.watched_extmarks
                .retain(|(id, grid), _| *id != channel_id || current_grids.contains(grid));
            let routed_chrome = if first_redraw {
                &initial_chrome_events
            } else {
                &chrome_events
            };
            route_chrome(channel, options, routed_chrome)?;
            frames.insert(channel_id, channel.flush()?);
        }
        // The semantic stream for vim.ui_attach callbacks is the events that
        // fired this pass only: upstream dispatches each ui event exactly
        // once at emission (ui.c:790-822). The persistent-state snapshot
        // initializes newly attached RPC UIs (`routed_chrome` above), never
        // Lua callbacks.
        Ok(RedrawOutput(frames, chrome_events))
    }

    /// Emits a grid resize plus either a full initial image or minimal line diffs.
    ///
    /// # Errors
    ///
    /// Returns [`UiChannelError::BatchNotStarted`] if the channel has no active
    /// redraw batch.
    pub fn emit_grid(
        &mut self,
        channel_id: u64,
        channel: &mut crate::channel::UiChannel,
        grid: &Grid,
    ) -> Result<(), UiChannelError> {
        let key = (channel_id, grid.id());
        let lines = if let Some(previous) = self.previous.get_mut(&key) {
            if previous.width() == grid.width() && previous.height() == grid.height() {
                // Same geometry: one pass emits the changed spans and updates
                // `previous` in place — no whole-grid clone per flush.
                let lines = grid.diff_and_sync(previous);
                for line in lines {
                    channel.emit(grid_line_event(grid.id(), line))?;
                }
                return Ok(());
            }
            channel.emit(UiEvent::new(
                "grid_resize",
                vec![
                    Object::Integer(grid.id()),
                    integer(grid.width()),
                    integer(grid.height()),
                ],
            ))?;
            grid.full_lines()
        } else {
            channel.emit(UiEvent::new(
                "grid_resize",
                vec![
                    Object::Integer(grid.id()),
                    integer(grid.width()),
                    integer(grid.height()),
                ],
            ))?;
            grid.full_lines()
        };
        for line in lines {
            channel.emit(grid_line_event(grid.id(), line))?;
        }
        // First attach or geometry change: reseed the snapshot. The steady
        // state never reaches this line.
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
                    vec![
                        Object::String(name.clone()),
                        Object::Integer(i64::try_from(id).unwrap_or(i64::MAX)),
                    ],
                ))?;
                self.sent_groups.insert(key, id);
            }
        }
        Ok(())
    }
}

/// The canonical mode table: the sole source of `mode_info_set` entries
/// and of the index every `mode_change` event carries.
const MODES: &[(&str, &str, &str, i64, i64, i64, i64)] = &[
    ("normal", "n", "block", 0, 0, 0, 0),
    ("visual", "v", "block", 0, 0, 0, 0),
    ("insert", "i", "vertical", 25, 0, 0, 0),
    ("replace", "r", "horizontal", 20, 0, 0, 0),
    ("cmdline_normal", "c", "block", 0, 0, 0, 0),
    ("cmdline_insert", "ci", "vertical", 25, 0, 0, 0),
    ("cmdline_replace", "cr", "horizontal", 20, 0, 0, 0),
    ("operator", "o", "block", 0, 0, 0, 0),
    ("visual_select", "ve", "block", 0, 0, 0, 0),
    ("cmdline_hover", "c", "block", 0, 0, 0, 0),
    ("statusline_hover", "s", "block", 0, 0, 0, 0),
    ("statusline_drag", "sd", "block", 0, 0, 0, 0),
    ("vsep_hover", "vs", "block", 0, 0, 0, 0),
    ("vsep_drag", "vd", "block", 0, 0, 0, 0),
    ("more", "m", "block", 0, 0, 0, 0),
    ("more_lastline", "ml", "block", 0, 0, 0, 0),
    ("showmatch", "sm", "block", 0, 0, 0, 0),
    ("terminal", "t", "block", 0, 500, 500, 0),
];

/// Index of `name` in the canonical mode table; upstream `mode_change`
/// events carry this index, and UIs validate the pair against
/// `mode_info_set`. `normal` and any unlisted name answer index 0.
#[must_use]
pub fn mode_index(name: &str) -> usize {
    MODES
        .iter()
        .position(|(mode, ..)| *mode == name)
        .unwrap_or(0)
}

fn emit_startup_metadata(
    channel: &mut crate::channel::UiChannel,
    options: UiOptions,
) -> Result<(), UiChannelError> {
    for (name, value) in [
        ("ambiwidth", Object::String(OxStr::from("single"))),
        ("arabicshape", Object::Boolean(true)),
        ("emoji", Object::Boolean(true)),
        (
            "guifont",
            Object::String(OxStr::from(
                "Source Code Pro,DejaVu Sans Mono,Courier New,monospace",
            )),
        ),
        ("guifontwide", Object::String(OxStr::from(""))),
        ("linespace", Object::Integer(0)),
        ("mousefocus", Object::Boolean(false)),
        ("mousehide", Object::Boolean(true)),
        ("mousemoveevent", Object::Boolean(false)),
        ("pumblend", Object::Integer(0)),
        ("showtabline", Object::Integer(1)),
        ("termguicolors", Object::Boolean(false)),
        ("termsync", Object::Boolean(true)),
        ("ttimeout", Object::Boolean(true)),
        ("ttimeoutlen", Object::Integer(50)),
        ("verbose", Object::Integer(0)),
    ] {
        channel.emit(UiEvent::new(
            "option_set",
            vec![Object::String(OxStr::from(name)), value],
        ))?;
    }
    for (name, enabled) in [
        ("ext_linegrid", options.ext_linegrid),
        ("ext_multigrid", options.ext_multigrid),
        ("ext_hlstate", options.ext_hlstate),
        ("ext_termcolors", options.ext_termcolors),
    ] {
        channel.emit(UiEvent::new(
            "option_set",
            vec![Object::String(OxStr::from(name)), Object::Boolean(enabled)],
        ))?;
    }
    channel.emit(UiEvent::new(
        "default_colors_set",
        vec![
            Object::Integer(14_738_154),
            Object::Integer(1_316_379),
            Object::Integer(-1),
            Object::Integer(0),
            Object::Integer(0),
        ],
    ))?;

    let modes = MODES
        .iter()
        .copied()
        .map(
            |(name, short_name, cursor_shape, cell_percentage, blinkwait, blinkon, blinkoff)| {
                Object::Dict(ox_types::Dict(vec![
                    (OxStr::from("name"), Object::String(OxStr::from(name))),
                    (
                        OxStr::from("short_name"),
                        Object::String(OxStr::from(short_name)),
                    ),
                    (
                        OxStr::from("cursor_shape"),
                        Object::String(OxStr::from(cursor_shape)),
                    ),
                    (
                        OxStr::from("cell_percentage"),
                        Object::Integer(cell_percentage),
                    ),
                    (OxStr::from("blinkwait"), Object::Integer(blinkwait)),
                    (OxStr::from("blinkon"), Object::Integer(blinkon)),
                    (OxStr::from("blinkoff"), Object::Integer(blinkoff)),
                    (OxStr::from("attr_id"), Object::Integer(0)),
                    (OxStr::from("attr_id_lm"), Object::Integer(0)),
                    (OxStr::from("hl_id"), Object::Integer(0)),
                    (OxStr::from("id_lm"), Object::Integer(0)),
                ]))
            },
        )
        .collect();
    channel.emit(UiEvent::new(
        "mode_info_set",
        vec![Object::Boolean(true), Object::Array(modes)],
    ))
}

fn emit_position(
    channel: &mut crate::channel::UiChannel,
    layer: &Layer,
    compindex: Option<usize>,
) -> Result<(), EmitterError> {
    let window = layer.window.map_or(Object::Nil, Object::Window);
    match layer.kind {
        LayerKind::Window => channel
            .emit(UiEvent::new(
                "win_pos",
                vec![
                    Object::Integer(layer.grid.id()),
                    window,
                    signed(layer.row),
                    signed(layer.col),
                    integer(layer.grid.width()),
                    integer(layer.grid.height()),
                ],
            ))
            .map_err(EmitterError::from),
        LayerKind::Float => {
            // Route through i32: the widening to f64 is provably lossless
            // (f64's mantissa holds every i32), so anything wider is rejected
            // instead of silently rounded.
            let row = i32::try_from(layer.row).map_err(|_| EmitterError::Position(layer.row))?;
            let col = i32::try_from(layer.col).map_err(|_| EmitterError::Position(layer.col))?;
            channel
                .emit(UiEvent::new(
                    "win_float_pos",
                    vec![
                        Object::Integer(layer.grid.id()),
                        window,
                        Object::String(OxStr::from("NW")),
                        Object::Integer(1),
                        Object::Float(f64::from(row)),
                        Object::Float(f64::from(col)),
                        Object::Boolean(false),
                        Object::Integer(i64::from(layer.zindex)),
                        Object::Integer(i64::try_from(compindex.unwrap_or(0)).unwrap_or(i64::MAX)),
                        signed(layer.row),
                        signed(layer.col),
                    ],
                ))
                .map_err(EmitterError::from)
        }
        LayerKind::Message => channel
            .emit(UiEvent::new(
                "msg_set_pos",
                vec![
                    Object::Integer(layer.grid.id()),
                    signed(layer.row),
                    Object::Boolean(false),
                    Object::String(OxStr::from(" ")),
                    Object::Integer(i64::from(crate::compositor::MESSAGE_ZINDEX)),
                    Object::Integer(0),
                ],
            ))
            .map_err(EmitterError::from),
    }
}

/// Computes the 1-based compositor index of every float layer, in stacking
/// order. Mirrors Neovim's `comp_index`: the position of a float grid among
/// the layered grids above the default grid, not the UI channel id. Two floats
/// with equal z-index keep distinct, stable indexes in insertion order.
fn float_compindexes(compositor: &Compositor) -> BTreeMap<usize, usize> {
    let mut floats: Vec<(u32, usize)> = compositor
        .layers()
        .iter()
        .enumerate()
        .filter(|(_, layer)| layer.kind == LayerKind::Float)
        .map(|(index, layer)| (layer.zindex, index))
        .collect();
    floats.sort_unstable();
    floats
        .into_iter()
        .enumerate()
        .map(|(rank, (_, index))| (index, rank + 1))
        .collect()
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
        if supported {
            channel.emit(event.clone())?;
        }
    }
    Ok(())
}

/// Default maximum popup menu height when the `pumheight` option is unset.
///
/// Mirrors `PUM_DEF_HEIGHT` in `.references/neovim/src/nvim/popupmenu.c:92`.
const PUM_DEF_HEIGHT: usize = 10;

/// Default minimum popup menu width (`pumwidth` option default).
///
/// Upstream `options.lua` defines `pumwidth` with default `15`.
const PUM_MIN_WIDTH: usize = 15;

/// String drawn for multi-line messages waiting for a key press.
///
/// Mirrors `hit_return_msg` in `.references/neovim/src/nvim/message.c:1624`.
const HIT_ENTER_PROMPT: &str = "Press ENTER or type command to continue";

/// Ensures the highlight groups chrome fallbacks paint with are interned.
///
/// These groups are the producer's responsibility, but the fallback path must
/// not paint with attribute `0` when the producer has not yet defined them.
fn ensure_chrome_highlights(highlights: &mut HlState) -> Result<(), HlError> {
    define_group_if_missing(highlights, "Question", question_highlight())?;
    define_group_if_missing(highlights, "Pmenu", pmenu_highlight())?;
    define_group_if_missing(highlights, "PmenuSel", pmenu_sel_highlight())?;
    define_group_if_missing(highlights, "PmenuSbar", pmenu_sbar_highlight())?;
    define_group_if_missing(highlights, "PmenuThumb", pmenu_thumb_highlight())?;
    define_group_if_missing(highlights, "ErrorMsg", error_msg_highlight())?;
    define_group_if_missing(highlights, "MsgArea", Highlight::default())?;
    define_group_if_missing(highlights, "MoreMsg", more_msg_highlight())?;
    Ok(())
}

fn define_group_if_missing(
    highlights: &mut HlState,
    name: &str,
    highlight: Highlight,
) -> Result<u64, HlError> {
    if let Some(id) = highlights.group_id(&OxStr::from(name)) {
        Ok(id)
    } else {
        highlights.define_group(name, highlight)
    }
}

fn named_highlight(name: &str, rgb: HlAttrs, cterm: HlAttrs, cterm_explicit: bool) -> Highlight {
    Highlight {
        rgb,
        cterm,
        cterm_explicit,
        info: vec![HlInfo {
            kind: OxStr::from("ui"),
            hi_name: Some(OxStr::from(name)),
            ui_name: Some(OxStr::from(name)),
            id: None,
        }],
        ..Highlight::default()
    }
}

fn question_highlight() -> Highlight {
    named_highlight(
        "Question",
        HlAttrs {
            foreground: Some(0x00_73_73),
            ..HlAttrs::default()
        },
        HlAttrs {
            foreground: Some(6),
            fg_indexed: true,
            ..HlAttrs::default()
        },
        true,
    )
}

fn pmenu_highlight() -> Highlight {
    named_highlight(
        "Pmenu",
        HlAttrs {
            background: Some(0xc4_c6_cd),
            ..HlAttrs::default()
        },
        HlAttrs {
            reverse: true,
            ..HlAttrs::default()
        },
        true,
    )
}

fn pmenu_sel_highlight() -> Highlight {
    named_highlight(
        "PmenuSel",
        HlAttrs {
            reverse: true,
            ..HlAttrs::default()
        },
        HlAttrs {
            reverse: true,
            underline: true,
            ..HlAttrs::default()
        },
        true,
    )
}

fn pmenu_sbar_highlight() -> Highlight {
    // Upstream links PmenuSbar to Pmenu, but for the fallback we keep it
    // as a distinct group so the symbolic resolver can tell track from item.
    named_highlight(
        "PmenuSbar",
        HlAttrs {
            background: Some(0xc4_c6_cd),
            ..HlAttrs::default()
        },
        HlAttrs {
            reverse: true,
            ..HlAttrs::default()
        },
        true,
    )
}

fn pmenu_thumb_highlight() -> Highlight {
    named_highlight(
        "PmenuThumb",
        HlAttrs {
            background: Some(0x9b_9e_a4),
            ..HlAttrs::default()
        },
        HlAttrs {
            background: Some(8),
            bg_indexed: true,
            ..HlAttrs::default()
        },
        true,
    )
}

fn error_msg_highlight() -> Highlight {
    named_highlight(
        "ErrorMsg",
        HlAttrs {
            foreground: Some(0x59_00_08),
            ..HlAttrs::default()
        },
        HlAttrs {
            foreground: Some(1),
            fg_indexed: true,
            ..HlAttrs::default()
        },
        true,
    )
}

fn more_msg_highlight() -> Highlight {
    named_highlight(
        "MoreMsg",
        HlAttrs {
            foreground: Some(0x00_5f_00),
            ..HlAttrs::default()
        },
        HlAttrs {
            foreground: Some(2),
            fg_indexed: true,
            ..HlAttrs::default()
        },
        true,
    )
}
fn apply_message_fallback(
    grid: &mut Grid,
    highlights: &HlState,
    chrome: &ChromeState,
) -> Result<Option<(usize, usize)>, GridError> {
    let msg_area_id = highlights.group_id(&OxStr::from("MsgArea")).unwrap_or(0);
    if let Some(message) = &chrome.message {
        if message
            .content
            .iter()
            .all(|chunk| chunk.text.to_string_lossy().is_empty())
        {
            return Ok(None);
        }
        let question_id = highlights.group_id(&OxStr::from("Question")).unwrap_or(0);
        let logical = message_lines(&message.content);
        let mut visual: Vec<Vec<(String, u64)>> = Vec::new();
        for line in logical {
            visual.extend(wrap_fragments(&line, grid.width()));
        }
        if visual.is_empty() {
            return Ok(None);
        }
        if visual.len() == 1 {
            let floor = message_floor(grid, chrome);
            paint_message_line(grid, floor, &visual[0], msg_area_id)?;
            return Ok(None);
        }
        let prompt = wrap_fragments(&[(HIT_ENTER_PROMPT.to_owned(), question_id)], grid.width());
        let floor = message_floor(grid, chrome);
        // The message area owns `floor + 1` rows; a prompt that wraps past
        // that keeps its tail (upstream draws the prompt bottom-aligned in
        // the message area, `msg_scroll_up`, message.c), never past it.
        let available = floor + 1;
        let displayed = visual.len().min(available.saturating_sub(prompt.len()));
        let start = visual.len().saturating_sub(displayed);
        let displayed_content = &visual[start..];
        let prompt_avail = available - displayed;
        let prompt_start = prompt.len().saturating_sub(prompt_avail);
        let shown_prompt = &prompt[prompt_start..];
        let total = displayed + shown_prompt.len();
        let first_row = available.saturating_sub(total);
        let scroll_rows = isize::try_from(total).unwrap_or(0);
        if scroll_rows > 0 {
            grid.scroll(0, grid.height(), 0, grid.width(), scroll_rows, 0)?;
        }
        for (i, line) in displayed_content.iter().enumerate() {
            paint_message_line(grid, first_row + i, line, msg_area_id)?;
        }
        for (i, line) in shown_prompt.iter().enumerate() {
            paint_message_line(grid, first_row + displayed + i, line, msg_area_id)?;
        }
        if let Some(last) = shown_prompt.last() {
            let row = first_row + displayed + shown_prompt.len() - 1;
            let col = line_width(last).min(grid.width());
            Ok(Some((row, col)))
        } else {
            Ok(None)
        }
    } else if !chrome.showmode.is_empty() {
        let floor = message_floor(grid, chrome);
        let line = chunks_to_fragments(&chrome.showmode);
        let visual = wrap_fragments(&line, grid.width());
        if let Some(first) = visual.first() {
            paint_message_line(grid, floor, first, msg_area_id)?;
        }
        Ok(None)
    } else {
        Ok(None)
    }
}

fn paint_message_line(
    grid: &mut Grid,
    row: usize,
    line: &[(String, u64)],
    msg_area_id: u64,
) -> Result<(), GridError> {
    if grid.width() == 0 {
        return Ok(());
    }
    let fill_id = line.last().map_or(
        msg_area_id,
        |(_, hl_id)| {
            if *hl_id == 0 { msg_area_id } else { *hl_id }
        },
    );
    grid.write_text(row, 0, &" ".repeat(grid.width()), fill_id)?;
    let mut col = 0;
    for (text, hl_id) in line {
        if !text.is_empty() {
            let write_id = if *hl_id == 0 { msg_area_id } else { *hl_id };
            col = grid.write_text(row, col, text, write_id)?;
        }
    }
    Ok(())
}

fn message_floor(grid: &Grid, chrome: &ChromeState) -> usize {
    let reserved = usize::from(chrome.cmdline.is_some() || cmdline_mode_active(chrome));
    grid.height().saturating_sub(1).saturating_sub(reserved)
}

fn cmdline_mode_active(chrome: &ChromeState) -> bool {
    matches!(&chrome.mode, Some((name, _)) if name.as_bytes().starts_with(b"cmdline"))
}

fn chunks_to_fragments(chunks: &[ContentChunk]) -> Vec<(String, u64)> {
    chunks
        .iter()
        .map(|chunk| (chunk.text.to_string_lossy().into_owned(), chunk.hl_id))
        .collect()
}

fn message_lines(chunks: &[ContentChunk]) -> Vec<Vec<(String, u64)>> {
    let mut lines: Vec<Vec<(String, u64)>> = Vec::new();
    let mut current: Vec<(String, u64)> = Vec::new();
    for chunk in chunks {
        let text = chunk.text.to_string_lossy().into_owned();
        let mut run = String::new();
        for c in text.chars() {
            if c == '\n' {
                if !run.is_empty() {
                    current.push((std::mem::take(&mut run), chunk.hl_id));
                }
                if current.is_empty() {
                    current.push((String::new(), chunk.hl_id));
                }
                lines.push(std::mem::take(&mut current));
            } else {
                run.push(c);
            }
        }
        if !run.is_empty() {
            current.push((std::mem::take(&mut run), chunk.hl_id));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn wrap_fragments(fragments: &[(String, u64)], width: usize) -> Vec<Vec<(String, u64)>> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines: Vec<Vec<(String, u64)>> = Vec::new();
    let mut current: Vec<(String, u64)> = Vec::new();
    let mut col = 0;
    for (text, hl_id) in fragments {
        if text.is_empty() {
            current.push((String::new(), *hl_id));
            continue;
        }
        let mut byte = 0;
        while byte < text.len() {
            let remaining = width.saturating_sub(col);
            if remaining == 0 {
                lines.push(std::mem::take(&mut current));
                col = 0;
                continue;
            }
            let mut end = byte;
            let mut w = 0;
            for (i, c) in text[byte..].char_indices() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(1);
                if w + cw > remaining {
                    break;
                }
                w += cw;
                end = byte + i + c.len_utf8();
                if w == remaining {
                    break;
                }
            }
            if end == byte {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    col = 0;
                    continue;
                }
                let c = text[byte..].chars().next().unwrap_or('\0');
                let len = c.len_utf8().max(1);
                let piece = if text.len() >= byte + len {
                    text[byte..byte + len].to_owned()
                } else {
                    text[byte..].to_owned()
                };
                current.push((piece, *hl_id));
                byte = (byte + len).min(text.len());
                col += UnicodeWidthChar::width(c).unwrap_or(1);
            } else {
                current.push((text[byte..end].to_owned(), *hl_id));
                byte = end;
                col += w;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn line_width(line: &[(String, u64)]) -> usize {
    line.iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum()
}

fn apply_cmdline_fallback(
    grid: &mut Grid,
    _highlights: &HlState,
    chrome: &ChromeState,
) -> Result<Option<(usize, usize)>, GridError> {
    if grid.height() == 0 {
        return Ok(None);
    }
    let row = grid.height() - 1;
    if let Some(cmdline) = &chrome.cmdline {
        grid.write_text(row, 0, &" ".repeat(grid.width()), 0)?;
        let mut col = 0;
        col = grid.write_text(
            row,
            col,
            &cmdline.first_char.to_string_lossy(),
            cmdline.hl_id,
        )?;
        col = grid.write_text(row, col, &cmdline.prompt.to_string_lossy(), cmdline.hl_id)?;
        if cmdline.indent != 0 {
            grid.set_hl_span(row, col, col + cmdline.indent, cmdline.hl_id)?;
            col += cmdline.indent;
        }
        let mut cursor = None;
        let mut remaining = cmdline.position;
        for chunk in &cmdline.content {
            let text = chunk.text.to_string_lossy();
            let mut split = remaining.min(text.len());
            while !text.is_char_boundary(split) {
                split -= 1;
            }
            let (before, after) = text.split_at(split);
            col = grid.write_text(row, col, before, chunk.hl_id)?;
            remaining = remaining.saturating_sub(split);
            if remaining == 0 && cursor.is_none() {
                cursor = Some((row, col));
            }
            col = grid.write_text(row, col, after, chunk.hl_id)?;
        }
        Ok(cursor.or(Some((row, col))))
    } else if cmdline_mode_active(chrome) {
        grid.write_text(row, 0, &" ".repeat(grid.width()), 0)?;
        Ok(Some((row, 0)))
    } else if !chrome.cmdline_block.is_empty() {
        let block = &chrome.cmdline_block;
        let rows = block.len().min(grid.height());
        for (i, line) in block.iter().rev().take(rows).enumerate() {
            let r = row.saturating_sub(i);
            grid.write_text(r, 0, &" ".repeat(grid.width()), 0)?;
            let mut col = 0;
            for chunk in line {
                col = grid.write_text(r, col, &chunk.text.to_string_lossy(), chunk.hl_id)?;
            }
        }
        Ok(Some((row, 0)))
    } else {
        Ok(None)
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the popupmenu rows, scrollbar, and truncation rules form one draw pass"
)]
fn apply_popupmenu_fallback(
    grid: &mut Grid,
    highlights: &HlState,
    chrome: &ChromeState,
) -> Result<(), GridError> {
    let Some(state) = &chrome.popupmenu else {
        return Ok(());
    };
    if grid.height() == 0 || grid.width() == 0 || state.items.is_empty() {
        return Ok(());
    }
    if state.row >= grid.height() || state.col >= grid.width() {
        return Ok(());
    }

    let pmenu = highlights.group_id(&OxStr::from("Pmenu")).unwrap_or(0);
    let pmenu_sel = highlights
        .group_id(&OxStr::from("PmenuSel"))
        .unwrap_or(pmenu);
    let pmenu_sbar = highlights
        .group_id(&OxStr::from("PmenuSbar"))
        .unwrap_or(pmenu);
    let pmenu_thumb = highlights
        .group_id(&OxStr::from("PmenuThumb"))
        .unwrap_or(pmenu_sbar);

    let mut word_width = 0;
    let mut kind_width = 0;
    let mut menu_width = 0;
    for item in &state.items {
        let word = item.word.to_string_lossy();
        word_width = word_width.max(UnicodeWidthStr::width(word.as_ref()));
        let kind = item.kind.to_string_lossy();
        if !kind.is_empty() {
            kind_width = kind_width.max(UnicodeWidthStr::width(kind.as_ref()) + 1);
        }
        let menu = item.menu.to_string_lossy();
        if !menu.is_empty() {
            menu_width = menu_width.max(UnicodeWidthStr::width(menu.as_ref()) + 1);
        }
    }
    let natural_width = word_width + kind_width + menu_width;
    let total_width = natural_width.max(PUM_MIN_WIDTH);

    let selected = if state.selected < 0 {
        -1
    } else {
        state
            .selected
            .min(i64::try_from(state.items.len()).unwrap_or(i64::MAX) - 1)
    };

    // Upstream pum_row is one row below the cursor row (pum_win_row).
    let first_row = state.row + 1;
    // Reserve the bottom row for the status line.
    let available_height = grid.height().saturating_sub(first_row).saturating_sub(1);
    let pum_size = state.items.len();
    let pum_height = pum_size.min(available_height).min(PUM_DEF_HEIGHT);
    if pum_height == 0 {
        return Ok(());
    }

    let pum_scrollbar = pum_size > pum_height;
    let available_width = grid.width().saturating_sub(state.col);
    let pum_width = if pum_scrollbar {
        total_width.min(available_width.saturating_sub(1))
    } else {
        total_width.min(available_width)
    };
    if pum_width == 0 {
        return Ok(());
    }

    // pum_first: keep a few context lines around the selected item.
    // Mirrors popupmenu.c:1075-1128.
    let context = pum_height / 2;
    let mut pum_first: usize = 0;
    if pum_height > 2 && selected >= 0 {
        let selected_i64 = selected;
        let scroll_down = selected_i64 - i64::try_from(context).unwrap_or(0);
        let pum_first_i64 = i64::try_from(pum_first).unwrap_or(0);
        if pum_first_i64 > scroll_down {
            pum_first = usize::try_from(scroll_down.max(0)).unwrap_or(0);
        } else {
            let scroll_up = selected_i64 + i64::try_from(context).unwrap_or(0)
                - i64::try_from(pum_height).unwrap_or(0)
                + 1;
            if pum_first_i64 < scroll_up {
                pum_first = usize::try_from(scroll_up.max(0)).unwrap_or(0);
            }
        }
        let max_first = i64::try_from(pum_size.saturating_sub(pum_height)).unwrap_or(0);
        pum_first = pum_first.min(usize::try_from(max_first.max(0)).unwrap_or(0));
    } else if selected >= 0 {
        let first = selected - i64::try_from(pum_height).unwrap_or(0) + 1;
        pum_first = usize::try_from(first.max(0)).unwrap_or(0);
        let max_first = i64::try_from(pum_size.saturating_sub(pum_height)).unwrap_or(0);
        pum_first = pum_first.min(usize::try_from(max_first.max(0)).unwrap_or(0));
    }

    let scroll_range = pum_size.saturating_sub(pum_height);
    let thumb_height = if pum_scrollbar {
        (pum_height * pum_height / pum_size).max(1)
    } else {
        0
    };
    let thumb_pos = if pum_scrollbar && scroll_range > 0 {
        (pum_first * (pum_height - thumb_height) + scroll_range / 2) / scroll_range
    } else {
        0
    };

    for i in 0..pum_height {
        let idx = pum_first + i;
        if idx >= pum_size {
            break;
        }
        let item = &state.items[idx];
        let is_selected = selected >= 0 && idx == usize::try_from(selected).unwrap_or(usize::MAX);
        let row = first_row + i;
        let row_attr = if is_selected { pmenu_sel } else { pmenu };

        if state.col + pum_width <= grid.width() {
            grid.write_text(row, state.col, &" ".repeat(pum_width), row_attr)?;
        }

        let mut col = state.col;
        let word = item.word.to_string_lossy();
        write_truncated(grid, row, col, word.as_ref(), word_width, row_attr)?;
        col += word_width;

        if kind_width > 0 {
            let kind = item.kind.to_string_lossy();
            let text = kind.as_ref();
            let text_width = UnicodeWidthStr::width(text).min(kind_width - 1);
            write_truncated(
                grid,
                row,
                col + 1,
                &text[..char_prefix_end(text, text_width)],
                text_width,
                row_attr,
            )?;
            col += kind_width;
        }

        if menu_width > 0 {
            let menu = item.menu.to_string_lossy();
            let text = menu.as_ref();
            let text_width = UnicodeWidthStr::width(text).min(menu_width - 1);
            write_truncated(
                grid,
                row,
                col + 1,
                &text[..char_prefix_end(text, text_width)],
                text_width,
                row_attr,
            )?;
        }

        if pum_scrollbar {
            let thumb = i >= thumb_pos && i < thumb_pos + thumb_height;
            let sbar_attr = if thumb { pmenu_thumb } else { pmenu_sbar };
            let sbar_col = state.col + pum_width;
            if sbar_col < grid.width() {
                grid.put(row, sbar_col, " ", sbar_attr, 1)?;
            }
        }
    }
    Ok(())
}

fn char_prefix_end(text: &str, max_width: usize) -> usize {
    let mut end = 0;
    let mut w = 0;
    for (i, c) in text.char_indices() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(1);
        if w + cw > max_width {
            break;
        }
        w += cw;
        end = i + c.len_utf8();
    }
    end
}

fn write_truncated(
    grid: &mut Grid,
    row: usize,
    start_col: usize,
    text: &str,
    max_width: usize,
    hl_id: u64,
) -> Result<(), GridError> {
    if max_width == 0 || start_col >= grid.width() {
        return Ok(());
    }
    let end = char_prefix_end(text, max_width);
    grid.write_text(row, start_col, &text[..end], hl_id)?;
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

fn watched_scroll_only(previous: &[WatchedExtmark], current: &[WatchedExtmark]) -> bool {
    if previous.is_empty() || previous.len() != current.len() {
        return false;
    }
    let mut row_delta = None;
    for (before, after) in previous.iter().zip(current) {
        if before.namespace != after.namespace
            || before.mark != after.mark
            || before.col != after.col
            || before.buffer_row != after.buffer_row
        {
            return false;
        }
        let delta = i128::try_from(after.row).unwrap_or(i128::MAX)
            - i128::try_from(before.row).unwrap_or(i128::MAX);
        if delta == 0 || row_delta.is_some_and(|expected| expected != delta) {
            return false;
        }
        row_delta = Some(delta);
    }
    true
}

fn grid_line_event(grid: i64, line: GridLine) -> UiEvent {
    UiEvent::new(
        "grid_line",
        vec![
            Object::Integer(grid),
            integer(line.row),
            integer(line.start_col),
            Object::Array(line.cells),
            Object::Boolean(line.wrap),
        ],
    )
}

fn integer(value: usize) -> Object {
    Object::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}
fn signed(value: isize) -> Object {
    Object::Integer(i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX }))
}
