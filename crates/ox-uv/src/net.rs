//! UvLoop-owned non-blocking TCP, Unix pipe, terminal, and UDP handles.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use mio::net::{TcpListener, TcpStream, UdpSocket};
use mio::{Interest, Token};
use ox_loop::{DrainState, Readiness};

use crate::handle::Handle;
use crate::uv_loop::NetDispatchQueue;
use crate::{CallbackError, HandleId, UvLoop};

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use mio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use mio::unix::SourceFd;
#[cfg(unix)]
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
#[cfg(unix)]
use rustix::net::{AddressFamily, SocketType};
#[cfg(unix)]
use rustix::termios::{
    LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcgetwinsize, tcsetattr,
};

pub(crate) const STREAM_CHUNK: usize = 64 * 1024;
const DATAGRAM_CHUNK: usize = 65_536;

/// A result type for network operations. See `uv` error conventions in `runtime/doc/luvref.txt`.
pub type NetResult<T> = Result<T, NetError>;

/// Errors raised by network handles and requests. See `uv` error conventions in `runtime/doc/luvref.txt`.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// An operating-system I/O error occurred.
    #[error("network I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The owning loop reported an error.
    #[error(transparent)]
    Loop(#[from] crate::Error),
    /// The handle is in the wrong state for the request.
    #[error("invalid network handle state: {0}")]
    InvalidState(&'static str),
    /// The handle has already been closed.
    #[error("network handle is closed")]
    Closed,
    /// The request identity space is exhausted.
    #[error("network request identity space exhausted")]
    RequestLimit,
    /// The operation is not supported on this platform.
    #[error("unsupported network operation: {0}")]
    Unsupported(&'static str),
}

impl From<ox_loop::Error> for NetError {
    fn from(error: ox_loop::Error) -> Self {
        Self::Loop(crate::Error::Loop(error))
    }
}

/// Opaque identifier for a queued write or send request. See `uv.write()` and `uv.udp_send()` in `runtime/doc/luvref.txt`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WriteId(u64);

impl WriteId {
    /// Returns the numeric request identifier.
    pub fn get(self) -> u64 { self.0 }
}

/// Event delivered by a network handle callback. See `uv.read_start()`, `uv.write()`, and `uv.udp_recv_start()` in `runtime/doc/luvref.txt`.
pub enum NetEvent {
    /// A listening TCP socket accepted a new stream.
    AcceptedTcp(Box<Tcp>),
    #[cfg(unix)]
    /// A listening pipe accepted a new stream.
    AcceptedPipe(Box<Pipe>),
    /// A connection attempt completed.
    Connected(NetResult<()>),
    /// Bytes were read from the stream.
    Read(Vec<u8>),
    /// The stream reached end-of-file.
    Eof,
    /// A queued write or send completed.
    WriteComplete {
        /// Identifier returned by `write` or `send`.
        id: WriteId,
        /// Outcome of the write or send.
        result: NetResult<()>,
    },
    /// A stream shutdown completed.
    ShutdownComplete(NetResult<()>),
    /// A UDP datagram was received.
    Datagram {
        /// Datagram payload.
        data: Vec<u8>,
        /// Source address.
        from: SocketAddr,
    },
    /// An error occurred on the handle.
    Error(NetError),
}

/// Loop callback invoked with handle events. See `uv.read_start()` and `uv.udp_recv_start()` in `runtime/doc/luvref.txt`.
pub type NetCallback = Box<dyn FnMut(&mut UvLoop, HandleId, NetEvent)>;
pub(crate) type CallbackCell = Rc<RefCell<Option<NetCallback>>>;

pub(crate) struct PendingWrite {
    pub(crate) id: WriteId,
    pub(crate) data: Vec<u8>,
    pub(crate) offset: usize,
}

pub(crate) struct WriteQueue {
    next_id: u64,
    pub(crate) pending: VecDeque<PendingWrite>,
    shutdown_requested: bool,
}

impl WriteQueue {
    pub(crate) fn new() -> Self {
        Self { next_id: 0, pending: VecDeque::new(), shutdown_requested: false }
    }

    pub(crate) fn push(&mut self, data: Vec<u8>) -> NetResult<WriteId> {
        let id = WriteId(self.next_id);
        self.next_id = self.next_id.checked_add(1).ok_or(NetError::RequestLimit)?;
        self.pending.push_back(PendingWrite { id, data, offset: 0 });
        Ok(id)
    }

    pub(crate) fn wants_write(&self) -> bool {
        !self.pending.is_empty() || self.shutdown_requested
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.shutdown_requested = false;
    }
}

pub(crate) fn is_would_block(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
}

pub(crate) fn drive_writes<W: Write>(writer: &mut W, queue: &mut WriteQueue, events: &mut Vec<NetEvent>) {
    loop {
        let Some(write) = queue.pending.front_mut() else { break };
        if write.offset == write.data.len() {
            let id = write.id;
            queue.pending.pop_front();
            events.push(NetEvent::WriteComplete { id, result: Ok(()) });
            continue;
        }
        match writer.write(&write.data[write.offset..]) {
            Ok(0) => {
                let id = write.id;
                queue.pending.pop_front();
                events.push(NetEvent::WriteComplete {
                    id,
                    result: Err(NetError::Io(io::Error::from(io::ErrorKind::WriteZero))),
                });
            }
            Ok(written) => write.offset += written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_would_block(&error) => break,
            Err(error) => {
                let id = write.id;
                queue.pending.pop_front();
                events.push(NetEvent::WriteComplete { id, result: Err(NetError::Io(error)) });
            }
        }
    }
}

pub(crate) fn drain_reads<R: Read>(reader: &mut R, events: &mut Vec<NetEvent>) {
    loop {
        let mut data = vec![0; STREAM_CHUNK];
        match reader.read(&mut data) {
            Ok(0) => { events.push(NetEvent::Eof); break; }
            Ok(read) => { data.truncate(read); events.push(NetEvent::Read(data)); }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_would_block(&error) => break,
            Err(error) => { events.push(NetEvent::Error(NetError::Io(error))); break; }
        }
    }
}

pub(crate) fn interest(readable: bool, writable: bool) -> Interest {
    match (readable, writable) {
        (true, true) => Interest::READABLE.add(Interest::WRITABLE),
        (false, true) => Interest::WRITABLE,
        _ => Interest::READABLE,
    }
}

pub(crate) fn live(uv_loop: &UvLoop, id: HandleId) -> bool {
    uv_loop.state(id).is_some() && !uv_loop.is_closing(id)
}

pub(crate) fn invoke(callback: &CallbackCell, uv_loop: &mut UvLoop, id: HandleId, event: NetEvent) {
    if !live(uv_loop, id) { return; }
    let taken = callback.borrow_mut().take();
    let Some(mut user_callback) = taken else { return };
    user_callback(uv_loop, id, event);
    let mut slot = callback.borrow_mut();
    if slot.is_none() { *slot = Some(user_callback); }
}

pub(crate) fn queue_batch(queue: &NetDispatchQueue, dispatch: impl FnOnce(&mut UvLoop) + 'static) {
    queue.borrow_mut().push_back(Box::new(dispatch));
}

fn close_id<F>(uv_loop: &mut UvLoop, id: HandleId, callback: Option<F>) -> crate::Result<()>
where
    F: FnOnce(&mut UvLoop, HandleId) -> Result<(), CallbackError> + 'static,
{
    uv_loop.close(id, callback)
}

enum TcpIo { Listener(TcpListener), Stream(TcpStream) }

enum TcpReadyEvent { Public(NetEvent), Accepted(TcpStream) }

struct TcpState {
    io: Option<TcpIo>,
    listening: bool,
    reading: bool,
    connecting: bool,
    writes: WriteQueue,
    registered: bool,
}

/// TCP stream or server handle. See `uv_tcp_t` in `runtime/doc/luvref.txt`.
pub struct Tcp {
    id: HandleId,
    token: Token,
    state: Rc<RefCell<TcpState>>,
    _callback: CallbackCell,
}

impl Tcp {
    /// Binds a TCP socket to `address`. See `uv.tcp_bind()` in `runtime/doc/luvref.txt`.
    pub fn bind<F>(uv_loop: &mut UvLoop, address: SocketAddr, callback: F) -> NetResult<Self>
    where F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static,
    {
        Self::attach(uv_loop, TcpState {
            io: Some(TcpIo::Listener(TcpListener::bind(address)?)), listening: false,
            reading: false, connecting: false, writes: WriteQueue::new(), registered: false,
        }, Rc::new(RefCell::new(Some(Box::new(callback)))))
    }

    /// Connects a TCP socket to `address`. See `uv.tcp_connect()` in `runtime/doc/luvref.txt`.
    pub fn connect<F>(uv_loop: &mut UvLoop, address: SocketAddr, callback: F) -> NetResult<Self>
    where F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static,
    {
        Self::attach(uv_loop, TcpState {
            io: Some(TcpIo::Stream(TcpStream::connect(address)?)), listening: false,
            reading: false, connecting: true, writes: WriteQueue::new(), registered: false,
        }, Rc::new(RefCell::new(Some(Box::new(callback)))))
    }

    fn attach(uv_loop: &mut UvLoop, state: TcpState, callback: CallbackCell) -> NetResult<Self> {
        let id = uv_loop.allocate_external(state.connecting)?;
        let token = uv_loop.allocate_io_token()?;
        let state = Rc::new(RefCell::new(state));
        register_tcp(uv_loop, id, token, &state, &callback)?;
        Ok(Self { id, token, state, _callback: callback })
    }

    /// Starts listening for incoming connections with `backlog` slots. See `uv.listen()` in `runtime/doc/luvref.txt`.
    pub fn listen(&mut self, uv_loop: &mut UvLoop, backlog: u32) -> NetResult<()> {
        if backlog == 0 { return Err(NetError::InvalidState("listen backlog must be nonzero")); }
        {
            let mut state = self.state.borrow_mut();
            match state.io {
                Some(TcpIo::Listener(_)) => state.listening = true,
                Some(TcpIo::Stream(_)) => return Err(NetError::InvalidState("TCP stream cannot listen")),
                None => return Err(NetError::Closed),
            }
        }
        sync_tcp(uv_loop, self.id, self.token, &self.state)
    }

    /// Starts reading bytes from the stream. See `uv.read_start()` in `runtime/doc/luvref.txt`.
    pub fn read_start(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        {
            let mut state = self.state.borrow_mut();
            match state.io {
                Some(TcpIo::Stream(_)) => state.reading = true,
                Some(TcpIo::Listener(_)) => return Err(NetError::InvalidState("TCP listener cannot read bytes")),
                None => return Err(NetError::Closed),
            }
        }
        sync_tcp(uv_loop, self.id, self.token, &self.state)
    }

    /// Stops reading bytes from the stream. See `uv.read_stop()` in `runtime/doc/luvref.txt`.
    pub fn read_stop(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        { let mut state = self.state.borrow_mut(); if state.io.is_none() { return Err(NetError::Closed); } state.reading = false; }
        sync_tcp(uv_loop, self.id, self.token, &self.state)
    }

    /// Queues `data` to be written. See `uv.write()` in `runtime/doc/luvref.txt`.
    pub fn write(&mut self, uv_loop: &mut UvLoop, data: Vec<u8>) -> NetResult<WriteId> {
        let id = {
            let mut state = self.state.borrow_mut();
            if !matches!(state.io, Some(TcpIo::Stream(_))) { return if state.io.is_none() { Err(NetError::Closed) } else { Err(NetError::InvalidState("TCP listener cannot write")) }; }
            state.writes.push(data)?
        };
        sync_tcp(uv_loop, self.id, self.token, &self.state)?;
        Ok(id)
    }

    /// Shuts down the write side of the stream. See `uv.shutdown()` in `runtime/doc/luvref.txt`.
    pub fn shutdown(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        {
            let mut state = self.state.borrow_mut();
            if !matches!(state.io, Some(TcpIo::Stream(_))) { return if state.io.is_none() { Err(NetError::Closed) } else { Err(NetError::InvalidState("TCP listener cannot shut down writes")) }; }
            state.writes.shutdown_requested = true;
        }
        sync_tcp(uv_loop, self.id, self.token, &self.state)
    }

    /// Enables or disables TCP_NODELAY. See `uv.tcp_nodelay()` in `runtime/doc/luvref.txt`.
    pub fn nodelay(&self, enable: bool) -> NetResult<()> {
        let state = self.state.borrow();
        let Some(TcpIo::Stream(stream)) = state.io.as_ref() else { return if state.io.is_none() { Err(NetError::Closed) } else { Err(NetError::InvalidState("TCP listener has no nodelay option")) }; };
        stream.set_nodelay(enable).map_err(NetError::Io)
    }

    #[cfg(unix)]
    /// Configures TCP keepalive timers. See `uv.tcp_keepalive()` in `runtime/doc/luvref.txt`.
    pub fn keepalive(&self, enable: bool, delay: Option<Duration>, interval: Option<Duration>, count: Option<u32>) -> NetResult<()> {
        let state = self.state.borrow();
        let Some(TcpIo::Stream(stream)) = state.io.as_ref() else { return if state.io.is_none() { Err(NetError::Closed) } else { Err(NetError::InvalidState("TCP listener has no keepalive option")) }; };
        rustix::net::sockopt::set_socket_keepalive(stream, enable).map_err(errno_error)?;
        if enable {
            if let Some(delay) = delay { rustix::net::sockopt::set_tcp_keepidle(stream, delay).map_err(errno_error)?; }
            if let Some(interval) = interval { rustix::net::sockopt::set_tcp_keepintvl(stream, interval).map_err(errno_error)?; }
            if let Some(count) = count { rustix::net::sockopt::set_tcp_keepcnt(stream, count).map_err(errno_error)?; }
        }
        Ok(())
    }

    /// Returns the local socket address. See `uv.tcp_getsockname()` in `runtime/doc/luvref.txt`.
    pub fn local_addr(&self) -> NetResult<SocketAddr> {
        match self.state.borrow().io.as_ref() {
            Some(TcpIo::Listener(listener)) => listener.local_addr().map_err(NetError::Io),
            Some(TcpIo::Stream(stream)) => stream.local_addr().map_err(NetError::Io),
            None => Err(NetError::Closed),
        }
    }

    /// Returns the peer socket address. See `uv.tcp_getpeername()` in `runtime/doc/luvref.txt`.
    pub fn peer_addr(&self) -> NetResult<SocketAddr> {
        match self.state.borrow().io.as_ref() {
            Some(TcpIo::Stream(stream)) => stream.peer_addr().map_err(NetError::Io),
            Some(TcpIo::Listener(_)) => Err(NetError::InvalidState("TCP listener has no peer")),
            None => Err(NetError::Closed),
        }
    }
}

fn register_tcp(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<TcpState>>, callback: &CallbackCell) -> NetResult<()> {
    {
        let mut state = state.borrow_mut();
        let interests = tcp_interest(&state);
        let io = state.io.as_mut().ok_or(NetError::Closed)?;
        match io {
            TcpIo::Listener(source) => uv_loop.inner_mut().reactor().register(source, token, interests)?,
            TcpIo::Stream(source) => uv_loop.inner_mut().reactor().register(source, token, interests)?,
        }
        state.registered = true;
    }
    let shared = Rc::clone(state);
    let user_callback = Rc::clone(callback);
    let queue = uv_loop.net_dispatch_queue();
    if let Err(error) = uv_loop.inner_mut().on_readiness(token, move |ready, _| {
        let events = tcp_ready(&mut shared.borrow_mut(), ready);
        if !events.is_empty() {
            let dispatch_state = Rc::clone(&shared);
            let dispatch_callback = Rc::clone(&user_callback);
            queue_batch(&queue, move |uv_loop| deliver_tcp(uv_loop, id, token, dispatch_state, dispatch_callback, events));
        }
        Ok(DrainState::Drained)
    }) {
        let mut state = state.borrow_mut();
        if let Some(io) = state.io.as_mut() {
            match io {
                TcpIo::Listener(source) => { let _ = uv_loop.inner_mut().reactor().deregister(source); }
                TcpIo::Stream(source) => { let _ = uv_loop.inner_mut().reactor().deregister(source); }
            }
        }
        state.registered = false;
        return Err(crate::Error::from(error).into());
    }
    Ok(())
}

fn tcp_interest(state: &TcpState) -> Interest {
    match state.io {
        Some(TcpIo::Listener(_)) => Interest::READABLE,
        Some(TcpIo::Stream(_)) => interest(state.reading, state.connecting || state.writes.wants_write()),
        None => Interest::READABLE,
    }
}

fn tcp_active(state: &TcpState) -> bool {
    state.listening || state.reading || state.connecting || state.writes.wants_write()
}

fn sync_tcp(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<TcpState>>) -> NetResult<()> {
    if !live(uv_loop, id) { return Ok(()); }
    let active;
    {
        let mut state = state.borrow_mut();
        active = tcp_active(&state);
        let interests = tcp_interest(&state);
        if state.registered {
            match state.io.as_mut().ok_or(NetError::Closed)? {
                TcpIo::Listener(source) => uv_loop.inner_mut().reactor().reregister(source, token, interests)?,
                TcpIo::Stream(source) => uv_loop.inner_mut().reactor().reregister(source, token, interests)?,
            }
        }
    }
    uv_loop.set_external_active(id, active)?;
    Ok(())
}

fn tcp_ready(state: &mut TcpState, ready: Readiness) -> Vec<TcpReadyEvent> {
    let mut events = Vec::new();
    if (ready.writable || ready.error) && state.connecting {
        let result = match state.io.as_ref() {
            Some(TcpIo::Stream(stream)) => match stream.take_error() {
                Ok(Some(error)) => Err(NetError::Io(error)),
                Ok(None) => stream.peer_addr().map(|_| ()).map_err(NetError::Io),
                Err(error) => Err(NetError::Io(error)),
            },
            _ => Err(NetError::Closed),
        };
        state.connecting = false;
        events.push(TcpReadyEvent::Public(NetEvent::Connected(result)));
    }
    match state.io.as_mut() {
        Some(TcpIo::Listener(listener)) if ready.readable && state.listening => loop {
            match listener.accept() {
                Ok((stream, _)) => events.push(TcpReadyEvent::Accepted(stream)),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if is_would_block(&error) => break,
                Err(error) => { events.push(TcpReadyEvent::Public(NetEvent::Error(NetError::Io(error)))); break; }
            }
        },
        Some(TcpIo::Stream(stream)) => {
            let mut public = Vec::new();
            if ready.writable {
                drive_writes(stream, &mut state.writes, &mut public);
                if state.writes.shutdown_requested && state.writes.pending.is_empty() {
                    state.writes.shutdown_requested = false;
                    public.push(NetEvent::ShutdownComplete(stream.shutdown(Shutdown::Write).map_err(NetError::Io)));
                }
            }
            if ready.readable && state.reading {
                drain_reads(stream, &mut public);
                if public.iter().any(|event| matches!(event, NetEvent::Eof)) { state.reading = false; }
            }
            if ready.read_closed && state.reading { state.reading = false; public.push(NetEvent::Eof); }
            if ready.write_closed && !state.writes.pending.is_empty() {
                while let Some(write) = state.writes.pending.pop_front() {
                    public.push(NetEvent::WriteComplete { id: write.id, result: Err(NetError::Closed) });
                }
            }
            events.extend(public.into_iter().map(TcpReadyEvent::Public));
        }
        _ => {}
    }
    events
}

fn deliver_tcp(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: Rc<RefCell<TcpState>>, callback: CallbackCell, events: Vec<TcpReadyEvent>) {
    if !live(uv_loop, id) { return; }
    if let Err(error) = sync_tcp(uv_loop, id, token, &state) { invoke(&callback, uv_loop, id, NetEvent::Error(error)); }
    for event in events {
        if !live(uv_loop, id) { break; }
        let event = match event {
            TcpReadyEvent::Public(event) => event,
            TcpReadyEvent::Accepted(stream) => {
                let child_state = TcpState { io: Some(TcpIo::Stream(stream)), listening: false, reading: false, connecting: false, writes: WriteQueue::new(), registered: false };
                match Tcp::attach(uv_loop, child_state, Rc::clone(&callback)) {
                    Ok(child) => NetEvent::AcceptedTcp(Box::new(child)),
                    Err(error) => NetEvent::Error(error),
                }
            }
        };
        invoke(&callback, uv_loop, id, event);
    }
}

fn close_tcp(uv_loop: &mut UvLoop, handle: &Tcp) -> crate::Result<()> {
    uv_loop.inner_mut().remove_readiness(handle.token);
    let mut state = handle.state.borrow_mut();
    if state.registered {
        if let Some(io) = state.io.as_mut() {
            match io {
                TcpIo::Listener(source) => uv_loop.inner_mut().reactor().deregister(source)?,
                TcpIo::Stream(source) => uv_loop.inner_mut().reactor().deregister(source)?,
            }
        }
        state.registered = false;
    }
    state.io = None;
    state.listening = false;
    state.reading = false;
    state.connecting = false;
    state.writes.clear();
    Ok(())
}

impl Handle for Tcp {
    fn id(&self) -> HandleId { self.id }
    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        close_tcp(uv_loop, self)?;
        close_id(uv_loop, self.id, None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>)
    }
    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where F: FnOnce(&mut UvLoop, HandleId) -> Result<(), CallbackError> + 'static {
        close_tcp(uv_loop, self)?;
        close_id(uv_loop, self.id, Some(callback))
    }
}

#[cfg(unix)]
pub(crate) fn errno_error(error: rustix::io::Errno) -> NetError {
    NetError::Io(io::Error::from_raw_os_error(error.raw_os_error()))
}

#[cfg(unix)]
enum PipeIo { Listener(UnixListener), Stream(UnixStream) }
#[cfg(unix)]
enum PipeReadyEvent { Public(NetEvent), Accepted(UnixStream) }

/// The kind of stream handle passed by [`Pipe::write2`].
///
/// Matches the `uv.write2()` contract in `runtime/doc/luvref.txt` (lines
/// 1632-1666): `send_handle` must be a TCP socket or a pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeHandleKind {
    /// A TCP stream (listening or connected).
    Tcp,
    /// A Unix domain socket / named pipe.
    Pipe,
}

impl PipeHandleKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Pipe => "pipe",
        }
    }
}

/// Derives the luv-style handle type ("pipe" or "tcp") from an open file
/// descriptor by inspecting its `stat` mode, socket type, and address family.
///
/// FIFOs and Unix domain sockets (`AF_UNIX` + `SOCK_STREAM`) resolve to
/// `"pipe"`; TCP sockets (`AF_INET`/`AF_INET6` + `SOCK_STREAM`) resolve to
/// `"tcp"`. Any other file type is rejected.
#[cfg(unix)]
fn inspect_fd_kind<Fd: AsFd>(fd: &Fd) -> NetResult<&'static str> {
    let stat = rustix::fs::fstat(fd).map_err(errno_error)?;
    let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    if file_type.is_fifo() {
        return Ok("pipe");
    }
    if !file_type.is_socket() {
        return Err(NetError::Unsupported(
            "write2 send_handle must be a TCP socket or pipe",
        ));
    }
    let sock_type = rustix::net::sockopt::socket_type(fd).map_err(errno_error)?;
    if sock_type != SocketType::STREAM {
        return Err(NetError::Unsupported(
            "write2 send_handle must be a TCP socket or pipe",
        ));
    }
    let domain = rustix::net::getsockname(fd)
        .map_err(errno_error)?
        .address_family();
    if domain == AddressFamily::UNIX {
        return Ok("pipe");
    }
    if domain == AddressFamily::INET || domain == AddressFamily::INET6 {
        return Ok("tcp");
    }
    Err(NetError::Unsupported(
        "write2 send_handle must be a TCP socket or pipe",
    ))
}

#[cfg(unix)]
struct PipePending {
    fd: std::os::fd::OwnedFd,
    kind: &'static str,
}

#[cfg(unix)]
struct PipeState {
    io: Option<PipeIo>,
    path: Option<PathBuf>,
    listening: bool,
    reading: bool,
    connecting: bool,
    writes: WriteQueue,
    registered: bool,
    ipc: bool,
    pending: VecDeque<PipePending>,
    pending_instances: u32,
}

#[cfg(unix)]
/// Unix domain socket or named pipe handle. See `uv_pipe_t` in `runtime/doc/luvref.txt`.
pub struct Pipe {
    id: HandleId,
    token: Token,
    state: Rc<RefCell<PipeState>>,
    _callback: CallbackCell,
}

#[cfg(unix)]
impl Pipe {
    /// Binds a pipe to `path`. See `uv.pipe_bind()` in `runtime/doc/luvref.txt`.
    pub fn bind<F>(uv_loop: &mut UvLoop, path: impl AsRef<Path>, callback: F) -> NetResult<Self>
    where F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static {
        let path = path.as_ref().to_path_buf();
        Self::attach(uv_loop, PipeState { io: Some(PipeIo::Listener(UnixListener::bind(&path)?)), path: Some(path), listening: false, reading: false, connecting: false, writes: WriteQueue::new(), registered: false, ipc: false, pending: VecDeque::new(), pending_instances: 0 }, Rc::new(RefCell::new(Some(Box::new(callback)))))
    }

    /// Connects a pipe to `path`. See `uv.pipe_connect()` in `runtime/doc/luvref.txt`.
    pub fn connect<F>(uv_loop: &mut UvLoop, path: impl AsRef<Path>, callback: F) -> NetResult<Self>
    where F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static {
        Self::attach(uv_loop, PipeState { io: Some(PipeIo::Stream(UnixStream::connect(path.as_ref())?)), path: None, listening: false, reading: false, connecting: true, writes: WriteQueue::new(), registered: false, ipc: false, pending: VecDeque::new(), pending_instances: 0 }, Rc::new(RefCell::new(Some(Box::new(callback)))))
    }

    fn attach(uv_loop: &mut UvLoop, state: PipeState, callback: CallbackCell) -> NetResult<Self> {
        let id = uv_loop.allocate_external(state.connecting)?;
        let token = uv_loop.allocate_io_token()?;
        let state = Rc::new(RefCell::new(state));
        register_pipe(uv_loop, id, token, &state, &callback)?;
        Ok(Self { id, token, state, _callback: callback })
    }

    /// Starts listening for incoming connections with `backlog` slots. See `uv.listen()` in `runtime/doc/luvref.txt`.
    pub fn listen(&mut self, uv_loop: &mut UvLoop, backlog: u32) -> NetResult<()> {
        if backlog == 0 { return Err(NetError::InvalidState("listen backlog must be nonzero")); }
        { let mut state = self.state.borrow_mut(); match state.io { Some(PipeIo::Listener(_)) => state.listening = true, Some(PipeIo::Stream(_)) => return Err(NetError::InvalidState("connected pipe cannot listen")), None => return Err(NetError::Closed) } }
        sync_pipe(uv_loop, self.id, self.token, &self.state)
    }

    /// Starts reading bytes from the stream. See `uv.read_start()` in `runtime/doc/luvref.txt`.
    pub fn read_start(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        { let mut state = self.state.borrow_mut(); match state.io { Some(PipeIo::Stream(_)) => state.reading = true, Some(PipeIo::Listener(_)) => return Err(NetError::InvalidState("pipe listener cannot read bytes")), None => return Err(NetError::Closed) } }
        sync_pipe(uv_loop, self.id, self.token, &self.state)
    }

    /// Stops reading bytes from the stream. See `uv.read_stop()` in `runtime/doc/luvref.txt`.
    pub fn read_stop(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        { let mut state = self.state.borrow_mut(); if state.io.is_none() { return Err(NetError::Closed); } state.reading = false; }
        sync_pipe(uv_loop, self.id, self.token, &self.state)
    }

    /// Queues `data` to be written. See `uv.write()` in `runtime/doc/luvref.txt`.
    pub fn write(&mut self, uv_loop: &mut UvLoop, data: Vec<u8>) -> NetResult<WriteId> {
        let id = { let mut state = self.state.borrow_mut(); if !matches!(state.io, Some(PipeIo::Stream(_))) { return if state.io.is_none() { Err(NetError::Closed) } else { Err(NetError::InvalidState("pipe listener cannot write")) }; } state.writes.push(data)? };
        sync_pipe(uv_loop, self.id, self.token, &self.state)?;
        Ok(id)
    }

    /// Shuts down the write side of the stream. See `uv.shutdown()` in `runtime/doc/luvref.txt`.
    pub fn shutdown(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        { let mut state = self.state.borrow_mut(); if !matches!(state.io, Some(PipeIo::Stream(_))) { return if state.io.is_none() { Err(NetError::Closed) } else { Err(NetError::InvalidState("pipe listener cannot shut down writes")) }; } state.writes.shutdown_requested = true; }
        sync_pipe(uv_loop, self.id, self.token, &self.state)
    }

    /// Returns the bound path, if any. See `uv.pipe_getsockname()` in `runtime/doc/luvref.txt`.
    pub fn local_name(&self) -> NetResult<Option<PathBuf>> {
        match self.state.borrow().io.as_ref() {
            Some(PipeIo::Listener(listener)) => Ok(listener.local_addr()?.as_pathname().map(Path::to_path_buf)),
            Some(PipeIo::Stream(stream)) => Ok(stream.local_addr()?.as_pathname().map(Path::to_path_buf)),
            None => Err(NetError::Closed),
        }
    }

    /// Returns the peer path, if any. See `uv.pipe_getpeername()` in `runtime/doc/luvref.txt`.
    pub fn peer_name(&self) -> NetResult<Option<PathBuf>> {
        match self.state.borrow().io.as_ref() {
            Some(PipeIo::Stream(stream)) => Ok(stream.peer_addr()?.as_pathname().map(Path::to_path_buf)),
            Some(PipeIo::Listener(_)) => Err(NetError::InvalidState("pipe listener has no peer")),
            None => Err(NetError::Closed),
        }
    }

    /// Alters pipe permissions. See `uv.pipe_chmod()` in `runtime/doc/luvref.txt`.
    pub fn chmod(&self, readable: bool, writable: bool) -> NetResult<()> {
        let state = self.state.borrow();
        let path = state.path.as_ref().ok_or(NetError::InvalidState("pipe has no bound pathname"))?;
        let metadata = std::fs::metadata(path)?;
        let mut mode = metadata.permissions().mode();
        if readable { mode |= 0o444; }
        if writable { mode |= 0o222; }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    /// Wraps an already-connected stream as an IPC pipe.
    ///
    /// `ipc` enables the `SCM_RIGHTS` receive track so `write2`'s counterpart
    /// can publish received descriptors for `pipe_pending_count`/`type`. This
    /// is the ox-uv spelling of the `uv.new_pipe(ipc)` + `uv.pipe_open(fd)`
    /// combination in `runtime/doc/luvref.txt` (lines 2007-2032).
    pub fn from_stream<F>(uv_loop: &mut UvLoop, stream: mio::net::UnixStream, ipc: bool, callback: F) -> NetResult<Self>
    where F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static {
        Self::attach(uv_loop, PipeState { io: Some(PipeIo::Stream(stream)), path: None, listening: false, reading: false, connecting: true, writes: WriteQueue::new(), registered: false, ipc, pending: VecDeque::new(), pending_instances: 0 }, Rc::new(RefCell::new(Some(Box::new(callback)))))
    }

    /// Sends `data` and passes `send_handle`'s descriptor over an IPC pipe.
    ///
    /// See `uv.write2()` in `runtime/doc/luvref.txt` (lines 1632-1666). The
    /// descriptor is delivered as an `SCM_RIGHTS` ancillary message alongside
    /// the payload via a single `sendmsg`. The pipe must have been created
    /// with the `ipc` option (`new_pipe(true)`), be a connected stream, and
    /// `kind` must name what `send_handle` actually is (`tcp` or `pipe` per
    /// luvref). The actual file descriptor is inspected before sending; a kind
    /// that does not match the fd's derived type is rejected with a typed
    /// error.
    ///
    /// Because a Unix `SCM_RIGHTS` transfer carries no type tag on the wire,
    /// the receiving side derives the pending handle type from the received fd
    /// itself (`fstat` + `getsockopt` `SO_TYPE` + `getsockname`).
    ///
    /// ox-uv performs the fd-passing synchronously rather than queueing a
    /// deferred `uv_write_t` request; the return value is the number of bytes
    /// written.
    pub fn write2<S: std::os::fd::AsFd>(
        &self,
        uv_loop: &mut UvLoop,
        data: Vec<u8>,
        send_handle: &S,
        kind: PipeHandleKind,
    ) -> NetResult<usize> {
        let _ = uv_loop;
        let state = self.state.borrow();
        if !state.ipc {
            return Err(NetError::InvalidState(
                "write2 requires an IPC pipe (create it with new_pipe(true))",
            ));
        }
        let Some(PipeIo::Stream(stream)) = state.io.as_ref() else {
            return if state.io.is_none() {
                Err(NetError::Closed)
            } else {
                Err(NetError::InvalidState("pipe listener cannot send handles"))
            };
        };
        let derived = inspect_fd_kind(send_handle)?;
        if derived != kind.as_str() {
            return Err(NetError::InvalidState(
                "send_handle kind does not match the actual file descriptor",
            ));
        }
        crate::ipc::send_handle(stream, &data, send_handle).map_err(NetError::Io)
    }

    /// Returns the number of handles received over an IPC pipe but not yet
    /// accepted. See `uv.pipe_pending_count()` in `runtime/doc/luvref.txt`
    /// (lines 2106-2115).
    pub fn pending_count(&self) -> usize {
        self.state.borrow().pending.len()
    }

    /// Returns the type of the next pending IPC handle.
    ///
    /// See `uv.pipe_pending_type()` in `runtime/doc/luvref.txt`
    /// (lines 2117-2130). The type is derived by inspecting the received file
    /// descriptor (`fstat` + `getsockopt` `SO_TYPE` + `getsockname`): FIFOs and
    /// Unix domain sockets report `"pipe"`; TCP sockets report `"tcp"`.
    pub fn pending_type(&self) -> Option<&'static str> {
        self.state.borrow().pending.front().map(|pending| pending.kind)
    }

    /// Sets the pipe's IPC pending-instance count.
    ///
    /// See `uv.pipe_pending_instances()` in `runtime/doc/luvref.txt`
    /// (lines 2091-2104). This setting applies to Windows only; ox-uv stores
    /// it for API compatibility and it has no runtime effect on Unix.
    pub fn pending_instances(&self, count: u32) {
        self.state.borrow_mut().pending_instances = count;
    }

    /// Takes the next pending IPC descriptor out of the queue.
    ///
    /// This is the ox-uv counterpart of libuv's `uv_accept`-based consumption
    /// of a pending handle: the received descriptor is removed from the
    /// pending queue and returned for the caller to re-wrap, because luv
    /// consumes pending handles through `accept`.
    pub fn pending_take_fd(&self) -> Option<std::os::fd::OwnedFd> {
        self.state.borrow_mut().pending.pop_front().map(|pending| pending.fd)
    }
}

#[cfg(unix)]
fn register_pipe(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<PipeState>>, callback: &CallbackCell) -> NetResult<()> {
    {
        let mut state = state.borrow_mut();
        let interests = pipe_interest(&state);
        match state.io.as_mut().ok_or(NetError::Closed)? {
            PipeIo::Listener(source) => uv_loop.inner_mut().reactor().register(source, token, interests)?,
            PipeIo::Stream(source) => uv_loop.inner_mut().reactor().register(source, token, interests)?,
        }
        state.registered = true;
    }
    let shared = Rc::clone(state);
    let user_callback = Rc::clone(callback);
    let queue = uv_loop.net_dispatch_queue();
    uv_loop.inner_mut().on_readiness(token, move |ready, _| {
        let events = pipe_ready(&mut shared.borrow_mut(), ready);
        if !events.is_empty() {
            let dispatch_state = Rc::clone(&shared);
            let dispatch_callback = Rc::clone(&user_callback);
            queue_batch(&queue, move |uv_loop| deliver_pipe(uv_loop, id, token, dispatch_state, dispatch_callback, events));
        }
        Ok(DrainState::Drained)
    }).map_err(crate::Error::from)?;
    Ok(())
}

#[cfg(unix)]
fn pipe_interest(state: &PipeState) -> Interest {
    match state.io { Some(PipeIo::Listener(_)) => Interest::READABLE, Some(PipeIo::Stream(_)) => interest(state.reading, state.connecting || state.writes.wants_write()), None => Interest::READABLE }
}
#[cfg(unix)]
fn pipe_active(state: &PipeState) -> bool { state.listening || state.reading || state.connecting || state.writes.wants_write() }

#[cfg(unix)]
fn sync_pipe(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<PipeState>>) -> NetResult<()> {
    if !live(uv_loop, id) { return Ok(()); }
    let active;
    {
        let mut state = state.borrow_mut(); active = pipe_active(&state); let interests = pipe_interest(&state);
        if state.registered { match state.io.as_mut().ok_or(NetError::Closed)? { PipeIo::Listener(source) => uv_loop.inner_mut().reactor().reregister(source, token, interests)?, PipeIo::Stream(source) => uv_loop.inner_mut().reactor().reregister(source, token, interests)? } }
    }
    uv_loop.set_external_active(id, active)?;
    Ok(())
}

#[cfg(unix)]
fn pipe_ready(state: &mut PipeState, ready: Readiness) -> Vec<PipeReadyEvent> {
    let mut events = Vec::new();
    let mut received: Vec<PipePending> = Vec::new();
    if (ready.writable || ready.error) && state.connecting {
        let result = match state.io.as_ref() { Some(PipeIo::Stream(stream)) => match stream.take_error() { Ok(Some(error)) => Err(NetError::Io(error)), Ok(None) => stream.peer_addr().map(|_| ()).map_err(NetError::Io), Err(error) => Err(NetError::Io(error)) }, _ => Err(NetError::Closed) };
        state.connecting = false; events.push(PipeReadyEvent::Public(NetEvent::Connected(result)));
    }
    match state.io.as_mut() {
        Some(PipeIo::Listener(listener)) if ready.readable && state.listening => loop {
            match listener.accept() { Ok((stream, _)) => events.push(PipeReadyEvent::Accepted(stream)), Err(error) if error.kind() == io::ErrorKind::Interrupted => {}, Err(error) if is_would_block(&error) => break, Err(error) => { events.push(PipeReadyEvent::Public(NetEvent::Error(NetError::Io(error)))); break; } }
        },
        Some(PipeIo::Stream(stream)) => {
            let mut public = Vec::new();
            if ready.writable { drive_writes(stream, &mut state.writes, &mut public); if state.writes.shutdown_requested && state.writes.pending.is_empty() { state.writes.shutdown_requested = false; public.push(NetEvent::ShutdownComplete(stream.shutdown(Shutdown::Write).map_err(NetError::Io))); } }
            if ready.readable && state.reading {
                if state.ipc {
                    let (ipc_events, fds) = drain_ipc_reads(stream);
                    public.extend(ipc_events);
                    for fd in fds {
                        match inspect_fd_kind(&fd) {
                            Ok(kind) => received.push(PipePending { fd, kind }),
                            Err(error) => public.push(NetEvent::Error(error)),
                        }
                    }
                } else {
                    drain_reads(stream, &mut public);
                }
                if public.iter().any(|event| matches!(event, NetEvent::Eof)) { state.reading = false; }
            }
            if ready.read_closed && state.reading { state.reading = false; public.push(NetEvent::Eof); }
            if ready.write_closed && !state.writes.pending.is_empty() { while let Some(write) = state.writes.pending.pop_front() { public.push(NetEvent::WriteComplete { id: write.id, result: Err(NetError::Closed) }); } }
            events.extend(public.into_iter().map(PipeReadyEvent::Public));
        }
        _ => {}
    }
    state.pending.extend(received);
    events
}

#[cfg(unix)]
fn drain_ipc_reads(stream: &UnixStream) -> (Vec<NetEvent>, Vec<std::os::fd::OwnedFd>) {
    let mut events = Vec::new();
    let mut fds = Vec::new();
    loop {
        match crate::ipc::recv_handle(stream, STREAM_CHUNK) {
            Ok((data, received)) => {
                if data.is_empty() && received.is_none() { events.push(NetEvent::Eof); break; }
                if let Some(fd) = received { fds.push(fd); }
                if !data.is_empty() { events.push(NetEvent::Read(data)); }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_would_block(&error) => break,
            Err(error) => { events.push(NetEvent::Error(NetError::Io(error))); break; }
        }
    }
    (events, fds)
}

#[cfg(unix)]
fn deliver_pipe(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: Rc<RefCell<PipeState>>, callback: CallbackCell, events: Vec<PipeReadyEvent>) {
    if !live(uv_loop, id) { return; }
    if let Err(error) = sync_pipe(uv_loop, id, token, &state) { invoke(&callback, uv_loop, id, NetEvent::Error(error)); }
    for event in events {
        if !live(uv_loop, id) { break; }
        let event = match event {
            PipeReadyEvent::Public(event) => event,
            PipeReadyEvent::Accepted(stream) => {
                let child_state = PipeState { io: Some(PipeIo::Stream(stream)), path: None, listening: false, reading: false, connecting: false, writes: WriteQueue::new(), registered: false, ipc: state.borrow().ipc, pending: VecDeque::new(), pending_instances: 0 };
                match Pipe::attach(uv_loop, child_state, Rc::clone(&callback)) { Ok(child) => NetEvent::AcceptedPipe(Box::new(child)), Err(error) => NetEvent::Error(error) }
            }
        };
        invoke(&callback, uv_loop, id, event);
    }
}

#[cfg(unix)]
fn close_pipe(uv_loop: &mut UvLoop, handle: &Pipe) -> crate::Result<()> {
    uv_loop.inner_mut().remove_readiness(handle.token);
    let mut state = handle.state.borrow_mut();
    if state.registered {
        if let Some(io) = state.io.as_mut() { match io { PipeIo::Listener(source) => uv_loop.inner_mut().reactor().deregister(source)?, PipeIo::Stream(source) => uv_loop.inner_mut().reactor().deregister(source)? } }
        state.registered = false;
    }
    state.io = None; state.listening = false; state.reading = false; state.connecting = false; state.writes.clear(); state.pending.clear();
    Ok(())
}

#[cfg(unix)]
impl Handle for Pipe {
    fn id(&self) -> HandleId { self.id }
    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> { close_pipe(uv_loop, self)?; close_id(uv_loop, self.id, None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>) }
    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where F: FnOnce(&mut UvLoop, HandleId) -> Result<(), CallbackError> + 'static { close_pipe(uv_loop, self)?; close_id(uv_loop, self.id, Some(callback)) }
}

#[cfg(unix)]
/// Terminal mode for a TTY handle. See `uv.tty_set_mode()` in `runtime/doc/luvref.txt`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TtyMode {
    /// Canonical terminal mode.
    Normal,
    /// Character-at-a-time with signals and echo disabled.
    Cbreak,
    /// Raw mode with all line processing disabled.
    Raw,
}

#[cfg(unix)]
struct TtyState {
    file: Option<File>, original: Termios, readable: bool, reading: bool, writes: WriteQueue, registered: bool,
}

#[cfg(unix)]
/// Terminal stream handle. See `uv_tty_t` in `runtime/doc/luvref.txt`.
pub struct Tty { id: HandleId, token: Token, state: Rc<RefCell<TtyState>>, _callback: CallbackCell }

#[cfg(unix)]
impl Tty {
    /// Opens a TTY stream from a file descriptor. See `uv.new_tty()` in `runtime/doc/luvref.txt`.
    pub fn open<F>(uv_loop: &mut UvLoop, file: File, readable: bool, callback: F) -> NetResult<Self>
    where F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static {
        let original = tcgetattr(&file).map_err(errno_error)?;
        let flags = fcntl_getfl(&file).map_err(errno_error)?;
        fcntl_setfl(&file, flags | OFlags::NONBLOCK).map_err(errno_error)?;
        let id = uv_loop.allocate_external(false)?;
        let token = uv_loop.allocate_io_token()?;
        let state = Rc::new(RefCell::new(TtyState { file: Some(file), original, readable, reading: false, writes: WriteQueue::new(), registered: false }));
        let callback: CallbackCell = Rc::new(RefCell::new(Some(Box::new(callback))));
        register_tty(uv_loop, id, token, &state, &callback)?;
        Ok(Self { id, token, state, _callback: callback })
    }

    /// Sets the terminal mode. See `uv.tty_set_mode()` in `runtime/doc/luvref.txt`.
    pub fn set_mode(&self, mode: TtyMode) -> NetResult<()> {
        let state = self.state.borrow();
        let file = state.file.as_ref().ok_or(NetError::Closed)?;
        let mut attributes = state.original.clone();
        match mode { TtyMode::Normal => {}, TtyMode::Raw => attributes.make_raw(), TtyMode::Cbreak => { attributes.local_modes.remove(LocalModes::ICANON | LocalModes::ECHO); attributes.special_codes[SpecialCodeIndex::VMIN] = 1; attributes.special_codes[SpecialCodeIndex::VTIME] = 0; } }
        tcsetattr(file, OptionalActions::Now, &attributes).map_err(errno_error)
    }

    /// Returns the terminal window size. See `uv.tty_get_winsize()` in `runtime/doc/luvref.txt`.
    pub fn get_winsize(&self) -> NetResult<(u16, u16)> {
        let state = self.state.borrow();
        let size = tcgetwinsize(state.file.as_ref().ok_or(NetError::Closed)?).map_err(errno_error)?;
        Ok((size.ws_col, size.ws_row))
    }

    /// Starts reading input. See `uv.read_start()` in `runtime/doc/luvref.txt`.
    pub fn read_start(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        { let mut state = self.state.borrow_mut(); if state.file.is_none() { return Err(NetError::Closed); } if !state.readable { return Err(NetError::InvalidState("TTY was opened write-only")); } state.reading = true; }
        sync_tty(uv_loop, self.id, self.token, &self.state)
    }
    /// Stops reading input. See `uv.read_stop()` in `runtime/doc/luvref.txt`.
    pub fn read_stop(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        { let mut state = self.state.borrow_mut(); if state.file.is_none() { return Err(NetError::Closed); } state.reading = false; }
        sync_tty(uv_loop, self.id, self.token, &self.state)
    }
    /// Queues `data` to be written. See `uv.write()` in `runtime/doc/luvref.txt`.
    pub fn write(&mut self, uv_loop: &mut UvLoop, data: Vec<u8>) -> NetResult<WriteId> {
        let id = { let mut state = self.state.borrow_mut(); if state.file.is_none() { return Err(NetError::Closed); } state.writes.push(data)? };
        sync_tty(uv_loop, self.id, self.token, &self.state)?;
        Ok(id)
    }
}

#[cfg(unix)]
fn register_tty(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<TtyState>>, callback: &CallbackCell) -> NetResult<()> {
    {
        let mut state = state.borrow_mut(); let interests = interest(state.reading, state.writes.wants_write()); let fd = state.file.as_ref().ok_or(NetError::Closed)?.as_raw_fd(); uv_loop.inner_mut().reactor().register(&mut SourceFd(&fd), token, interests)?; state.registered = true;
    }
    let shared = Rc::clone(state); let user_callback = Rc::clone(callback); let queue = uv_loop.net_dispatch_queue();
    uv_loop.inner_mut().on_readiness(token, move |ready, _| {
        let events = tty_ready(&mut shared.borrow_mut(), ready);
        if !events.is_empty() { let dispatch_state = Rc::clone(&shared); let dispatch_callback = Rc::clone(&user_callback); queue_batch(&queue, move |uv_loop| deliver_tty(uv_loop, id, token, dispatch_state, dispatch_callback, events)); }
        Ok(DrainState::Drained)
    }).map_err(crate::Error::from)?;
    Ok(())
}

#[cfg(unix)]
fn sync_tty(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<TtyState>>) -> NetResult<()> {
    if !live(uv_loop, id) { return Ok(()); }
    let active;
    { let state = state.borrow_mut(); active = state.reading || state.writes.wants_write(); let interests = interest(state.reading, state.writes.wants_write()); if state.registered { let fd = state.file.as_ref().ok_or(NetError::Closed)?.as_raw_fd(); uv_loop.inner_mut().reactor().reregister(&mut SourceFd(&fd), token, interests)?; } }
    uv_loop.set_external_active(id, active)?;
    Ok(())
}

#[cfg(unix)]
fn tty_ready(state: &mut TtyState, ready: Readiness) -> Vec<NetEvent> {
    let mut events = Vec::new();
    let Some(file) = state.file.as_ref() else { return events }; let mut io = file;
    if ready.writable { drive_writes(&mut io, &mut state.writes, &mut events); }
    if ready.readable && state.reading { drain_reads(&mut io, &mut events); if events.iter().any(|event| matches!(event, NetEvent::Eof)) { state.reading = false; } }
    if ready.read_closed && state.reading { state.reading = false; events.push(NetEvent::Eof); }
    if ready.write_closed && !state.writes.pending.is_empty() { while let Some(write) = state.writes.pending.pop_front() { events.push(NetEvent::WriteComplete { id: write.id, result: Err(NetError::Closed) }); } }
    events
}

#[cfg(unix)]
fn deliver_tty(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: Rc<RefCell<TtyState>>, callback: CallbackCell, events: Vec<NetEvent>) {
    if !live(uv_loop, id) { return; }
    if let Err(error) = sync_tty(uv_loop, id, token, &state) { invoke(&callback, uv_loop, id, NetEvent::Error(error)); }
    for event in events { invoke(&callback, uv_loop, id, event); }
}

#[cfg(unix)]
fn close_tty(uv_loop: &mut UvLoop, handle: &Tty) -> crate::Result<()> {
    uv_loop.inner_mut().remove_readiness(handle.token);
    let mut state = handle.state.borrow_mut();
    if state.registered { if let Some(file) = state.file.as_ref() { let fd = file.as_raw_fd(); uv_loop.inner_mut().reactor().deregister(&mut SourceFd(&fd))?; } state.registered = false; }
    if let Some(file) = state.file.as_ref() { tcsetattr(file, OptionalActions::Now, &state.original).map_err(|error| crate::Error::Io(io::Error::from_raw_os_error(error.raw_os_error())))?; }
    state.file = None; state.reading = false; state.writes.clear();
    Ok(())
}

#[cfg(unix)]
impl Handle for Tty {
    fn id(&self) -> HandleId { self.id }
    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> { close_tty(uv_loop, self)?; close_id(uv_loop, self.id, None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>) }
    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where F: FnOnce(&mut UvLoop, HandleId) -> Result<(), CallbackError> + 'static { close_tty(uv_loop, self)?; close_id(uv_loop, self.id, Some(callback)) }
}

struct PendingDatagram { id: WriteId, data: Vec<u8>, target: Option<SocketAddr> }
struct UdpState { socket: Option<UdpSocket>, receiving: bool, sends: VecDeque<PendingDatagram>, next_id: u64, registered: bool }

/// UDP socket handle. See `uv_udp_t` in `runtime/doc/luvref.txt`.
pub struct Udp { id: HandleId, token: Token, state: Rc<RefCell<UdpState>>, _callback: CallbackCell }

impl Udp {
    /// Binds a UDP socket to `address`. See `uv.udp_bind()` in `runtime/doc/luvref.txt`.
    pub fn bind<F>(uv_loop: &mut UvLoop, address: SocketAddr, callback: F) -> NetResult<Self>
    where F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static {
        let id = uv_loop.allocate_external(false)?; let token = uv_loop.allocate_io_token()?;
        let state = Rc::new(RefCell::new(UdpState { socket: Some(UdpSocket::bind(address)?), receiving: false, sends: VecDeque::new(), next_id: 0, registered: false }));
        let callback: CallbackCell = Rc::new(RefCell::new(Some(Box::new(callback))));
        register_udp(uv_loop, id, token, &state, &callback)?;
        Ok(Self { id, token, state, _callback: callback })
    }

    /// Associates the socket with a remote address. See `uv.udp_connect()` in `runtime/doc/luvref.txt`.
    pub fn connect(&self, address: SocketAddr) -> NetResult<()> { self.state.borrow().socket.as_ref().ok_or(NetError::Closed)?.connect(address).map_err(NetError::Io) }
    /// Returns the local socket address. See `uv.udp_getsockname()` in `runtime/doc/luvref.txt`.
    pub fn local_addr(&self) -> NetResult<SocketAddr> { self.state.borrow().socket.as_ref().ok_or(NetError::Closed)?.local_addr().map_err(NetError::Io) }
    /// Returns the peer socket address. See `uv.udp_getpeername()` in `runtime/doc/luvref.txt`.
    pub fn peer_addr(&self) -> NetResult<SocketAddr> { self.state.borrow().socket.as_ref().ok_or(NetError::Closed)?.peer_addr().map_err(NetError::Io) }
    /// Starts receiving datagrams. See `uv.udp_recv_start()` in `runtime/doc/luvref.txt`.
    pub fn recv_start(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> { { let mut state = self.state.borrow_mut(); if state.socket.is_none() { return Err(NetError::Closed); } state.receiving = true; } sync_udp(uv_loop, self.id, self.token, &self.state) }
    /// Stops receiving datagrams. See `uv.udp_recv_stop()` in `runtime/doc/luvref.txt`.
    pub fn recv_stop(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> { { let mut state = self.state.borrow_mut(); if state.socket.is_none() { return Err(NetError::Closed); } state.receiving = false; } sync_udp(uv_loop, self.id, self.token, &self.state) }
    /// Queues a datagram for sending. See `uv.udp_send()` in `runtime/doc/luvref.txt`.
    pub fn send(&mut self, uv_loop: &mut UvLoop, data: Vec<u8>, target: Option<SocketAddr>) -> NetResult<WriteId> {
        let id = { let mut state = self.state.borrow_mut(); if state.socket.is_none() { return Err(NetError::Closed); } let id = WriteId(state.next_id); state.next_id = state.next_id.checked_add(1).ok_or(NetError::RequestLimit)?; state.sends.push_back(PendingDatagram { id, data, target }); id };
        sync_udp(uv_loop, self.id, self.token, &self.state)?; Ok(id)
    }

    /// Joins a multicast group. See `uv.udp_set_membership()` in `runtime/doc/luvref.txt`.
    pub fn join_multicast(&self, group: IpAddr, interface_v4: Option<Ipv4Addr>, interface_v6: Option<u32>) -> NetResult<()> {
        let state = self.state.borrow(); let socket = state.socket.as_ref().ok_or(NetError::Closed)?;
        match group { IpAddr::V4(group) => socket.join_multicast_v4(&group, &interface_v4.unwrap_or(Ipv4Addr::UNSPECIFIED))?, IpAddr::V6(group) => socket.join_multicast_v6(&group, interface_v6.unwrap_or(0))? }
        Ok(())
    }
    /// Leaves a multicast group. See `uv.udp_set_membership()` in `runtime/doc/luvref.txt`.
    pub fn leave_multicast(&self, group: IpAddr, interface_v4: Option<Ipv4Addr>, interface_v6: Option<u32>) -> NetResult<()> {
        let state = self.state.borrow(); let socket = state.socket.as_ref().ok_or(NetError::Closed)?;
        match group { IpAddr::V4(group) => socket.leave_multicast_v4(&group, &interface_v4.unwrap_or(Ipv4Addr::UNSPECIFIED))?, IpAddr::V6(group) => socket.leave_multicast_v6(&group, interface_v6.unwrap_or(0))? }
        Ok(())
    }
    /// Enables or disables multicast loopback. See `uv.udp_set_multicast_loop()` in `runtime/doc/luvref.txt`.
    pub fn set_multicast_loop(&self, family: IpAddr, enable: bool) -> NetResult<()> { let state = self.state.borrow(); let socket = state.socket.as_ref().ok_or(NetError::Closed)?; match family { IpAddr::V4(_) => socket.set_multicast_loop_v4(enable)?, IpAddr::V6(_) => socket.set_multicast_loop_v6(enable)? } Ok(()) }
    /// Sets the multicast time-to-live. See `uv.udp_set_multicast_ttl()` in `runtime/doc/luvref.txt`.
    pub fn set_multicast_ttl(&self, family: IpAddr, ttl: u32) -> NetResult<()> {
        let state = self.state.borrow(); let socket = state.socket.as_ref().ok_or(NetError::Closed)?;
        match family { IpAddr::V4(_) => socket.set_multicast_ttl_v4(ttl)?, IpAddr::V6(_) => { #[cfg(unix)] rustix::net::sockopt::set_ipv6_multicast_hops(socket, ttl).map_err(errno_error)?; #[cfg(not(unix))] return Err(NetError::Unsupported("safe IPv6 multicast hop configuration is unavailable")); } }
        Ok(())
    }
    #[cfg(unix)]
    /// Sets the multicast interface. See `uv.udp_set_multicast_interface()` in `runtime/doc/luvref.txt`.
    pub fn set_multicast_interface(&self, address: IpAddr, interface_index: Option<u32>) -> NetResult<()> {
        let state = self.state.borrow(); let socket = state.socket.as_ref().ok_or(NetError::Closed)?;
        match address { IpAddr::V4(address) => rustix::net::sockopt::set_ip_multicast_if(socket, &address).map_err(errno_error)?, IpAddr::V6(_) => rustix::net::sockopt::set_ipv6_multicast_if(socket, interface_index.unwrap_or(0)).map_err(errno_error)? }
        Ok(())
    }
    /// Enables or disables broadcast. See `uv.udp_set_broadcast()` in `runtime/doc/luvref.txt`.
    pub fn set_broadcast(&self, enable: bool) -> NetResult<()> { self.state.borrow().socket.as_ref().ok_or(NetError::Closed)?.set_broadcast(enable).map_err(NetError::Io) }
    /// Sets the unicast time-to-live. See `uv.udp_set_ttl()` in `runtime/doc/luvref.txt`.
    pub fn set_ttl(&self, ttl: u32) -> NetResult<()> { self.state.borrow().socket.as_ref().ok_or(NetError::Closed)?.set_ttl(ttl).map_err(NetError::Io) }
}

fn register_udp(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<UdpState>>, callback: &CallbackCell) -> NetResult<()> {
    { let mut state = state.borrow_mut(); let interests = interest(state.receiving, !state.sends.is_empty()); uv_loop.inner_mut().reactor().register(state.socket.as_mut().ok_or(NetError::Closed)?, token, interests)?; state.registered = true; }
    let shared = Rc::clone(state); let user_callback = Rc::clone(callback); let queue = uv_loop.net_dispatch_queue();
    uv_loop.inner_mut().on_readiness(token, move |ready, _| {
        let events = udp_ready(&mut shared.borrow_mut(), ready);
        if !events.is_empty() { let dispatch_state = Rc::clone(&shared); let dispatch_callback = Rc::clone(&user_callback); queue_batch(&queue, move |uv_loop| deliver_udp(uv_loop, id, token, dispatch_state, dispatch_callback, events)); }
        Ok(DrainState::Drained)
    }).map_err(crate::Error::from)?;
    Ok(())
}

fn sync_udp(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: &Rc<RefCell<UdpState>>) -> NetResult<()> {
    if !live(uv_loop, id) { return Ok(()); }
    let active;
    { let mut state = state.borrow_mut(); active = state.receiving || !state.sends.is_empty(); let interests = interest(state.receiving, !state.sends.is_empty()); if state.registered { uv_loop.inner_mut().reactor().reregister(state.socket.as_mut().ok_or(NetError::Closed)?, token, interests)?; } }
    uv_loop.set_external_active(id, active)?; Ok(())
}

fn udp_ready(state: &mut UdpState, ready: Readiness) -> Vec<NetEvent> {
    let mut events = Vec::new(); let Some(socket) = state.socket.as_ref() else { return events };
    if ready.readable && state.receiving { loop { let mut data = vec![0; DATAGRAM_CHUNK]; match socket.recv_from(&mut data) { Ok((read, from)) => { data.truncate(read); events.push(NetEvent::Datagram { data, from }); }, Err(error) if error.kind() == io::ErrorKind::Interrupted => {}, Err(error) if is_would_block(&error) => break, Err(error) => { events.push(NetEvent::Error(NetError::Io(error))); break; } } } }
    if ready.writable { loop { let Some(send) = state.sends.front() else { break }; let result = match send.target { Some(target) => socket.send_to(&send.data, target), None => socket.send(&send.data) }; match result { Ok(written) if written == send.data.len() => { let id = send.id; state.sends.pop_front(); events.push(NetEvent::WriteComplete { id, result: Ok(()) }); }, Ok(_) => { let id = send.id; state.sends.pop_front(); events.push(NetEvent::WriteComplete { id, result: Err(NetError::Io(io::Error::from(io::ErrorKind::WriteZero))) }); }, Err(error) if error.kind() == io::ErrorKind::Interrupted => {}, Err(error) if is_would_block(&error) => break, Err(error) => { let id = send.id; state.sends.pop_front(); events.push(NetEvent::WriteComplete { id, result: Err(NetError::Io(error)) }); } } } }
    if ready.error { match socket.take_error() { Ok(Some(error)) => events.push(NetEvent::Error(NetError::Io(error))), Err(error) => events.push(NetEvent::Error(NetError::Io(error))), Ok(None) => {} } }
    events
}

fn deliver_udp(uv_loop: &mut UvLoop, id: HandleId, token: Token, state: Rc<RefCell<UdpState>>, callback: CallbackCell, events: Vec<NetEvent>) {
    if !live(uv_loop, id) { return; }
    if let Err(error) = sync_udp(uv_loop, id, token, &state) { invoke(&callback, uv_loop, id, NetEvent::Error(error)); }
    for event in events { invoke(&callback, uv_loop, id, event); }
}

fn close_udp(uv_loop: &mut UvLoop, handle: &Udp) -> crate::Result<()> {
    uv_loop.inner_mut().remove_readiness(handle.token);
    let mut state = handle.state.borrow_mut();
    if state.registered { if let Some(socket) = state.socket.as_mut() { uv_loop.inner_mut().reactor().deregister(socket)?; } state.registered = false; }
    state.socket = None; state.receiving = false; state.sends.clear();
    Ok(())
}

impl Handle for Udp {
    fn id(&self) -> HandleId { self.id }
    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> { close_udp(uv_loop, self)?; close_id(uv_loop, self.id, None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>) }
    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where F: FnOnce(&mut UvLoop, HandleId) -> Result<(), CallbackError> + 'static { close_udp(uv_loop, self)?; close_id(uv_loop, self.id, Some(callback)) }
}
