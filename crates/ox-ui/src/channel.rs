//! Attached UI registry, capability negotiation, and redraw batching.

use std::collections::BTreeMap;

use ox_rpc::{RedrawBatch, RedrawEvent};
use ox_types::{Dict, Object, OxStr};
use thiserror::Error;

/// Negotiated UI extensions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UiOptions {
    /// Line-grid protocol support.
    pub ext_linegrid: bool,
    /// Separate grids per window.
    pub ext_multigrid: bool,
    /// External message presentation.
    pub ext_messages: bool,
    /// External command-line presentation.
    pub ext_cmdline: bool,
    /// External popup-menu presentation.
    pub ext_popupmenu: bool,
    /// Highlight source metadata.
    pub ext_hlstate: bool,
    /// Terminal color fallback preference.
    pub ext_termcolors: bool,
}

impl UiOptions {
    /// Parses recognized boolean `nvim_ui_attach` options. Unknown keys are ignored.
    #[must_use]
    pub fn from_dict(options: &Dict) -> Self {
        let get = |name: &'static str| {
            matches!(options.get(&OxStr::from(name)), Some(Object::Boolean(true)))
        };
        Self {
            ext_linegrid: get("ext_linegrid"),
            ext_multigrid: get("ext_multigrid"),
            ext_messages: get("ext_messages"),
            ext_cmdline: get("ext_cmdline"),
            ext_popupmenu: get("ext_popupmenu"),
            ext_hlstate: get("ext_hlstate"),
            ext_termcolors: get("ext_termcolors"),
        }
        .normalized()
    }

    /// Applies extension implications documented by `ui-ext-options`.
    #[must_use]
    pub const fn normalized(mut self) -> Self {
        if self.ext_multigrid || self.ext_hlstate || self.ext_messages {
            self.ext_linegrid = true;
        }
        if self.ext_messages {
            self.ext_cmdline = true;
        }
        self
    }
}

/// A protocol event before it is grouped into a redraw frame.
#[derive(Clone, Debug, PartialEq)]
pub struct UiEvent {
    /// Event name.
    pub name: OxStr,
    /// Ordered event arguments.
    pub args: Vec<Object>,
}

impl UiEvent {
    /// Creates an event.
    #[must_use]
    pub fn new(name: impl Into<OxStr>, args: Vec<Object>) -> Self {
        Self { name: name.into(), args }
    }
}

/// UI channel lifecycle failures.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum UiChannelError {
    /// A channel is already attached.
    #[error("UI channel {0} is already attached")]
    AlreadyAttached(u64),
    /// A channel is not attached.
    #[error("UI channel {0} is not attached")]
    NotAttached(u64),
    /// UI dimensions must both be non-zero.
    #[error("invalid UI size {width}x{height}")]
    InvalidSize {
        /// Requested width.
        width: usize,
        /// Requested height.
        height: usize,
    },
    /// The legacy cell-at-a-time grid protocol is outside this crate's linegrid contract.
    #[error("UI channel requires ext_linegrid")]
    LegacyGridUnsupported,
    /// A caller tried to emit outside a redraw transaction.
    #[error("UI channel {0} has no active redraw batch")]
    BatchNotStarted(u64),
}

/// One attached remote UI and its active redraw batch.
#[derive(Clone, Debug)]
pub struct UiChannel {
    id: u64,
    width: usize,
    height: usize,
    options: UiOptions,
    batch: Option<RedrawBatch>,
}

impl UiChannel {
    /// Creates an attached channel.
    pub fn new(id: u64, width: usize, height: usize, options: UiOptions) -> Result<Self, UiChannelError> {
        validate_size(width, height)?;
        let options = options.normalized();
        if !options.ext_linegrid { return Err(UiChannelError::LegacyGridUnsupported); }
        Ok(Self { id, width, height, options, batch: None })
    }

    /// RPC channel identity.
    #[must_use]
    pub const fn id(&self) -> u64 { self.id }

    /// Current UI dimensions.
    #[must_use]
    pub const fn size(&self) -> (usize, usize) { (self.width, self.height) }

    /// Negotiated capabilities.
    #[must_use]
    pub const fn options(&self) -> UiOptions { self.options }

    /// Starts a new redraw transaction, discarding no previously completed data.
    pub fn begin(&mut self) { self.batch = Some(RedrawBatch::new()); }

    /// Adds an event to the current transaction.
    pub fn emit(&mut self, event: UiEvent) -> Result<(), UiChannelError> {
        let batch = self.batch.as_mut().ok_or(UiChannelError::BatchNotStarted(self.id))?;
        if event.name == OxStr::from("flush") {
            return Ok(());
        }
        if event.name == OxStr::from("grid_line") {
            if let Some((grid, row, col, cells, wrap)) = grid_line_parts(&event.args) {
                batch.grid_line(grid, row, col, cells, wrap);
                return Ok(());
            }
        }
        batch.push(event.name, event.args);
        Ok(())
    }

    /// Ends the transaction with exactly one `flush` and returns packed bytes.
    pub fn flush(&mut self) -> Result<Vec<u8>, UiChannelError> {
        let mut batch = self.batch.take().ok_or(UiChannelError::BatchNotStarted(self.id))?;
        if batch.events().last().is_none_or(|event| event.name != OxStr::from("flush")) {
            batch.push("flush", vec![]);
        }
        Ok(batch.pack())
    }

    /// Returns current batched events for inspection.
    #[must_use]
    pub fn events(&self) -> Option<&[RedrawEvent]> {
        self.batch.as_ref().map(RedrawBatch::events)
    }

    fn resize(&mut self, width: usize, height: usize) -> Result<(), UiChannelError> {
        validate_size(width, height)?;
        self.width = width;
        self.height = height;
        Ok(())
    }
}

/// Registry of attached UIs keyed by RPC channel id.
#[derive(Clone, Debug, Default)]
pub struct UiChannels {
    channels: BTreeMap<u64, UiChannel>,
}

impl UiChannels {
    /// Creates an empty registry.
    #[must_use]
    pub const fn new() -> Self { Self { channels: BTreeMap::new() } }

    /// Attaches a UI.
    pub fn attach(
        &mut self,
        id: u64,
        width: usize,
        height: usize,
        options: UiOptions,
    ) -> Result<(), UiChannelError> {
        if self.channels.contains_key(&id) { return Err(UiChannelError::AlreadyAttached(id)); }
        self.channels.insert(id, UiChannel::new(id, width, height, options)?);
        Ok(())
    }

    /// Detaches and returns a UI.
    pub fn detach(&mut self, id: u64) -> Result<UiChannel, UiChannelError> {
        self.channels.remove(&id).ok_or(UiChannelError::NotAttached(id))
    }

    /// Changes an attached UI's dimensions.
    pub fn try_resize(&mut self, id: u64, width: usize, height: usize) -> Result<(), UiChannelError> {
        self.channels.get_mut(&id).ok_or(UiChannelError::NotAttached(id))?.resize(width, height)
    }

    /// Reads an attached channel.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&UiChannel> { self.channels.get(&id) }

    /// Mutably reads an attached channel.
    pub fn get_mut(&mut self, id: u64) -> Option<&mut UiChannel> { self.channels.get_mut(&id) }

    /// Iterates attached channels in stable id order.
    pub fn iter(&self) -> impl Iterator<Item = (&u64, &UiChannel)> {
        self.channels.iter()
    }

    /// Iterates attached channels in stable id order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&u64, &mut UiChannel)> {
        self.channels.iter_mut()
    }

    /// Number of attached UIs.
    #[must_use]
    pub fn len(&self) -> usize { self.channels.len() }

    /// Whether no UI is attached.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.channels.is_empty() }
}

fn validate_size(width: usize, height: usize) -> Result<(), UiChannelError> {
    if width == 0 || height == 0 { return Err(UiChannelError::InvalidSize { width, height }); }
    Ok(())
}

fn grid_line_parts(args: &[Object]) -> Option<(i64, i64, i64, Vec<Object>, bool)> {
    let [Object::Integer(grid), Object::Integer(row), Object::Integer(col), Object::Array(cells), Object::Boolean(wrap)] = args else {
        return None;
    };
    Some((*grid, *row, *col, cells.clone(), *wrap))
}
