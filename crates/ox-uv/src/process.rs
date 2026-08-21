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
#[derive(Debug, Default)]
pub struct ProcessPipes {
    /// Writable parent endpoint connected to child stdin.
    pub stdin: Option<ChildStdin>,
    /// Readable parent endpoint connected to child stdout.
    pub stdout: Option<ChildStdout>,
    /// Readable parent endpoint connected to child stderr.
    pub stderr: Option<ChildStderr>,
}

/// A spawned and asynchronously reaped child plus its created pipes.
#[derive(Debug)]
pub struct SpawnedProcess {
    /// Process control handle.
    pub process: Process,
    /// Parent-side pipe endpoints requested in the spawn options.
    pub pipes: ProcessPipes,
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
    let pipes = ProcessPipes {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
    };
    let state = Arc::new(Mutex::new(ChildState::Standard(child)));
    let handle_id = match uv_loop.allocate_external(true) {
        Ok(id) => id,
        Err(error) => {
            terminate_and_reap(&state);
            return Err(error.into());
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
pub struct SpawnedPty {
    /// Child process handle.
    pub process: Process,
    /// Duplex parent-side PTY master.
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

/// Spawns a child attached to a native pseudoterminal.
///
/// `portable-pty` owns the platform-specific, safe child setup required for a
/// controlling terminal on Unix and ConPTY on Windows. The returned master is
/// duplex: clone a reader and take its writer through the `MasterPty` methods.
/// PTY stdio is necessarily the slave terminal, so `options.stdio` is ignored.
/// PTY identity changes and detached mode are rejected because
/// `portable_pty::CommandBuilder` 0.9.0 has no corresponding safe controls.
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
        master: pair.master,
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
                ChildState::Portable(child) => child
                    .try_wait()
                    .map(|status| status.map(portable_exit_values)),
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
