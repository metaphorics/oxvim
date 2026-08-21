//! Channel ids, the per-channel RPC state, and the `nvim_error_event` helper.
//!
//! # Upstream mapping
//!
//! Channel ids are allocated as in `src/nvim/channel.c` `channel_alloc()`:
//! `CHAN_STDIO = 1`, `CHAN_STDERR = 2` (`channel_defs.h`), and the first
//! dynamic id is `CHAN_STDERR + 1 = 3` (`static next_chan_id = CHAN_STDERR + 1`,
//! then `chan->id = next_chan_id++`).
//!
//! The "stdout-as-stderr" special case: when Nvim is *embedded*, `channel.c
//! channel_from_stdio()` redirects the stdio U/I channel's descriptors onto the
//! process stderr (`dup2(STDERR_FILENO, STDOUT_FILENO)` etc., channel.c:586–593)
//! so the embedder's own stdout stays usable. Channel 2 (`CHAN_STDERR`) itself
//! is a separate non-RPC stream (`v:stderr`, vars.c:317). We model the
//! distinction with [`ChannelKind`] and the [`ChannelState::is_rpc`] flag; the
//! fd aliasing itself is an OS-level concern owned by `ox-loop`.

use std::collections::HashMap;

use ox_types::{ApiError, Object, OxStr};

/// The kind of transport a channel id denotes, mirroring `kChannelStream*`
/// (`channel_defs.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// The primary stdio channel (`CHAN_STDIO = 1`), the embedder's RPC pipe.
    Stdio,
    /// The stderr channel (`CHAN_STDERR = 2`), `v:stderr`; not RPC.
    Stderr,
    /// A job, socket or other dynamically allocated channel (`id >= 3`).
    Dynamic,
}

/// A channel identifier, opaque on purpose so ids cannot be confused with other
/// integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(u64);

/// The primary stdio channel (`CHAN_STDIO = 1`).
pub const CHAN_STDIO: ChannelId = ChannelId(1);
/// The reserved stderr channel (`CHAN_STDERR = 2`).
pub const CHAN_STDERR: ChannelId = ChannelId(2);

impl ChannelId {
    /// Wrap a raw id.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw id value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Which transport this id denotes.
    #[must_use]
    pub const fn kind(self) -> ChannelKind {
        match self.0 {
            1 => ChannelKind::Stdio,
            2 => ChannelKind::Stderr,
            _ => ChannelKind::Dynamic,
        }
    }
}

/// Hands out increasing dynamic channel ids, starting at `3` like upstream's
/// `next_chan_id`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChannelIdAllocator {
    next: u64,
}

impl ChannelIdAllocator {
    /// An allocator whose next id is `3` (`CHAN_STDERR + 1`).
    #[must_use]
    pub fn new() -> Self {
        Self { next: CHAN_STDERR.get() + 1 }
    }

    /// Allocate the next id, increasing monotonically.
    #[must_use]
    pub fn alloc(&mut self) -> ChannelId {
        let id = ChannelId::new(self.next);
        // Saturate rather than overflow; u64 exhaustion is unreachable.
        self.next = self.next.saturating_add(1);
        id
    }
}

/// Per-channel RPC state mirroring `RpcState` in
/// `src/nvim/msgpack_rpc/channel.c` (`rpc_start` / `rpc_close`).
#[derive(Debug, Default, Clone)]
pub struct ChannelState {
    /// Whether `rpc_start()` was called for this channel (`channel->is_rpc`).
    is_rpc: bool,
    /// Whether `rpc_close()` marked the RPC half closed (`rpc.closed`).
    closed: bool,
    /// Outstanding requests keyed by msgid → method name, providing the error
    /// context used when a channel closes with frames in flight
    /// (`chan_close_on_err` walks the call stack).
    pending: HashMap<u32, OxStr>,
}

impl ChannelState {
    /// Freshly created, non-RPC channel state.
    #[must_use]
    pub fn new() -> Self {
        Self { is_rpc: false, closed: false, pending: HashMap::new() }
    }

    /// Start RPC on this channel (`rpc_start`: `is_rpc = true`, `closed = false`).
    pub fn start_rpc(&mut self) {
        self.is_rpc = true;
        self.closed = false;
    }

    /// Mark the RPC half as closed (`rpc_close`).
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Whether this channel is an RPC channel.
    #[must_use]
    pub fn is_rpc(&self) -> bool {
        self.is_rpc
    }

    /// Whether the RPC half is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Record an outstanding request so its method name is available for error
    /// reporting if the channel dies mid-call.
    pub fn register_request(&mut self, msgid: u32, method: OxStr) {
        self.pending.insert(msgid, method);
    }

    /// Resolve (remove) a pending request, returning its method name.
    pub fn resolve_request(&mut self, msgid: u32) -> Option<OxStr> {
        self.pending.remove(&msgid)
    }

    /// The method name recorded for `msgid`, if it is still pending.
    #[must_use]
    pub fn method_for(&self, msgid: u32) -> Option<&OxStr> {
        self.pending.get(&msgid)
    }

    /// Number of requests awaiting a response.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// Encode the `nvim_error_event` notification `[2, "nvim_error_event",
/// [errtype, msg]]`, exactly as `channel.c serialize_response()` emits for a
/// failed notification (`serialize_request(..., 0, "nvim_error_event", args)`
/// where `args` is the `[type, message]` pair).
pub fn nvim_error_event(error: &ApiError) -> Vec<u8> {
    let params = vec![
        Object::Integer(error.error_type()),
        Object::String(OxStr::from(error.message())),
    ];
    crate::message::Message::Notification {
        method: OxStr::from("nvim_error_event"),
        params,
    }
    .encode_bytes()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::codec::IncrementalDecoder;

    #[test]
    fn channel_ids_match_upstream() {
        assert_eq!(CHAN_STDIO.get(), 1);
        assert_eq!(CHAN_STDERR.get(), 2);
        assert_eq!(CHAN_STDIO.kind(), ChannelKind::Stdio);
        assert_eq!(CHAN_STDERR.kind(), ChannelKind::Stderr);
        assert_eq!(ChannelId::new(9).kind(), ChannelKind::Dynamic);
    }

    #[test]
    fn allocator_starts_at_three_and_increases() {
        let mut alloc = ChannelIdAllocator::new();
        assert_eq!(alloc.alloc(), ChannelId::new(3));
        assert_eq!(alloc.alloc(), ChannelId::new(4));
        assert_eq!(alloc.alloc(), ChannelId::new(5));
    }

    #[test]
    fn channel_state_lifecycle() {
        let mut st = ChannelState::new();
        assert!(!st.is_rpc() && !st.is_closed());
        st.start_rpc();
        assert!(st.is_rpc() && !st.is_closed());
        st.register_request(7, OxStr::from("nvim_buf_line_count"));
        st.register_request(8, OxStr::from("nvim_get_mode"));
        assert_eq!(st.pending_count(), 2);
        assert_eq!(st.method_for(7), Some(&OxStr::from("nvim_buf_line_count")));
        assert_eq!(st.resolve_request(7), Some(OxStr::from("nvim_buf_line_count")));
        assert_eq!(st.pending_count(), 1);
        assert_eq!(st.resolve_request(99), None);
        st.close();
        assert!(st.is_closed());
    }

    #[test]
    fn nvim_error_event_wire_shape() {
        let bytes = nvim_error_event(&ApiError::exception("boom"));
        // [2, "nvim_error_event", [0, "boom"]]
        let expected: &[u8] = &[
            0x93,       // array(3)
            0x02,       // notification kind
            0xb0, b'n', b'v', b'i', b'm', b'_', b'e', b'r', b'r', b'o', b'r', b'_', b'e', b'v',
            b'e', b'n', b't', // fixstr(16) "nvim_error_event"
            0x92,       // array(2): params = [errtype, msg]
            0x00,       // type 0 = exception
            0xa4, b'b', b'o', b'o', b'm', // "boom"
        ];
        assert_eq!(bytes, expected);
        let mut dec = IncrementalDecoder::new();
        let msgs = dec.feed(&bytes).unwrap();
        let crate::message::Message::Notification { method, params } = &msgs[0] else {
            panic!("expected notification")
        };
        assert_eq!(*method, OxStr::from("nvim_error_event"));
        assert_eq!(
            *params,
            vec![Object::Integer(0), Object::String(OxStr::from("boom"))]
        );
    }
}