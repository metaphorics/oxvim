//! Readiness-poll handle for watching an arbitrary file descriptor.
//!
//! This is the safe ox-uv mapping of `uv_poll_t` in `runtime/doc/luvref.txt`
//! (lines 1115-1210): `uv.new_poll(fd)`, `uv.new_socket_poll(fd)`,
//! `uv.poll_start()`, and `uv.poll_stop()`. On Unix any descriptor that
//! `poll(2)` accepts may be watched; the descriptor is duplicated and placed
//! in non-blocking mode, then registered with the owning loop's reactor
//! through the sanctioned `UvLoop::inner_mut` seam.

use std::cell::RefCell;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::rc::Rc;

use mio::Token;
use mio::unix::SourceFd;
use ox_loop::{DrainState, Readiness};

use crate::handle::Handle;
use crate::net::queue_batch;
use crate::{CallbackError, HandleId, UvLoop};

/// Requested or reported poll event bits.
///
/// `r` is READABLE, `w` is WRITABLE, `d` is DISCONNECT, and `p` is
/// PRIORITIZED, matching the `uv.poll_start()` event strings in
/// `runtime/doc/luvref.txt`: `"r"`, `"w"`, `"rw"`, `"d"`, `"rd"`, `"wd"`,
/// `"rwd"`, `"p"`, `"rp"`, `"wp"`, `"rwp"`, `"dp"`, `"rdp"`, `"wdp"`, or
/// `"rwdp"`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PollEvents(u8);

pub(crate) const POLL_READABLE: u8 = 0b0001;
pub(crate) const POLL_WRITABLE: u8 = 0b0010;
pub(crate) const POLL_DISCONNECT: u8 = 0b0100;
pub(crate) const POLL_PRIORITIZED: u8 = 0b1000;
const EVENT_NAMES: [&str; 16] = [
    "", "r", "w", "rw", "d", "rd", "wd", "rwd", "p", "rp", "wp", "rwp", "dp", "rdp", "wdp",
    "rwdp",
];

impl PollEvents {
    /// Builds a mask from raw event bits.
    pub fn from_mask(mask: u8) -> Self {
        Self(mask)
    }
    /// Returns whether the READABLE event is set.
    pub fn readable(self) -> bool {
        self.0 & POLL_READABLE != 0
    }
    /// Returns whether the WRITABLE event is set.
    pub fn writable(self) -> bool {
        self.0 & POLL_WRITABLE != 0
    }
    /// Returns whether the DISCONNECT event is set.
    pub fn disconnect(self) -> bool {
        self.0 & POLL_DISCONNECT != 0
    }
    /// Returns whether the PRIORITIZED event is set.
    pub fn prioritized(self) -> bool {
        self.0 & POLL_PRIORITIZED != 0
    }
    /// Returns the luvref event string for these bits, or `None` when none
    /// are set.
    pub fn name(self) -> Option<&'static str> {
        let index = usize::from(self.0);
        if index == 0 { None } else { Some(EVENT_NAMES[index]) }
    }
}

/// Parses a `uv.poll_start()` event string into its bit mask.
pub(crate) fn poll_start_mask(events: &str) -> Option<u8> {
    let mut mask = 0u8;
    for ch in events.chars() {
        match ch {
            'r' => mask |= POLL_READABLE,
            'w' => mask |= POLL_WRITABLE,
            'd' => mask |= POLL_DISCONNECT,
            'p' => mask |= POLL_PRIORITIZED,
            _ => return None,
        }
    }
    Some(mask)
}

/// Maps requested event bits to mio readiness interests.
///
/// DISCONNECT and PRIORITIZED have no exact epoll/mio equivalent; READABLE
/// carries data and hangup (`EPOLLIN`/`EPOLLHUP`) while WRITABLE carries
/// `EPOLLOUT`. These classes therefore map onto READABLE/WRITABLE and the
/// distinction is surfaced in the reported mask, documented as an
/// approximation on the epoll backend.
fn poll_interest(mask: u8) -> mio::Interest {
    let readable = mask & (POLL_READABLE | POLL_DISCONNECT) != 0;
    let writable = mask & (POLL_WRITABLE | POLL_PRIORITIZED) != 0;
    match (readable, writable) {
        (true, true) => mio::Interest::READABLE.add(mio::Interest::WRITABLE),
        (true, false) => mio::Interest::READABLE,
        (false, true) => mio::Interest::WRITABLE,
        (false, false) => mio::Interest::READABLE,
    }
}

/// Computes which requested events fired for a mio readiness notification.
fn fired_events(mask: u8, ready: Readiness) -> u8 {
    let mut fired = 0u8;
    if mask & POLL_READABLE != 0 && (ready.readable || ready.error) {
        fired |= POLL_READABLE;
    }
    if mask & POLL_WRITABLE != 0 && ready.writable {
        fired |= POLL_WRITABLE;
    }
    if mask & POLL_DISCONNECT != 0 && (ready.error || ready.read_closed || ready.write_closed) {
        fired |= POLL_DISCONNECT;
    }
    if mask & POLL_PRIORITIZED != 0 && ready.error {
        fired |= POLL_PRIORITIZED;
    }
    fired
}

type PollHandler = Box<dyn FnMut(&mut UvLoop, HandleId, PollEvents)>;
type PollCallback = Rc<RefCell<Option<PollHandler>>>;

struct PollState {
    fd: Option<OwnedFd>,
    mask: u8,
    active: bool,
    registered: bool,
}

fn poll_active(state: &PollState) -> bool {
    state.active
}

fn duplicate_nonblocking<F: AsFd>(fd: &F) -> crate::net::NetResult<OwnedFd> {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
    let duplicate = rustix::io::dup(fd).map_err(crate::net::errno_error)?;
    fcntl_setfl(
        &duplicate,
        fcntl_getfl(&duplicate).map_err(crate::net::errno_error)? | OFlags::NONBLOCK,
    )
    .map_err(crate::net::errno_error)?;
    Ok(duplicate)
}

/// A readiness-poll handle registered with the owning loop.
///
/// See `uv_poll_t` in `runtime/doc/luvref.txt` (lines 1115-1210).
pub struct Poll {
    id: HandleId,
    token: Token,
    state: Rc<RefCell<PollState>>,
    _callback: PollCallback,
}

impl std::fmt::Debug for Poll {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Poll").field("id", &self.id).finish()
    }
}

impl Poll {
    /// Wraps `fd` as a poll handle, duplicating it and setting it non-blocking.
    ///
    /// See `uv.new_poll()` in `runtime/doc/luvref.txt`. The caller remains
    /// responsible for the original descriptor; it may be closed immediately
    /// after `poll_stop` or `close`.
    pub fn new<F, C>(uv_loop: &mut UvLoop, fd: F, callback: C) -> crate::net::NetResult<Self>
    where
        F: AsFd,
        C: FnMut(&mut UvLoop, HandleId, PollEvents) + 'static,
    {
        Self::wrap(
            uv_loop,
            duplicate_nonblocking(&fd)?,
            POLL_READABLE | POLL_WRITABLE,
            Rc::new(RefCell::new(Some(Box::new(callback)))),
        )
    }

    /// Initializes a poll handle from a socket descriptor.
    ///
    /// See `uv.new_socket_poll()` in `runtime/doc/luvref.txt` (lines
    /// 1156-1167). On Unix this is identical to [`Poll::new`].
    pub fn new_socket<F, C>(uv_loop: &mut UvLoop, fd: F, callback: C) -> crate::net::NetResult<Self>
    where
        F: AsFd,
        C: FnMut(&mut UvLoop, HandleId, PollEvents) + 'static,
    {
        Self::new(uv_loop, fd, callback)
    }

    fn wrap(uv_loop: &mut UvLoop, fd: OwnedFd, mask: u8, callback: PollCallback) -> crate::net::NetResult<Self> {
        let id = uv_loop.allocate_external(false)?;
        let token = uv_loop.allocate_io_token()?;
        let state = Rc::new(RefCell::new(PollState {
            fd: Some(fd),
            mask,
            active: false,
            registered: false,
        }));
        register_poll(uv_loop, id, token, &state, &callback)?;
        Ok(Self { id, token, state, _callback: callback })
    }

    /// Starts polling with an `events` mask, firing `callback` on readiness.
    ///
    /// Calling start on an already-active handle updates the watched mask.
    /// See `uv.poll_start()` in `runtime/doc/luvref.txt` (lines 1169-1196).
    pub fn poll_start(&mut self, uv_loop: &mut UvLoop, events: &str) -> crate::net::NetResult<()> {
        let mask = poll_start_mask(events).ok_or_else(|| crate::net::NetError::InvalidState("invalid poll events mask"))?;
        {
            let mut state = self.state.borrow_mut();
            state.mask = mask;
            state.active = true;
            if !state.registered {
                let fd = state.fd.as_ref().ok_or(crate::net::NetError::Closed)?.as_raw_fd();
                uv_loop.inner_mut().reactor().register(&mut SourceFd(&fd), self.token, poll_interest(mask))?;
                state.registered = true;
            }
        }
        sync_poll(uv_loop, self.id, self.token, &self.state)
    }

    /// Stops polling the file descriptor.
    ///
    /// See `uv.poll_stop()` in `runtime/doc/luvref.txt` (lines 1198-1208).
    pub fn poll_stop(&mut self, uv_loop: &mut UvLoop) -> crate::net::NetResult<()> {
        {
            let mut state = self.state.borrow_mut();
            state.active = false;
            if state.registered {
                if let Some(fd) = state.fd.as_ref() {
                    let raw = fd.as_raw_fd();
                    let _ = uv_loop.inner_mut().reactor().deregister(&mut SourceFd(&raw));
                }
                state.registered = false;
            }
        }
        uv_loop.set_external_active(self.id, false)?;
        Ok(())
    }
}

fn register_poll(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: &Rc<RefCell<PollState>>,
    callback: &PollCallback,
) -> crate::net::NetResult<()> {
    // Registration is deferred until `poll_start` so the descriptor remains
    // unregistered until polling begins, matching libuv.
    let shared = Rc::clone(state);
    let user_callback = Rc::clone(callback);
    let queue = uv_loop.net_dispatch_queue();
    uv_loop.inner_mut().on_readiness(token, move |ready, _| {
        let (fired, active) = {
            let state = shared.borrow();
            (fired_events(state.mask, ready), state.active)
        };
        if active && fired != 0 {
            let dispatch_state = Rc::clone(&shared);
            let dispatch_callback = Rc::clone(&user_callback);
            let dispatch_ready = PollEvents(fired);
            queue_batch(&queue, move |uv_loop| {
                deliver_poll(uv_loop, id, token, &dispatch_state, &dispatch_callback, dispatch_ready)
            });
        }
        Ok(DrainState::Drained)
    })
    .map_err(crate::Error::from)?;
    Ok(())
}

fn sync_poll(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: &Rc<RefCell<PollState>>,
) -> crate::net::NetResult<()> {
    if !crate::net::live(uv_loop, id) {
        return Ok(());
    }
    {
        let state = state.borrow_mut();
        let interests = poll_interest(state.mask);
        if state.registered {
            if let Some(fd) = state.fd.as_ref() {
                let raw = fd.as_raw_fd();
                uv_loop.inner_mut().reactor().reregister(&mut SourceFd(&raw), token, interests)?;
            }
        }
    }
    uv_loop.set_external_active(id, poll_active(&state.borrow()))?;
    Ok(())
}

fn deliver_poll(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: &Rc<RefCell<PollState>>,
    callback: &PollCallback,
    events: PollEvents,
) {
    if !crate::net::live(uv_loop, id) {
        return;
    }
    if let Err(error) = sync_poll(uv_loop, id, token, state) {
        invoke_poll(callback, uv_loop, id, PollEvents(POLL_READABLE));
        let _ = error;
        return;
    }
    invoke_poll(callback, uv_loop, id, events);
}

fn invoke_poll(callback: &PollCallback, uv_loop: &mut UvLoop, id: HandleId, events: PollEvents) {
    if !crate::net::live(uv_loop, id) {
        return;
    }
    let taken = callback.borrow_mut().take();
    let Some(mut handler) = taken else { return };
    handler(uv_loop, id, events);
    let mut slot = callback.borrow_mut();
    if slot.is_none() {
        *slot = Some(handler);
    }
}

fn close_poll(uv_loop: &mut UvLoop, handle: &Poll) -> crate::Result<()> {
    uv_loop.inner_mut().remove_readiness(handle.token);
    let mut state = handle.state.borrow_mut();
    if state.registered {
        if let Some(fd) = state.fd.as_ref() {
            let raw = fd.as_raw_fd();
            let _ = uv_loop.inner_mut().reactor().deregister(&mut SourceFd(&raw));
        }
        state.registered = false;
    }
    state.fd = None;
    state.active = false;
    Ok(())
}

impl Handle for Poll {
    fn id(&self) -> HandleId {
        self.id
    }
    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        close_poll(uv_loop, self)?;
        uv_loop.close(self.id, None::<fn(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError>>)
    }
    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where
        F: FnOnce(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError> + 'static,
    {
        close_poll(uv_loop, self)?;
        uv_loop.close(self.id, Some(callback))
    }
}