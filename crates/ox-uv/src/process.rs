//! Safe process spawning and control for the `vim.uv` process surface.
//!
//! Child waiters only reap and enqueue terminal results. Handle state changes
//! and user callbacks run during the owning [`crate::UvLoop`]'s pending phase.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::pool::{LoopPoster, UvLoopPoster};
use crate::{Handle, HandleId, UvLoop};

#[cfg(unix)]
use std::cell::RefCell;
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
#[cfg(unix)]
use std::rc::Rc;

#[cfg(unix)]
use mio::unix::SourceFd;
#[cfg(unix)]
use mio::{Interest, Token};
#[cfg(unix)]
use ox_loop::{DrainState, Readiness};

#[cfg(unix)]
use crate::net::{
    CallbackCell, NetError, NetEvent, NetResult, WriteId, WriteQueue, drain_reads, drive_writes,
    interest, invoke, is_would_block, live, queue_batch,
};

#[cfg(unix)]
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

/// Result delivered to a process exit callback.
pub type ProcessExitResult = Result<ProcessExit, ProcessError>;

/// Child standard-stream behavior.
///
/// This is the safe Rust mapping of the inherit, ignore, and create-pipe forms
/// in `luvref.txt`, `uv.spawn()` / `uv.spawn-options` (lines 1340-1452).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StdioConfig {
    /// Inherit the corresponding standard stream from the parent.
    #[default]
    Inherit,
    /// Connect the child stream to the platform null device.
    Ignore,
    /// Create a pipe and return its parent endpoint from [`spawn`].
    CreatePipe,
}

/// Options accepted by [`spawn`].
///
/// `environment: None` inherits the parent environment. `Some(entries)`
/// replaces it, matching `luvref.txt`, `uv.spawn-options` (lines 1403-1439).
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    /// Executable path.
    pub program: PathBuf,
    /// Arguments after the executable name.
    pub args: Vec<OsString>,
    /// Replacement environment, or `None` to inherit it.
    pub environment: Option<Vec<(OsString, OsString)>>,
    /// Child working directory.
    pub cwd: Option<PathBuf>,
    /// Standard input, output, and error configuration, in that order.
    pub stdio: [StdioConfig; 3],
    /// Spawn as a process-group leader where the platform supports it.
    pub detached: bool,
    /// Unix child user ID. Supplying it on Windows is unsupported.
    pub uid: Option<u32>,
    /// Unix child group ID. Supplying it on Windows is unsupported.
    pub gid: Option<u32>,
    /// Hide the child console window on Windows; ignored on Unix per luvref.
    pub hide: bool,
}

impl SpawnOptions {
    /// Creates options for `program` with inherited environment and stdio.
    ///
    /// See `luvref.txt`, `uv.spawn-options` (lines 1403-1449).
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            environment: None,
            cwd: None,
            stdio: [StdioConfig::Inherit; 3],
            detached: false,
            uid: None,
            gid: None,
            hide: false,
        }
    }
}

/// Exit values delivered after the child has been reaped.
///
/// See `luvref.txt`, `uv.spawn()` (lines 1340-1351, 1451-1452).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessExit {
    /// Normal exit code, or zero when the process terminated by signal.
    pub code: i64,
    /// Unix terminating signal number, or zero for a normal/Windows exit.
    pub signal: i32,
}

/// Parent-side endpoints created by [`StdioConfig::CreatePipe`].
///
/// On Unix each endpoint is a loop-managed, nonblocking [`ProcessPipe`]
/// handle registered with the owning [`UvLoop`]; reads and writes are drained
/// until `WouldBlock` and delivered as [`NetEvent`] readiness callbacks. On
/// other platforms the underlying blocking standard-stream types are returned
/// because the platform lacks a safe nonblocking registration primitive here.
#[cfg(unix)]
#[derive(Default)]
pub struct ProcessPipes {
    /// Writable parent endpoint connected to child stdin.
    pub stdin: Option<ProcessPipe>,
    /// Readable parent endpoint connected to child stdout.
    pub stdout: Option<ProcessPipe>,
    /// Readable parent endpoint connected to child stderr.
    pub stderr: Option<ProcessPipe>,
}

/// Parent-side endpoints created by [`StdioConfig::CreatePipe`].
#[cfg(not(unix))]
#[derive(Debug, Default)]
pub struct ProcessPipes {
    /// Writable parent endpoint connected to child stdin.
    pub stdin: Option<ChildStdin>,
    /// Readable parent endpoint connected to child stdout.
    pub stdout: Option<ChildStdout>,
    /// Readable parent endpoint connected to child stderr.
    pub stderr: Option<ChildStderr>,
}

#[cfg(unix)]
impl std::fmt::Debug for ProcessPipes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessPipes")
            .field("stdin", &self.stdin.as_ref().map(ProcessPipe::id))
            .field("stdout", &self.stdout.as_ref().map(ProcessPipe::id))
            .field("stderr", &self.stderr.as_ref().map(ProcessPipe::id))
            .finish()
    }
}

/// A spawned and asynchronously reaped child plus its created pipes.
#[derive(Debug)]
pub struct SpawnedProcess {
    /// Process control handle.
    pub process: Process,
    /// Parent-side pipe endpoints requested in the spawn options.
    pub pipes: ProcessPipes,
}

/// A parent-side endpoint of a [`StdioConfig::CreatePipe`] child stream.
#[cfg(unix)]
enum ChildStream {
    /// Writable child-stdin endpoint.
    In(ChildStdin),
    /// Readable child-stdout endpoint.
    Out(ChildStdout),
    /// Readable child-stderr endpoint.
    Err(ChildStderr),
}

#[cfg(unix)]
impl ChildStream {
    fn set_nonblocking(&self) -> io::Result<()> {
        use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
        match self {
            ChildStream::In(stream) => {
                fcntl_setfl(stream, fcntl_getfl(stream)? | OFlags::NONBLOCK)
            }
            ChildStream::Out(stream) => {
                fcntl_setfl(stream, fcntl_getfl(stream)? | OFlags::NONBLOCK)
            }
            ChildStream::Err(stream) => {
                fcntl_setfl(stream, fcntl_getfl(stream)? | OFlags::NONBLOCK)
            }
        }
        .map_err(io::Error::from)
    }
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for ChildStream {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            ChildStream::In(stream) => stream.as_raw_fd(),
            ChildStream::Out(stream) => stream.as_raw_fd(),
            ChildStream::Err(stream) => stream.as_raw_fd(),
        }
    }
}

#[cfg(unix)]
struct ProcessPipeState {
    io: Option<ChildStream>,
    reading: bool,
    writes: WriteQueue,
    registered: bool,
}

#[cfg(unix)]
fn process_pipe_interest(state: &ProcessPipeState) -> Interest {
    match &state.io {
        Some(ChildStream::In(_)) => interest(false, state.writes.wants_write()),
        Some(ChildStream::Out(_)) | Some(ChildStream::Err(_)) => {
            interest(state.reading, state.writes.wants_write())
        }
        None => Interest::READABLE,
    }
}

#[cfg(unix)]
fn process_pipe_active(state: &ProcessPipeState) -> bool {
    state.reading || state.writes.wants_write()
}

/// A [`StdioConfig::CreatePipe`] parent endpoint that pumps through the owning
/// loop instead of blocking the host thread.
///
/// The child stream's descriptor is registered with the loop's reactor through
/// the sanctioned `UvLoop::inner_mut` seam, placed in nonblocking mode, and
/// drained until `WouldBlock` on readiness the same way the network handles in
/// `net.rs` drain their streams. Events surface as [`NetEvent`] readiness
/// callbacks delivered during the loop's pending phase.
///
/// See `luvref.txt`, `uv.spawn()` / `uv.spawn-options` (lines 1340-1452).
#[cfg(unix)]
pub struct ProcessPipe {
    id: HandleId,
    token: Token,
    state: Rc<RefCell<ProcessPipeState>>,
    _callback: CallbackCell,
}

#[cfg(unix)]
impl std::fmt::Debug for ProcessPipe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ProcessPipe").field("id", &self.id).finish()
    }
}

#[cfg(unix)]
impl ProcessPipe {
    fn attach(uv_loop: &mut UvLoop, stream: ChildStream) -> NetResult<Self> {
        stream.set_nonblocking().map_err(NetError::Io)?;
        let id = uv_loop.allocate_external(false)?;
        let token = uv_loop.allocate_io_token()?;
        let state = Rc::new(RefCell::new(ProcessPipeState {
            io: Some(stream),
            reading: false,
            writes: WriteQueue::new(),
            registered: false,
        }));
        let callback: CallbackCell = Rc::new(RefCell::new(None));
        register_process_pipe(uv_loop, id, token, &state, &callback)?;
        Ok(Self { id, token, state, _callback: callback })
    }

    /// Starts reading bytes from this endpoint, installing `callback`.
    ///
    /// Readable events emit [`NetEvent::Read`] until [`NetEvent::Eof`]. The
    /// endpoint must be a stdout or stderr pipe; child stdin is write-only.
    /// See `luvref.txt`, `uv.read_start()` (lines 1948-1992).
    pub fn read_start<F>(&mut self, uv_loop: &mut UvLoop, callback: F) -> NetResult<()>
    where
        F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static,
    {
        {
            let mut state = self.state.borrow_mut();
            if state.io.is_none() {
                return Err(NetError::Closed);
            }
            if matches!(state.io, Some(ChildStream::In(_))) {
                return Err(NetError::InvalidState("child stdin cannot be read"));
            }
            state.reading = true;
        }
        *self._callback.borrow_mut() = Some(Box::new(callback));
        sync_process_pipe(uv_loop, self.id, self.token, &self.state)
    }

    /// Stops reading bytes from this endpoint. See `uv.read_stop()`.
    pub fn read_stop(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        {
            let mut state = self.state.borrow_mut();
            if state.io.is_none() {
                return Err(NetError::Closed);
            }
            state.reading = false;
        }
        sync_process_pipe(uv_loop, self.id, self.token, &self.state)
    }

    /// Queues or immediately writes `data` to this endpoint.
    ///
    /// The endpoint must be child stdin. A small write is flushed
    /// synchronously so callers may close the pipe immediately after; a
    /// partially buffered remainder resumes on writable readiness with a
    /// [`NetEvent::WriteComplete`]. See `luvref.txt`, `uv.write()`.
    pub fn write(&mut self, uv_loop: &mut UvLoop, data: Vec<u8>) -> NetResult<WriteId> {
        let id;
        let mut events = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            if state.io.is_none() {
                return Err(NetError::Closed);
            }
            if !matches!(state.io, Some(ChildStream::In(_))) {
                return Err(NetError::InvalidState("child stdout/stderr cannot be written"));
            }
            id = state.writes.push(data)?;
            let ProcessPipeState { io, writes, .. } = &mut *state;
            if let Some(ChildStream::In(stream)) = io {
                drive_writes(stream, writes, &mut events);
            }
        }
        if !events.is_empty() && live(uv_loop, self.id) {
            deliver_process_pipe(
                uv_loop,
                self.id,
                self.token,
                Rc::clone(&self.state),
                Rc::clone(&self._callback),
                events,
            );
        }
        sync_process_pipe(uv_loop, self.id, self.token, &self.state)?;
        Ok(id)
    }
}

#[cfg(unix)]
fn register_process_pipe(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: &Rc<RefCell<ProcessPipeState>>,
    callback: &CallbackCell,
) -> NetResult<()> {
    {
        let mut state = state.borrow_mut();
        let interests = process_pipe_interest(&state);
        let fd = state.io.as_ref().ok_or(NetError::Closed)?.as_raw_fd();
        uv_loop.inner_mut().reactor().register(&mut SourceFd(&fd), token, interests)?;
        state.registered = true;
    }
    let shared = Rc::clone(state);
    let user_callback = Rc::clone(callback);
    let queue = uv_loop.net_dispatch_queue();
    uv_loop
        .inner_mut()
        .on_readiness(token, move |ready, _| {
            let events = process_pipe_ready(&mut shared.borrow_mut(), ready);
            if !events.is_empty() {
                let dispatch_state = Rc::clone(&shared);
                let dispatch_callback = Rc::clone(&user_callback);
                queue_batch(&queue, move |uv_loop| {
                    deliver_process_pipe(uv_loop, id, token, dispatch_state, dispatch_callback, events)
                });
            }
            Ok(DrainState::Drained)
        })
        .map_err(crate::Error::from)?;
    Ok(())
}

#[cfg(unix)]
fn sync_process_pipe(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: &Rc<RefCell<ProcessPipeState>>,
) -> NetResult<()> {
    if !live(uv_loop, id) {
        return Ok(());
    }
    let active;
    {
        let state = state.borrow_mut();
        active = process_pipe_active(&state);
        let interests = process_pipe_interest(&state);
        if state.registered {
            let fd = state.io.as_ref().ok_or(NetError::Closed)?.as_raw_fd();
            uv_loop.inner_mut().reactor().reregister(&mut SourceFd(&fd), token, interests)?;
        }
    }
    uv_loop.set_external_active(id, active)?;
    Ok(())
}

#[cfg(unix)]
fn process_pipe_drain<R: std::io::Read>(
    ready: Readiness,
    reading: &mut bool,
    reader: &mut R,
    events: &mut Vec<NetEvent>,
) {
    if ready.readable && *reading {
        drain_reads(reader, events);
        if events.iter().any(|event| matches!(event, NetEvent::Eof)) {
            *reading = false;
        }
    }
    if ready.read_closed && *reading {
        *reading = false;
        events.push(NetEvent::Eof);
    }
}

#[cfg(unix)]
fn process_pipe_ready(state: &mut ProcessPipeState, ready: Readiness) -> Vec<NetEvent> {
    let mut events = Vec::new();
    match &mut state.io {
        Some(ChildStream::In(stream)) => {
            if ready.writable {
                drive_writes(stream, &mut state.writes, &mut events);
            }
            if ready.write_closed && !state.writes.pending.is_empty() {
                while let Some(write) = state.writes.pending.pop_front() {
                    events.push(NetEvent::WriteComplete { id: write.id, result: Err(NetError::Closed) });
                }
            }
        }
        Some(ChildStream::Out(stream)) => {
            process_pipe_drain(ready, &mut state.reading, stream, &mut events)
        }
        Some(ChildStream::Err(stream)) => {
            process_pipe_drain(ready, &mut state.reading, stream, &mut events)
        }
        None => {}
    }
    events
}

#[cfg(unix)]
fn deliver_process_pipe(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: Rc<RefCell<ProcessPipeState>>,
    callback: CallbackCell,
    events: Vec<NetEvent>,
) {
    if !live(uv_loop, id) {
        return;
    }
    if let Err(error) = sync_process_pipe(uv_loop, id, token, &state) {
        invoke(&callback, uv_loop, id, NetEvent::Error(error));
    }
    for event in events {
        if !live(uv_loop, id) {
            break;
        }
        invoke(&callback, uv_loop, id, event);
    }
}

#[cfg(unix)]
fn close_process_pipe(uv_loop: &mut UvLoop, handle: &ProcessPipe) -> crate::Result<()> {
    uv_loop.inner_mut().remove_readiness(handle.token);
    let mut state = handle.state.borrow_mut();
    if state.registered {
        if let Some(stream) = state.io.as_ref() {
            let fd = stream.as_raw_fd();
            uv_loop.inner_mut().reactor().deregister(&mut SourceFd(&fd))?;
        }
        state.registered = false;
    }
    state.io = None;
    state.reading = false;
    state.writes.clear();
    Ok(())
}

#[cfg(unix)]
impl Handle for ProcessPipe {
    fn id(&self) -> HandleId {
        self.id
    }

    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        close_process_pipe(uv_loop, self)?;
        uv_loop.close(
            self.id,
            None::<fn(&mut UvLoop, HandleId) -> std::result::Result<(), crate::CallbackError>>,
        )
    }

    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where
        F: FnOnce(&mut UvLoop, HandleId)
                -> std::result::Result<(), crate::CallbackError>
            + 'static,
    {
        close_process_pipe(uv_loop, self)?;
        uv_loop.close(self.id, Some(callback))
    }
}

/// Builds loop-managed pipe endpoints from the child's standard streams.
#[cfg(unix)]
fn build_process_pipes(
    uv_loop: &mut UvLoop,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
) -> Result<ProcessPipes, ProcessError> {
    let map_error = |error: NetError| {
        ProcessError::io("setup process pipe", io::Error::other(error.to_string()))
    };
    Ok(ProcessPipes {
        stdin: stdin
            .map(|stream| ProcessPipe::attach(uv_loop, ChildStream::In(stream)))
            .transpose()
            .map_err(map_error)?,
        stdout: stdout
            .map(|stream| ProcessPipe::attach(uv_loop, ChildStream::Out(stream)))
            .transpose()
            .map_err(map_error)?,
        stderr: stderr
            .map(|stream| ProcessPipe::attach(uv_loop, ChildStream::Err(stream)))
            .transpose()
            .map_err(map_error)?,
    })
}

/// Builds blocking endpoints on platforms without a safe loop registration path.
#[cfg(not(unix))]
fn build_process_pipes(
    _uv_loop: &mut UvLoop,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
) -> Result<ProcessPipes, ProcessError> {
    Ok(ProcessPipes { stdin, stdout, stderr })
}

/// Process and PTY operation failures.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// A platform operation failed.
    #[error("{operation} failed: {source}")]
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Platform error.
        #[source]
        source: io::Error,
    },
    /// A process identifier cannot name a process on this platform.
    #[error("invalid process id {0}")]
    InvalidPid(u32),
    /// Only safe, named signals are accepted.
    #[error("invalid or unsupported signal number {0}")]
    InvalidSignal(i32),
    /// Priority lies outside libuv's documented range.
    #[error("priority {0} is outside the supported range -20..=19")]
    InvalidPriority(i32),
    /// The requested operation has no complete safe implementation here.
    #[error("{feature} is unsupported: {reason}")]
    Unsupported {
        /// Capability that is unavailable.
        feature: &'static str,
        /// Precise missing safe integration primitive.
        reason: &'static str,
    },
    /// The owning loop could not allocate or update the process handle.
    #[error(transparent)]
    Handle(#[from] crate::Error),
}

impl ProcessError {
    fn io(operation: &'static str, source: impl Into<io::Error>) -> Self {
        Self::Io {
            operation,
            source: source.into(),
        }
    }
}

#[derive(Debug)]
enum ChildState {
    Standard(Child),
    Portable(Box<dyn portable_pty::Child + Send + Sync>),
    Exited,
}

/// A live or reaped child-process identity.
#[derive(Debug)]
pub struct Process {
    id: HandleId,
    pid: u32,
    child: Arc<Mutex<ChildState>>,
}

impl Process {
    /// Returns the child PID.
    ///
    /// See `luvref.txt`, `uv.process_get_pid()` (lines 1489-1498).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Sends a signal to this process, defaulting to SIGTERM.
    ///
    /// See `luvref.txt`, `uv.process_kill()` (lines 1457-1472).
    pub fn kill(&self, signal: Option<i32>) -> Result<(), ProcessError> {
        let state = lock_recover(&self.child);
        if matches!(&*state, ChildState::Exited) {
            return Err(ProcessError::io(
                "process_kill",
                io::Error::new(io::ErrorKind::NotFound, "process has already exited"),
            ));
        }

        #[cfg(unix)]
        {
            send_unix_signal(self.pid, signal)
        }

        #[cfg(windows)]
        {
            let mut state = state;
            match signal.unwrap_or(15) {
                9 | 15 => terminate_child(&mut *state)
                    .map_err(|error| ProcessError::io("process_kill", error)),
                number => Err(ProcessError::InvalidSignal(number)),
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = signal;
            Err(ProcessError::Unsupported {
                feature: "process_kill",
                reason: "this target has neither the Unix signal API nor Windows child termination",
            })
        }
    }
}

impl Handle for Process {
    fn id(&self) -> HandleId {
        self.id
    }

    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        uv_loop.close(
            self.id,
            None::<fn(&mut UvLoop, HandleId) -> std::result::Result<(), crate::CallbackError>>,
        )?;
        terminate_and_reap(&self.child);
        Ok(())
    }

    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where
        F: FnOnce(&mut UvLoop, HandleId)
                -> std::result::Result<(), crate::CallbackError>
            + 'static,
    {
        uv_loop.close(self.id, Some(callback))?;
        terminate_and_reap(&self.child);
        Ok(())
    }
}

/// Spawns a child, returns requested pipes, and schedules its exit callback.
///
/// The child waiter only reaps and posts. The callback itself is invoked by the
/// owning loop, never by the waiter. See `luvref.txt`, `uv.spawn()` and
/// `uv.spawn-options` (lines 1340-1455).
pub fn spawn<F>(
    uv_loop: &mut UvLoop,
    options: SpawnOptions,
    on_exit: F,
) -> Result<SpawnedProcess, ProcessError>
where
    F: FnOnce(&mut UvLoop, ProcessExitResult) + Send + 'static,
{
    validate_platform_options(&options)?;

    let mut command = Command::new(&options.program);
    command.args(&options.args);
    if let Some(environment) = &options.environment {
        command.env_clear();
        command.envs(environment.iter().map(|(name, value)| (name, value)));
    }
    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    command.stdin(map_stdio(options.stdio[0]));
    command.stdout(map_stdio(options.stdio[1]));
    command.stderr(map_stdio(options.stdio[2]));
    configure_platform_command(&mut command, &options);

    let mut child = command
        .spawn()
        .map_err(|error| ProcessError::io("spawn", error))?;
    let pid = child.id();
    let child_stdin = child.stdin.take();
    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();
    let state = Arc::new(Mutex::new(ChildState::Standard(child)));
    let handle_id = match uv_loop.allocate_external(true) {
        Ok(id) => id,
        Err(error) => {
            terminate_and_reap(&state);
            return Err(error.into());
        }
    };
    let pipes = match build_process_pipes(uv_loop, child_stdin, child_stdout, child_stderr) {
        Ok(pipes) => pipes,
        Err(error) => {
            let process = Process { id: handle_id, pid, child: Arc::clone(&state) };
            let _ = process.close(uv_loop);
            terminate_and_reap(&state);
            return Err(error);
        }
    };
    let process = Process { id: handle_id, pid, child: Arc::clone(&state) };
    let waiter_state = Arc::clone(&state);
    let waiter_poster = uv_loop.completion_poster();

    let waiter = std::thread::Builder::new()
        .name(format!("ox-uv-process-{pid}"))
        .spawn(move || wait_and_post(waiter_state, waiter_poster, handle_id, on_exit));
    if let Err(error) = waiter {
        let _ = process.close(uv_loop);
        return Err(ProcessError::io("start process waiter", error));
    }

    Ok(SpawnedProcess { process, pipes })
}

/// Sends a signal to an arbitrary PID, defaulting to SIGTERM.
///
/// See `luvref.txt`, `uv.kill()` (lines 1474-1487).
pub fn kill(pid: u32, signal: Option<i32>) -> Result<(), ProcessError> {
    #[cfg(unix)]
    {
        send_unix_signal(pid, signal)
    }

    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(ProcessError::Unsupported {
            feature: "kill(pid, signal)",
            reason: "safe standard-library APIs cannot open and signal an arbitrary Windows PID",
        })
    }
}

/// Sends a signal through a process handle.
///
/// See `luvref.txt`, `uv.process_kill()` (lines 1457-1472).
pub fn process_kill(process: &Process, signal: Option<i32>) -> Result<(), ProcessError> {
    process.kill(signal)
}

/// Returns the current process ID.
///
/// See `luvref.txt`, `uv.os_getpid()` (lines 4462-4466).
pub fn getpid() -> u32 {
    crate::misc::getpid()
}

/// Returns the parent process ID, or zero when the platform exposes no parent.
///
/// See `luvref.txt`, `uv.os_getppid()` (lines 4468-4472).
pub fn getppid() -> u32 {
    #[cfg(unix)]
    {
        rustix::process::getppid().map_or(0, |pid| pid.as_raw_pid() as u32)
    }

    #[cfg(not(unix))]
    {
        0
    }
}

/// Returns a process scheduling priority.
///
/// PID zero selects the current process. See `luvref.txt`,
/// `uv.os_getpriority()` (lines 4474-4482).
pub fn os_getpriority(pid: u32) -> Result<i32, ProcessError> {
    #[cfg(unix)]
    {
        let pid = optional_unix_pid(pid)?;
        rustix::process::getpriority_process(pid)
            .map_err(|error| ProcessError::io("os_getpriority", error))
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(ProcessError::Unsupported {
            feature: "os_getpriority",
            reason: "no safe standard-library process-priority API exists on this target",
        })
    }
}

/// Sets a process scheduling priority in the range -20 through 19.
///
/// PID zero selects the current process. See `luvref.txt`,
/// `uv.os_setpriority()` (lines 4484-4494).
pub fn os_setpriority(pid: u32, priority: i32) -> Result<(), ProcessError> {
    if !(-20..=19).contains(&priority) {
        return Err(ProcessError::InvalidPriority(priority));
    }

    #[cfg(unix)]
    {
        let pid = optional_unix_pid(pid)?;
        rustix::process::setpriority_process(pid, priority)
            .map_err(|error| ProcessError::io("os_setpriority", error))
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(ProcessError::Unsupported {
            feature: "os_setpriority",
            reason: "no safe standard-library process-priority API exists on this target",
        })
    }
}

/// PTY geometry requested for [`spawn_pty`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtySize {
    /// Terminal columns.
    pub columns: u16,
    /// Terminal rows.
    pub rows: u16,
}

/// A spawned PTY child and the parent-side PTY master.
#[cfg(unix)]
pub struct SpawnedPty {
    /// Child process handle.
    pub process: Process,
    /// Loop-managed, nonblocking parent-side PTY master.
    pub master: PtyHandle,
}

/// A spawned PTY child and the parent-side PTY master.
#[cfg(not(unix))]
pub struct SpawnedPty {
    /// Child process handle.
    pub process: Process,
    /// Parent-side PTY master.
    pub master: Box<dyn portable_pty::MasterPty + Send>,
}

impl std::fmt::Debug for SpawnedPty {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpawnedPty")
            .field("process", &self.process)
            .field("master", &"portable PTY master")
            .finish()
    }
}

#[cfg(unix)]
struct BorrowedUnixFd(i32);

#[cfg(unix)]
impl std::os::fd::AsRawFd for BorrowedUnixFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0
    }
}

#[cfg(unix)]
struct PtyState {
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    fd: Option<filedescriptor::FileDescriptor>,
    reading: bool,
    writes: WriteQueue,
    registered: bool,
}

#[cfg(unix)]
fn pty_interest(state: &PtyState) -> Interest {
    interest(state.reading, state.writes.wants_write())
}

#[cfg(unix)]
fn pty_active(state: &PtyState) -> bool {
    state.reading || state.writes.wants_write()
}

/// A parent-side pseudoterminal master wrapped as a registered loop handle.
///
/// The native master descriptor is duplicated, placed in nonblocking mode, and
/// registered with the owning loop's reactor through [`UvLoop::inner_mut`].
/// Reads and writes drain until `WouldBlock` on readiness and surface as
/// [`NetEvent`] callbacks during the loop's pending phase, matching the
/// `net.rs` handle contract instead of exposing a blocking
/// `portable_pty::MasterPty` to the host.
///
/// PTY stdio is necessarily the slave terminal, so `options.stdio` is ignored.
/// See `luvref.txt`, `uv.spawn()` and the Task 7b PTY extension.
#[cfg(unix)]
pub struct PtyHandle {
    id: HandleId,
    token: Token,
    state: Rc<RefCell<PtyState>>,
    _callback: CallbackCell,
}

#[cfg(unix)]
impl std::fmt::Debug for PtyHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PtyHandle").field("id", &self.id).finish()
    }
}

#[cfg(unix)]
impl PtyHandle {
    fn wrap(
        uv_loop: &mut UvLoop,
        master: Box<dyn portable_pty::MasterPty + Send>,
    ) -> Result<Self, ProcessError> {
        let raw = master.as_raw_fd().ok_or(ProcessError::Unsupported {
            feature: "PTY master fd",
            reason: "the native portable-pty backend exposed no Unix master descriptor",
        })?;
        let mut fd = filedescriptor::FileDescriptor::dup(&BorrowedUnixFd(raw))
            .map_err(|error| {
                ProcessError::io(
                    "duplicate PTY master fd",
                    io::Error::other(error.to_string()),
                )
            })?;
        fd.set_non_blocking(true).map_err(|error| {
            ProcessError::io(
                "make PTY master nonblocking",
                io::Error::other(error.to_string()),
            )
        })?;

        let id = uv_loop.allocate_external(false).map_err(ProcessError::from)?;
        let token = uv_loop.allocate_io_token().map_err(ProcessError::from)?;
        let state = Rc::new(RefCell::new(PtyState {
            master: Some(master),
            fd: Some(fd),
            reading: false,
            writes: WriteQueue::new(),
            registered: false,
        }));
        let callback: CallbackCell = Rc::new(RefCell::new(None));
        register_pty(uv_loop, id, token, &state, &callback)
            .map_err(|error| ProcessError::io("PTY handle", io::Error::other(error.to_string())))?;
        Ok(Self { id, token, state, _callback: callback })
    }

    /// Starts reading output from the session, installing `callback`.
    ///
    /// Readable bytes surface as [`NetEvent::Read`] until [`NetEvent::Eof`];
    /// a terminated session whose slave has closed reads as end-of-file.
    /// See `luvref.txt`, `uv.read_start()`.
    pub fn read_start<F>(&mut self, uv_loop: &mut UvLoop, callback: F) -> NetResult<()>
    where
        F: FnMut(&mut UvLoop, HandleId, NetEvent) + 'static,
    {
        {
            let mut state = self.state.borrow_mut();
            if state.fd.is_none() {
                return Err(NetError::Closed);
            }
            state.reading = true;
        }
        *self._callback.borrow_mut() = Some(Box::new(callback));
        sync_pty(uv_loop, self.id, self.token, &self.state)
    }

    /// Stops reading session output. See `luvref.txt`, `uv.read_stop()`.
    pub fn read_stop(&mut self, uv_loop: &mut UvLoop) -> NetResult<()> {
        {
            let mut state = self.state.borrow_mut();
            if state.fd.is_none() {
                return Err(NetError::Closed);
            }
            state.reading = false;
        }
        sync_pty(uv_loop, self.id, self.token, &self.state)
    }

    /// Writes `data` to the child's terminal input.
    ///
    /// A small write is flushed synchronously; a partially buffered remainder
    /// resumes on writable readiness and reports a [`NetEvent::WriteComplete`].
    /// See `luvref.txt`, `uv.write()`.
    pub fn write(&mut self, uv_loop: &mut UvLoop, data: Vec<u8>) -> NetResult<WriteId> {
        let id;
        let mut events = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            if state.fd.is_none() {
                return Err(NetError::Closed);
            }
            id = state.writes.push(data)?;
            let PtyState { fd, writes, .. } = &mut *state;
            if let Some(pipe_fd) = fd {
                drive_writes(pipe_fd, writes, &mut events);
            }
        }
        if !events.is_empty() && live(uv_loop, self.id) {
            deliver_pty(
                uv_loop,
                self.id,
                self.token,
                Rc::clone(&self.state),
                Rc::clone(&self._callback),
                events,
            );
        }
        sync_pty(uv_loop, self.id, self.token, &self.state)?;
        Ok(id)
    }

    /// Resizes the pseudoterminal window. See `luvref.txt`, `uv.tty_set_size()`.
    pub fn resize(&self, size: PtySize) -> Result<(), ProcessError> {
        let state = self.state.borrow();
        let master = state.master.as_ref().ok_or_else(closed_pty_error)?;
        master
            .resize(portable_pty::PtySize {
                rows: size.rows,
                cols: size.columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| ProcessError::io("resize PTY", io::Error::other(error.to_string())))
    }

    /// Returns the current pseudoterminal window geometry.
    pub fn get_size(&self) -> Result<PtySize, ProcessError> {
        let state = self.state.borrow();
        let master = state.master.as_ref().ok_or_else(closed_pty_error)?;
        master
            .get_size()
            .map(|size| PtySize { rows: size.rows, columns: size.cols })
            .map_err(|error| ProcessError::io("read PTY size", io::Error::other(error.to_string())))
    }
}

#[cfg(unix)]
fn closed_pty_error() -> ProcessError {
    ProcessError::io(
        "PTY master",
        io::Error::new(io::ErrorKind::BrokenPipe, "PTY master is closed"),
    )
}

#[cfg(unix)]
fn drain_pty_reads<R: std::io::Read>(reader: &mut R, events: &mut Vec<NetEvent>) {
    let chunk = crate::net::STREAM_CHUNK;
    loop {
        let mut data = vec![0; chunk];
        match reader.read(&mut data) {
            Ok(0) => {
                events.push(NetEvent::Eof);
                break;
            }
            Ok(read) => {
                data.truncate(read);
                events.push(NetEvent::Read(data));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_would_block(&error) => break,
            // Linux reports EIO when the slave has closed; libuv treats a
            // closed master read as end-of-stream, so map it to EOF here.
            Err(error) if error.raw_os_error() == Some(5) => {
                events.push(NetEvent::Eof);
                break;
            }
            Err(error) => {
                events.push(NetEvent::Error(NetError::Io(error)));
                break;
            }
        }
    }
}

#[cfg(unix)]
fn register_pty(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: &Rc<RefCell<PtyState>>,
    callback: &CallbackCell,
) -> NetResult<()> {
    {
        let mut state = state.borrow_mut();
        let interests = pty_interest(&state);
        let fd = state.fd.as_ref().ok_or(NetError::Closed)?.as_raw_fd();
        uv_loop.inner_mut().reactor().register(&mut SourceFd(&fd), token, interests)?;
        state.registered = true;
    }
    let shared = Rc::clone(state);
    let user_callback = Rc::clone(callback);
    let queue = uv_loop.net_dispatch_queue();
    uv_loop
        .inner_mut()
        .on_readiness(token, move |ready, _| {
            let events = pty_ready(&mut shared.borrow_mut(), ready);
            if !events.is_empty() {
                let dispatch_state = Rc::clone(&shared);
                let dispatch_callback = Rc::clone(&user_callback);
                queue_batch(&queue, move |uv_loop| {
                    deliver_pty(uv_loop, id, token, dispatch_state, dispatch_callback, events)
                });
            }
            Ok(DrainState::Drained)
        })
        .map_err(crate::Error::from)?;
    Ok(())
}

#[cfg(unix)]
fn sync_pty(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: &Rc<RefCell<PtyState>>,
) -> NetResult<()> {
    if !live(uv_loop, id) {
        return Ok(());
    }
    let active;
    {
        let state = state.borrow_mut();
        active = pty_active(&state);
        let interests = pty_interest(&state);
        if state.registered {
            let fd = state.fd.as_ref().ok_or(NetError::Closed)?.as_raw_fd();
            uv_loop.inner_mut().reactor().reregister(&mut SourceFd(&fd), token, interests)?;
        }
    }
    uv_loop.set_external_active(id, active)?;
    Ok(())
}

#[cfg(unix)]
fn pty_ready(state: &mut PtyState, ready: Readiness) -> Vec<NetEvent> {
    let mut events = Vec::new();
    if ready.writable {
        if let Some(fd) = state.fd.as_mut() {
            drive_writes(fd, &mut state.writes, &mut events);
        }
    }
    if ready.readable && state.reading {
        if let Some(fd) = state.fd.as_mut() {
            drain_pty_reads(fd, &mut events);
        }
        if events.iter().any(|event| matches!(event, NetEvent::Eof)) {
            state.reading = false;
        }
    }
    if ready.read_closed && state.reading {
        state.reading = false;
        events.push(NetEvent::Eof);
    }
    if ready.write_closed && !state.writes.pending.is_empty() {
        while let Some(write) = state.writes.pending.pop_front() {
            events.push(NetEvent::WriteComplete { id: write.id, result: Err(NetError::Closed) });
        }
    }
    events
}

#[cfg(unix)]
fn deliver_pty(
    uv_loop: &mut UvLoop,
    id: HandleId,
    token: Token,
    state: Rc<RefCell<PtyState>>,
    callback: CallbackCell,
    events: Vec<NetEvent>,
) {
    if !live(uv_loop, id) {
        return;
    }
    if let Err(error) = sync_pty(uv_loop, id, token, &state) {
        invoke(&callback, uv_loop, id, NetEvent::Error(error));
    }
    for event in events {
        if !live(uv_loop, id) {
            break;
        }
        invoke(&callback, uv_loop, id, event);
    }
}

#[cfg(unix)]
fn close_pty(uv_loop: &mut UvLoop, handle: &PtyHandle) -> crate::Result<()> {
    uv_loop.inner_mut().remove_readiness(handle.token);
    let mut state = handle.state.borrow_mut();
    if state.registered {
        if let Some(fd) = state.fd.as_ref() {
            let raw = fd.as_raw_fd();
            uv_loop.inner_mut().reactor().deregister(&mut SourceFd(&raw))?;
        }
        state.registered = false;
    }
    state.fd = None;
    state.master = None;
    state.reading = false;
    state.writes.clear();
    Ok(())
}

#[cfg(unix)]
impl Handle for PtyHandle {
    fn id(&self) -> HandleId {
        self.id
    }

    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        close_pty(uv_loop, self)?;
        uv_loop.close(
            self.id,
            None::<fn(&mut UvLoop, HandleId) -> std::result::Result<(), crate::CallbackError>>,
        )
    }

    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where
        F: FnOnce(&mut UvLoop, HandleId)
                -> std::result::Result<(), crate::CallbackError>
            + 'static,
    {
        close_pty(uv_loop, self)?;
        uv_loop.close(self.id, Some(callback))
    }
}

#[cfg(unix)]
fn build_pty_master(
    uv_loop: &mut UvLoop,
    master: Box<dyn portable_pty::MasterPty + Send>,
) -> Result<PtyHandle, ProcessError> {
    PtyHandle::wrap(uv_loop, master)
}

#[cfg(not(unix))]
fn build_pty_master(
    _uv_loop: &mut UvLoop,
    master: Box<dyn portable_pty::MasterPty + Send>,
) -> Result<Box<dyn portable_pty::MasterPty + Send>, ProcessError> {
    Ok(master)
}

/// Spawns a child attached to a native pseudoterminal.
///
/// `portable-pty` owns the platform-specific, safe child setup required for a
/// controlling terminal on Unix and ConPTY on Windows. On Unix the returned
/// master is a loop-managed [`PtyHandle`] that pumps through the owning loop;
/// elsewhere it is the platform `MasterPty`. PTY stdio is necessarily the slave
/// terminal, so `options.stdio` is ignored. PTY identity changes and detached
/// mode are rejected because `portable_pty::CommandBuilder` 0.9.0 has no
/// corresponding safe controls.
///
/// PTY spawning extends the process semantics in `luvref.txt`, `uv.spawn()` /
/// `uv.spawn-options` (lines 1340-1452).
pub fn spawn_pty<F>(
    uv_loop: &mut UvLoop,
    options: SpawnOptions,
    size: PtySize,
    on_exit: F,
) -> Result<SpawnedPty, ProcessError>
where
    F: FnOnce(&mut UvLoop, ProcessExitResult) + Send + 'static,
{
    if options.uid.is_some() || options.gid.is_some() {
        return Err(ProcessError::Unsupported {
            feature: "PTY spawn uid/gid",
            reason: "portable-pty 0.9.0 does not expose safe child identity controls",
        });
    }
    if options.detached {
        return Err(ProcessError::Unsupported {
            feature: "detached PTY spawn",
            reason: "a PTY child must remain its controlling-terminal session leader",
        });
    }

    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: size.rows,
            cols: size.columns,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| ProcessError::io("open PTY", io::Error::other(error.to_string())))?;

    let mut command = portable_pty::CommandBuilder::new(options.program.as_os_str());
    command.args(&options.args);
    if let Some(environment) = &options.environment {
        command.env_clear();
        for (name, value) in environment {
            command.env(name, value);
        }
    }
    if let Some(cwd) = &options.cwd {
        command.cwd(cwd.as_os_str());
    }

    let mut portable_child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| {
            ProcessError::io("spawn PTY child", io::Error::other(error.to_string()))
        })?;
    let pid = match portable_child.process_id() {
        Some(pid) => pid,
        None => {
            let _ = portable_child.kill();
            let _ = portable_child.wait();
            return Err(ProcessError::Unsupported {
                feature: "PTY child PID",
                reason: "the native portable-pty backend returned no process identifier",
            });
        }
    };
    drop(pair.slave);

    let child_state = ChildState::Portable(portable_child);

    let state = Arc::new(Mutex::new(child_state));
    let handle_id = match uv_loop.allocate_external(true) {
        Ok(id) => id,
        Err(error) => {
            terminate_and_reap(&state);
            return Err(error.into());
        }
    };
    let process = Process { id: handle_id, pid, child: Arc::clone(&state) };
    let master = match build_pty_master(uv_loop, pair.master) {
        Ok(master) => master,
        Err(error) => {
            let _ = process.close(uv_loop);
            return Err(error);
        }
    };
    let waiter_state = Arc::clone(&state);
    let waiter_poster = uv_loop.completion_poster();

    let waiter = std::thread::Builder::new()
        .name(format!("ox-uv-pty-{pid}"))
        .spawn(move || wait_and_post(waiter_state, waiter_poster, handle_id, on_exit));
    if let Err(error) = waiter {
        let _ = process.close(uv_loop);
        return Err(ProcessError::io("start PTY waiter", error));
    }

    Ok(SpawnedPty {
        process,
        master,
    })
}

fn map_stdio(config: StdioConfig) -> Stdio {
    match config {
        StdioConfig::Inherit => Stdio::inherit(),
        StdioConfig::Ignore => Stdio::null(),
        StdioConfig::CreatePipe => Stdio::piped(),
    }
}

#[cfg(unix)]
fn validate_platform_options(_options: &SpawnOptions) -> Result<(), ProcessError> {
    Ok(())
}

#[cfg(windows)]
fn validate_platform_options(options: &SpawnOptions) -> Result<(), ProcessError> {
    if options.uid.is_some() || options.gid.is_some() {
        return Err(ProcessError::Unsupported {
            feature: "spawn uid/gid",
            reason: "Windows does not support Unix child identities",
        });
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_platform_options(options: &SpawnOptions) -> Result<(), ProcessError> {
    if options.uid.is_some() || options.gid.is_some() || options.detached || options.hide {
        return Err(ProcessError::Unsupported {
            feature: "platform spawn options",
            reason: "uid, gid, detached, and hide are unavailable on this target",
        });
    }
    Ok(())
}

#[cfg(unix)]
fn configure_platform_command(command: &mut Command, options: &SpawnOptions) {
    if let Some(gid) = options.gid {
        command.gid(gid);
    }
    if let Some(uid) = options.uid {
        command.uid(uid);
    }
    if options.detached {
        command.process_group(0);
    }
}

#[cfg(windows)]
fn configure_platform_command(command: &mut Command, options: &SpawnOptions) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut flags = 0;
    if options.detached {
        flags |= CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS;
    }
    if options.hide {
        flags |= CREATE_NO_WINDOW;
    }
    if flags != 0 {
        command.creation_flags(flags);
    }
}

#[cfg(not(any(unix, windows)))]
fn configure_platform_command(_command: &mut Command, _options: &SpawnOptions) {}

fn wait_and_post<F>(
    child: Arc<Mutex<ChildState>>,
    poster: UvLoopPoster,
    handle_id: HandleId,
    on_exit: F,
) where
    F: FnOnce(&mut UvLoop, ProcessExitResult) + Send + 'static,
{
    loop {
        let result = {
            let mut state = lock_recover(&child);
            let result = match &mut *state {
                ChildState::Standard(child) => child.try_wait().map(|status| status.map(exit_values)),
                ChildState::Portable(child) => {
                    #[cfg(unix)]
                    {
                        // portable-pty wraps a real `std::process::Child`
                        // (its `Child` trait exposes only an exit code). Recover
                        // the genuine wait status so a signal death reports the
                        // terminating signal, matching ordinary `spawn`.
                        let std_status = {
                            let erased: &mut dyn portable_pty::Child = &mut **child;
                            erased.downcast_mut::<std::process::Child>()
                        };
                        match std_status {
                            Some(std_child) => {
                                std_child.try_wait().map(|status| status.map(exit_values))
                            }
                            None => child
                                .try_wait()
                                .map(|status| status.map(portable_exit_values)),
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        child.try_wait().map(|status| status.map(portable_exit_values))
                    }
                }
                ChildState::Exited => return,
            };
            if matches!(result, Ok(Some(_))) {
                *state = ChildState::Exited;
            }
            result
        };

        let terminal = match result {
            Ok(Some(exit)) => Ok(exit),
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => Err(ProcessError::io("wait for process", error)),
        };

        let _ = poster.post(Box::new(move |uv_loop| {
            if uv_loop.set_external_active(handle_id, false).is_ok() {
                on_exit(uv_loop, terminal);
            }
        }));
        return;
    }
}

fn terminate_and_reap(child: &Arc<Mutex<ChildState>>) {
    let mut state = lock_recover(child);
    match &mut *state {
        ChildState::Standard(child) => {
            let _ = child.kill();
            let _ = child.wait();
        }
        ChildState::Portable(child) => {
            let _ = child.kill();
            let _ = child.wait();
        }
        ChildState::Exited => {}
    }
    *state = ChildState::Exited;
}

#[cfg(windows)]
fn terminate_child(child: &mut ChildState) -> io::Result<()> {
    match child {
        ChildState::Standard(child) => child.kill(),
        ChildState::Portable(child) => child.kill(),
        ChildState::Exited => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "process has already exited",
        )),
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(unix)]
fn send_unix_signal(pid: u32, signal: Option<i32>) -> Result<(), ProcessError> {
    let pid = unix_pid(pid)?;
    let number = signal.unwrap_or(rustix::process::Signal::TERM.as_raw());
    if number == 0 {
        return rustix::process::test_kill_process(pid)
            .map_err(|error| ProcessError::io("kill", error));
    }
    let signal = rustix::process::Signal::from_named_raw(number)
        .ok_or(ProcessError::InvalidSignal(number))?;
    rustix::process::kill_process(pid, signal)
        .map_err(|error| ProcessError::io("kill", error))
}

#[cfg(unix)]
fn unix_pid(pid: u32) -> Result<rustix::process::Pid, ProcessError> {
    let raw = i32::try_from(pid).map_err(|_| ProcessError::InvalidPid(pid))?;
    rustix::process::Pid::from_raw(raw).ok_or(ProcessError::InvalidPid(pid))
}

#[cfg(unix)]
fn optional_unix_pid(pid: u32) -> Result<Option<rustix::process::Pid>, ProcessError> {
    if pid == 0 {
        Ok(None)
    } else {
        unix_pid(pid).map(Some)
    }
}

#[cfg(unix)]
fn exit_values(status: ExitStatus) -> ProcessExit {
    ProcessExit {
        code: status.code().map_or(0, i64::from),
        signal: status.signal().unwrap_or(0),
    }
}

#[cfg(not(unix))]
fn exit_values(status: ExitStatus) -> ProcessExit {
    ProcessExit {
        code: status.code().map_or(0, i64::from),
        signal: 0,
    }
}

fn portable_exit_values(status: portable_pty::ExitStatus) -> ProcessExit {
    ProcessExit {
        code: i64::from(status.exit_code()),
        signal: 0,
    }
}
