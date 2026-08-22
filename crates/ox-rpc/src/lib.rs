#![forbid(unsafe_code)]
//! msgpack-RPC codec, channel state machine, api-info metadata.
//!
//! Wire protocol mirrors Neovim's msgpack-RPC layer (`.references/neovim/`):
//!
//!   - Requests   are `[0, msgid, method, args]`
//!   - Responses  are `[1, msgid, error|[type,msg], result]`
//!   - Notices    are `[2, method, args]`
//!   - Redraw     is  `[2, "redraw", [[event, [args]...]...]]`
//!
//! Editor handles (Buffer/Window/Tabpage) are msgpack EXT 0/1/2.
//!
//! This crate is pure data handling; I/O and the event loop live in `ox-loop`.

mod channel;
mod codec;
mod message;
mod metadata;
mod redraw;

pub use channel::{
    nvim_error_event, ChannelId, ChannelIdAllocator, ChannelKind, ChannelState, CHAN_STDERR,
    CHAN_STDIO,
};
pub use codec::{decode, encode, DecodeError, IncrementalDecoder};
pub use message::{Message, MsgidCounter};
pub use metadata::{ApiMetadata, canonical_metadata};
pub use redraw::{RedrawBatch, RedrawEvent};