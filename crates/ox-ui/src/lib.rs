#![forbid(unsafe_code)]
//! Server-side UI event emission, grid model, compositor, and chrome state.

pub mod channel;
pub mod chrome;
pub mod compositor;
pub mod emitter;
pub mod grid;
pub mod hl;

pub use channel::{UiChannel, UiChannelError, UiChannels, UiEvent, UiOptions};
pub use chrome::{
    ChromeState, CmdlineState, ContentChunk, MessageState, ModeInfo, PopupItem, PopupmenuState,
};
pub use compositor::{
    ComposedScreen, Compositor, CompositorError, Layer, LayerKind, WatchedExtmark, MESSAGE_ZINDEX,
};
pub use emitter::{Emitter, EmitterError};
pub use grid::{Cell, Grid, GridError, GridLine};
pub use hl::{premix_color, Highlight, HlAttrs, HlError, HlEvent, HlInfo, HlState};
