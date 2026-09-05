//! Reactor-driven child process channels used by Vimscript job control.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ox_types::{DictRef, OxStr, Typval};
use ox_uv::process::{self, Process, ProcessPipe, SpawnOptions, StdioConfig};
#[cfg(unix)]
use ox_uv::process::{PtyHandle, PtySize};
use ox_uv::{NetEvent, UvLoop};

/// Callback values and their dictionary receiver from `jobstart()` options.
#[derive(Clone)]
pub struct JobCallbacks {
    /// Original options dictionary, bound as callback `self`.
    pub options: DictRef,
    /// Standard-output callback.
    pub stdout: Option<Typval>,
    /// Standard-error callback.
    pub stderr: Option<Typval>,
    /// Process-exit callback.
    pub exit: Option<Typval>,
}

/// Normalized options for one child process.
#[expect(
    clippy::struct_excessive_bools,
    reason = "these independent Vim job options do not form one state machine"
)]
pub struct JobStartOptions {
    /// Executable path.
    pub program: PathBuf,
    /// Arguments after the executable.
    pub args: Vec<OsString>,
    /// Replacement environment, or inherited environment when absent.
    pub environment: Option<Vec<(OsString, OsString)>>,
    /// Child working directory.
    pub cwd: Option<PathBuf>,
    /// Whether the child outlives editor shutdown.
    pub detached: bool,
    /// Whether stdio uses a pseudoterminal.
    pub pty: bool,
    /// Whether the pseudoterminal backs a `:terminal` buffer.
    pub term: bool,
    /// Whether the channel carries msgpack-rpc.
    pub rpc: bool,
    /// Whether stdin is connected to a writable pipe.
    pub stdin_pipe: bool,
    /// Whether stdout is delivered once at EOF.
    pub stdout_buffered: bool,
    /// Whether stderr is delivered once at EOF.
    pub stderr_buffered: bool,
    /// Editor buffer that receives PTY output as live text, when allocated.
    pub terminal_buffer: Option<ox_types::BufHandle>,
    /// Deferred callbacks and their receiver.
    pub callbacks: JobCallbacks,
}

/// A callback invocation deferred until after the reactor callback returns.
#[derive(Clone)]
pub struct JobEvent {
    /// Function, partial, or Lua reference to invoke.
    pub callback: Typval,
    /// Options dictionary bound as Vimscript `self`.
    pub receiver: DictRef,
    /// Upstream-compatible callback arguments.
    pub args: Vec<Typval>,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

enum RawEvent {
    Data(u64, StreamKind, Vec<u8>),
    Eof(u64, StreamKind),
    Exit(u64, Result<process::ProcessExit, String>),
}

#[derive(Default)]
struct StreamState {
    buffered: bool,
    bytes: Vec<u8>,
    eof: bool,
}

#[cfg(unix)]
enum JobInput {
    Pipe(ProcessPipe),
    Pty(PtyHandle),
}

#[cfg(not(unix))]
enum JobInput {
    Pipe(ProcessPipe),
}

struct Job {
    process: Process,
    input: Option<JobInput>,
    _stdout_pipe: Option<ProcessPipe>,
    _stderr_pipe: Option<ProcessPipe>,
    callbacks: JobCallbacks,
    stdout: StreamState,
    stderr: StreamState,
    status: i64,
    rpc: bool,
    /// PTY slave path the child is attached to, when spawned through a
    /// pseudoterminal. Mirrors upstream's `channel.pty` string populated for
    /// `jobstart(..., {'pty': v:true})` (`eval/funcs.c` `f_jobstart` →
    /// `os/shell.c` `terminal_running`) so `nvim_get_chan_info` can answer the
    /// `pty` field word-for-word.
    pty_slave: Option<String>,
    /// Buffer that receives PTY output, when one was allocated.
    terminal_buffer: Option<ox_types::BufHandle>,
    /// Raw PTY output accumulated for the terminal buffer.
    pty_output: Vec<u8>,
    /// Whether the default `nvim.terminal` `TermClose` handler is active.
    terminal_exit_message: bool,
    /// `jobstart({'detach': v:true})`: upstream leaves a detached child running
    /// past editor exit and terminates every other one (`channel_close_on_exit`).
    detached: bool,
}
/// Resolve the PTY slave path behind a child that was spawned through one.
///
/// `portable_pty` does not expose the slave path taken by the child, but on
/// Linux the child ends up with the slave on one of standard input, output,
/// or error; reading `/proc/<pid>/fd/0`'s link target is the cheapest stable
/// lookup and mirrors what `ps`/`lsof` prints.
#[cfg(unix)]
fn pty_slave_path(pid: u32) -> Option<String> {
    let link = std::fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    let text = link.to_string_lossy().into_owned();
    if text.starts_with("/dev/pts/") {
        Some(text)
    } else {
        None
    }
}

/// Owns job channels and the `ox-uv` loop which drives their process handles.
pub struct JobManager {
    loop_: UvLoop,
    jobs: HashMap<u64, Job>,
    raw: Arc<Mutex<VecDeque<RawEvent>>>,
    /// Events polled on a path with no callback host; the next poll re-enters
    /// them (`channel.c` defers callbacks onto the main loop instead of
    /// dropping them, which is what `let _ = poll()` did).
    deferred: Vec<JobEvent>,
}

impl JobManager {
    /// Create an isolated reactor-backed job table.
    ///
    /// # Errors
    ///
    /// Returns the reactor initialization error.
    pub fn new() -> Result<Self, String> {
        let loop_ = UvLoop::new().map_err(|error| error.to_string())?;
        Ok(Self {
            loop_,
            jobs: HashMap::new(),
            raw: Arc::new(Mutex::new(VecDeque::new())),
            deferred: Vec::new(),
        })
    }

    /// Spawn a process and register it under the already-allocated channel id.
    ///
    /// # Errors
    ///
    /// Returns the process or stream setup error.
    #[expect(
        clippy::too_many_lines,
        reason = "the Unix and non-Unix spawn paths share one resource-ownership boundary"
    )]
    pub fn start(&mut self, id: u64, options: JobStartOptions) -> Result<u32, String> {
        let mut spawn_options = SpawnOptions::new(options.program);
        spawn_options.args = options.args;
        spawn_options.environment = options.environment;
        spawn_options.cwd = options.cwd;
        spawn_options.detached = options.detached;
        spawn_options.stdio = [
            if options.stdin_pipe {
                StdioConfig::CreatePipe
            } else {
                StdioConfig::Ignore
            },
            StdioConfig::CreatePipe,
            StdioConfig::CreatePipe,
        ];

        let exit_queue = Arc::clone(&self.raw);
        let on_exit = move |_loop_: &mut UvLoop, result: process::ProcessExitResult| {
            let result = result.map_err(|error| error.to_string());
            lock_queue(&exit_queue).push_back(RawEvent::Exit(id, result));
        };

        #[cfg(unix)]
        let (process, input, stdout_pipe, stderr_pipe) = if options.pty {
            let mut spawned = process::spawn_pty(
                &mut self.loop_,
                spawn_options,
                PtySize {
                    columns: 80,
                    rows: 24,
                },
                on_exit,
            )
            .map_err(|error| error.to_string())?;
            let output_queue = Arc::clone(&self.raw);
            spawned
                .master
                .read_start(&mut self.loop_, move |_loop_, _handle, event| {
                    queue_stream_event(&output_queue, id, StreamKind::Stdout, event);
                })
                .map_err(|error| error.to_string())?;
            (
                spawned.process,
                Some(JobInput::Pty(spawned.master)),
                None,
                None,
            )
        } else {
            let mut spawned = process::spawn(&mut self.loop_, spawn_options, on_exit)
                .map_err(|error| error.to_string())?;
            let input = spawned.pipes.stdin.take().map(JobInput::Pipe);
            let mut stdout_pipe = spawned.pipes.stdout.take();
            let mut stderr_pipe = spawned.pipes.stderr.take();
            if let Some(pipe) = stdout_pipe.as_mut() {
                let output_queue = Arc::clone(&self.raw);
                pipe.read_start(&mut self.loop_, move |_loop_, _handle, event| {
                    queue_stream_event(&output_queue, id, StreamKind::Stdout, event);
                })
                .map_err(|error| error.to_string())?;
            }
            if let Some(pipe) = stderr_pipe.as_mut() {
                let output_queue = Arc::clone(&self.raw);
                pipe.read_start(&mut self.loop_, move |_loop_, _handle, event| {
                    queue_stream_event(&output_queue, id, StreamKind::Stderr, event);
                })
                .map_err(|error| error.to_string())?;
            }
            (spawned.process, input, stdout_pipe, stderr_pipe)
        };

        #[cfg(not(unix))]
        let (process, input, stdout_pipe, stderr_pipe) = {
            if options.pty {
                return Err("jobstart pty is unavailable on this platform".to_owned());
            }
            let mut spawned = process::spawn(&mut self.loop_, spawn_options, on_exit)
                .map_err(|error| error.to_string())?;
            let input = spawned.pipes.stdin.take().map(JobInput::Pipe);
            (spawned.process, input, None, None)
        };

        let pid = process.pid();
        #[cfg(unix)]
        let pty_slave = if options.pty {
            pty_slave_path(pid)
        } else {
            None
        };
        #[cfg(not(unix))]
        let pty_slave = None;
        self.jobs.insert(
            id,
            Job {
                process,
                input,
                _stdout_pipe: stdout_pipe,
                _stderr_pipe: stderr_pipe,
                callbacks: options.callbacks,
                stdout: StreamState {
                    buffered: options.stdout_buffered,
                    ..StreamState::default()
                },
                stderr: StreamState {
                    buffered: options.stderr_buffered,
                    ..StreamState::default()
                },
                status: -1,
                rpc: options.rpc,
                pty_slave,
                terminal_buffer: options.terminal_buffer,
                pty_output: Vec::new(),
                terminal_exit_message: true,
                detached: options.detached,
            },
        );
        Ok(pid)
    }

    /// Run one non-blocking reactor turn and return callback work queued by
    /// it, ahead of any events deferred by a poll that could not deliver them.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling error.
    pub fn poll(&mut self) -> Result<Vec<JobEvent>, String> {
        self.loop_.run_nowait().map_err(|error| error.to_string())?;
        let mut events = std::mem::take(&mut self.deferred);
        events.extend(self.drain_raw());
        Ok(events)
    }

    /// Stash events a caller polled but could not invoke; the next poll
    /// surfaces them first so callbacks are never silently discarded.
    pub fn defer_events(&mut self, events: Vec<JobEvent>) {
        self.deferred.extend(events);
    }

    /// Wait for the selected jobs, sharing one deadline across the list.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling error.
    pub fn wait(
        &mut self,
        ids: &[u64],
        timeout_ms: i64,
    ) -> Result<(Vec<i64>, Vec<JobEvent>), String> {
        let deadline = if timeout_ms < 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms.cast_unsigned()))
        };
        let mut events = Vec::new();
        loop {
            match self.poll() {
                Ok(mut polled) => events.append(&mut polled),
                Err(error) => {
                    // Keep the events gathered so far deliverable instead of
                    // dropping them with the error.
                    self.defer_events(events);
                    return Err(error);
                }
            }
            if ids
                .iter()
                .all(|id| self.jobs.get(id).is_none_or(|job| job.status >= 0))
            {
                // EOF readiness can trail the waiter notification by one turn.
                for _ in 0..4 {
                    match self.poll() {
                        Ok(mut polled) => events.append(&mut polled),
                        Err(error) => {
                            self.defer_events(events);
                            return Err(error);
                        }
                    }
                }
                break;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let statuses = ids
            .iter()
            .map(|id| self.jobs.get(id).map_or(-3, |job| job.status))
            .collect();
        Ok((statuses, events))
    }

    /// Write raw bytes to the process stdin or PTY master.
    ///
    /// # Errors
    ///
    /// Returns the process input write error.
    pub fn send(&mut self, id: u64, data: Vec<u8>) -> Result<bool, String> {
        let Some(job) = self.jobs.get_mut(&id) else {
            return Ok(false);
        };
        let Some(input) = job.input.as_mut() else {
            return Ok(false);
        };
        match input {
            JobInput::Pipe(pipe) => {
                pipe.write(&mut self.loop_, data)
                    .map_err(|error| error.to_string())?;
            }
            #[cfg(unix)]
            JobInput::Pty(pty) => {
                pty.write(&mut self.loop_, data)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(true)
    }

    /// Close a job's writable input endpoint so readers observe EOF.
    ///
    /// Dropping the handle is not enough. A [`ProcessPipe`] shares its stream
    /// state with the clone the loop's reactor holds, so letting the handle go
    /// left the child's standard input open: `systemlist('cat', '123')` wrote
    /// its input, dropped the handle, and then waited forever on a `cat` that
    /// never saw EOF. That was the only timeout in oldtest census 3, and the
    /// `/bin/sh -c cat` it left behind outlived the runner's deadline.
    ///
    /// `Handle::close` is what actually deregisters the descriptor and drops it.
    /// A write the synchronous attempt could not finish is still queued, so the
    /// loop is pumped until the queue drains first — the reactor clears it on
    /// either a completed write or a closed peer, so the pump terminates.
    pub fn close_input(&mut self, id: u64) -> bool {
        let Some(job) = self.jobs.get_mut(&id) else {
            return false;
        };
        let Some(input) = job.input.take() else {
            return false;
        };
        match input {
            JobInput::Pipe(pipe) => {
                let deadline = Instant::now() + Duration::from_secs(30);
                while pipe.has_pending_writes() && Instant::now() < deadline {
                    if self.loop_.run_nowait().is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                // `shutdown` refuses while a write is queued, so it could leave
                // the descriptor open on a stuck peer; the full handle close
                // cannot, and a descriptor still open is the whole defect.
                let _ignored = ox_uv::Handle::close(&pipe, &mut self.loop_);
            }
            // A PTY has no separate write side to shut: closing the master is
            // the child's hangup, which `stop`/teardown already performs.
            #[cfg(unix)]
            JobInput::Pty(_) => {}
        }
        true
    }

    /// Take output accumulated by buffered streams after the job completes.
    pub fn take_buffered_output(&mut self, id: u64) -> Option<(Vec<u8>, Vec<u8>)> {
        let job = self.jobs.get_mut(&id)?;
        Some((
            std::mem::take(&mut job.stdout.bytes),
            std::mem::take(&mut job.stderr.bytes),
        ))
    }

    /// Send SIGTERM to a live job. An already-reaped job is a successful no-op.
    ///
    /// # Errors
    ///
    /// Returns the process termination error.
    pub fn stop(&mut self, id: u64) -> Result<bool, String> {
        let Some(job) = self.jobs.get(&id) else {
            return Ok(false);
        };
        if job.status < 0 {
            job.process.kill(None).map_err(|error| error.to_string())?;
        }
        Ok(true)
    }

    #[must_use]
    /// Return the child PID for a registered job.
    pub fn pid(&self, id: u64) -> Option<u32> {
        self.jobs.get(&id).map(|job| job.process.pid())
    }

    #[must_use]
    /// Report whether a registered job channel carries msgpack-rpc.
    pub fn is_rpc(&self, id: u64) -> bool {
        self.jobs.get(&id).is_some_and(|job| job.rpc)
    }

    /// PTY slave path behind a job channel that was spawned through one.
    #[must_use]
    pub fn pty_slave(&self, id: u64) -> Option<&str> {
        self.jobs.get(&id).and_then(|job| job.pty_slave.as_deref())
    }

    /// Bind the editor-owned terminal buffer to an already-started PTY job.
    pub fn set_terminal_buffer(&mut self, id: u64, buffer: ox_types::BufHandle) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.terminal_buffer = Some(buffer);
        }
    }

    /// Enable or disable the default terminal process-exit message.
    pub fn set_terminal_exit_message(&mut self, id: u64, enabled: bool) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.terminal_exit_message = enabled;
        }
    }

    /// Take accumulated PTY output for the terminal buffer.
    pub fn take_pty_output(&mut self, id: u64) -> Option<Vec<u8>> {
        let job = self.jobs.get_mut(&id)?;
        if job.pty_output.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut job.pty_output))
    }

    /// Poll once and write any PTY output to the editor-owned terminal buffer.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling or terminal-buffer update error.
    pub fn flush_pty_to_editor(
        &mut self,
        id: u64,
        editor: &mut crate::Editor,
    ) -> Result<(), String> {
        let events = self.poll()?;
        self.defer_events(events);
        if let Some(bytes) = self.take_pty_output(id) {
            editor
                .append_terminal_buffer(id, &bytes)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Poll once and take pending output for every terminal-backed job.
    ///
    /// Callback events remain deferred for the normal callback host.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling error.
    pub fn take_all_pty_output(&mut self) -> Result<Vec<(u64, Vec<u8>)>, String> {
        let events = self.poll()?;
        self.defer_events(events);
        let output = self
            .jobs
            .iter_mut()
            .filter_map(|(&id, job)| {
                (!job.pty_output.is_empty()).then(|| (id, std::mem::take(&mut job.pty_output)))
            })
            .collect::<Vec<_>>();
        let cursor_queries = output
            .iter()
            .filter_map(|(id, bytes)| {
                bytes
                    .windows(4)
                    .any(|bytes| bytes == b"\x1b[6n")
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in cursor_queries {
            self.send(id, b"\x1b[1;1R".to_vec())?;
        }
        Ok(output)
    }

    fn drain_raw(&mut self) -> Vec<JobEvent> {
        let raw = {
            let mut queue = lock_queue(&self.raw);
            queue.drain(..).collect::<Vec<_>>()
        };
        let mut callbacks = Vec::new();
        for event in raw {
            match event {
                RawEvent::Data(id, stream, bytes) => {
                    let Some(job) = self.jobs.get_mut(&id) else {
                        continue;
                    };
                    // PTY output accumulates for the terminal buffer.
                    if job.terminal_buffer.is_some() && matches!(stream, StreamKind::Stdout) {
                        job.pty_output.extend_from_slice(&bytes);
                    }
                    let (state, callback, name) = stream_parts(job, stream);
                    if state.buffered {
                        state.bytes.extend_from_slice(&bytes);
                    } else if let Some(callback) = callback {
                        callbacks.push(data_event(
                            id,
                            callback,
                            &job.callbacks.options,
                            name,
                            &bytes,
                        ));
                    }
                }
                RawEvent::Eof(id, stream) => {
                    let Some(job) = self.jobs.get_mut(&id) else {
                        continue;
                    };
                    let (state, callback, name) = stream_parts(job, stream);
                    if state.eof {
                        continue;
                    }
                    state.eof = true;
                    if let Some(callback) = callback {
                        let bytes = if state.buffered {
                            std::mem::take(&mut state.bytes)
                        } else {
                            Vec::new()
                        };
                        callbacks.push(data_event(
                            id,
                            callback,
                            &job.callbacks.options,
                            name,
                            &bytes,
                        ));
                    }
                }
                RawEvent::Exit(id, result) => {
                    let Some(job) = self.jobs.get_mut(&id) else {
                        continue;
                    };
                    let status = match result {
                        Ok(exit) if exit.signal != 0 => i64::from(128 + exit.signal),
                        Ok(exit) => exit.code,
                        Err(_) => -2,
                    };
                    job.status = status;
                    if job.terminal_buffer.is_some() && job.terminal_exit_message {
                        job.pty_output.extend_from_slice(
                            format!("\r\n[Process exited {status}]\r\n").as_bytes(),
                        );
                    }
                    if let Some(callback) = job.callbacks.exit.clone() {
                        callbacks.push(JobEvent {
                            callback,
                            receiver: job.callbacks.options.clone(),
                            args: vec![
                                Typval::Number(id.cast_signed()),
                                Typval::Number(status),
                                Typval::String(OxStr::from("exit")),
                            ],
                        });
                    }
                }
            }
        }
        callbacks
    }
}

/// No non-detached child outlives the manager that spawned it.
///
/// Upstream terminates every job channel on exit and leaves only `detach`ed
/// ones running (`channel_close_on_exit`). Without this a child still blocked
/// when the editor goes away becomes an orphan holding the inherited standard
/// output, which is what made the census's `test_system.vim` timeout hang a
/// runner reading that pipe rather than just failing.
impl Drop for JobManager {
    fn drop(&mut self) {
        for job in self.jobs.values() {
            if job.status < 0 && !job.detached {
                let _ignored = ox_uv::Handle::close(&job.process, &mut self.loop_);
            }
        }
    }
}

fn stream_parts(
    job: &mut Job,
    stream: StreamKind,
) -> (&mut StreamState, Option<Typval>, &'static str) {
    match stream {
        StreamKind::Stdout => (&mut job.stdout, job.callbacks.stdout.clone(), "stdout"),
        StreamKind::Stderr => (&mut job.stderr, job.callbacks.stderr.clone(), "stderr"),
    }
}

fn data_event(
    id: u64,
    callback: Typval,
    receiver: &DictRef,
    name: &'static str,
    bytes: &[u8],
) -> JobEvent {
    let lines = if bytes.is_empty() {
        vec![Typval::String(OxStr(Vec::new()))]
    } else {
        bytes
            .split(|byte| *byte == b'\n')
            .map(|line| Typval::String(OxStr(line.strip_suffix(b"\r").unwrap_or(line).to_vec())))
            .collect()
    };
    JobEvent {
        callback,
        receiver: receiver.clone(),
        args: vec![
            Typval::Number(id.cast_signed()),
            Typval::list(lines),
            Typval::String(OxStr::from(name)),
        ],
    }
}

fn queue_stream_event(
    queue: &Arc<Mutex<VecDeque<RawEvent>>>,
    id: u64,
    stream: StreamKind,
    event: NetEvent,
) {
    let event = match event {
        NetEvent::Read(bytes) => Some(RawEvent::Data(id, stream, bytes)),
        NetEvent::Eof | NetEvent::Error(_) => Some(RawEvent::Eof(id, stream)),
        _ => None,
    };
    if let Some(event) = event {
        lock_queue(queue).push_back(event);
    }
}

fn lock_queue(
    queue: &Arc<Mutex<VecDeque<RawEvent>>>,
) -> std::sync::MutexGuard<'_, VecDeque<RawEvent>> {
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn options(command: &str, buffered: bool) -> JobStartOptions {
        let options = Typval::dict(Vec::new());
        let Typval::Dict(reference) = options else {
            unreachable!()
        };
        let callback = Typval::String(OxStr::from("Callback"));
        JobStartOptions {
            program: PathBuf::from("sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            environment: None,
            cwd: None,
            detached: false,
            pty: false,
            term: false,
            rpc: false,
            stdin_pipe: true,
            stdout_buffered: buffered,
            stderr_buffered: buffered,
            terminal_buffer: None,
            callbacks: JobCallbacks {
                options: reference,
                stdout: Some(callback.clone()),
                stderr: Some(callback.clone()),
                exit: Some(callback),
            },
        }
    }

    /// Buffered with no callbacks, which is how `system()`/`systemlist()`
    /// configure a job: `drain_raw` hands a buffered stream's bytes to the EOF
    /// callback when there is one, so only this shape leaves them for
    /// `take_buffered_output`.
    fn collected(command: &str) -> JobStartOptions {
        let Typval::Dict(reference) = Typval::dict(Vec::new()) else {
            unreachable!()
        };
        JobStartOptions {
            callbacks: JobCallbacks {
                options: reference,
                stdout: None,
                stderr: None,
                exit: None,
            },
            ..options(command, true)
        }
    }

    fn event_name(event: &JobEvent) -> &str {
        match event.args.get(2) {
            Some(Typval::String(name)) => match name.as_bytes() {
                b"stdout" => "stdout",
                b"stderr" => "stderr",
                b"exit" => "exit",
                _ => "unknown",
            },
            _ => "unknown",
        }
    }

    #[test]
    fn buffered_output_and_exit_are_deferred_through_the_loop() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("printf 'alpha\nbeta'; printf 'err' >&2", true))
            .unwrap();
        let (status, events) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
        assert!(events.iter().any(|event| event_name(event) == "stdout"));
        assert!(events.iter().any(|event| event_name(event) == "stderr"));
        assert!(events.iter().any(|event| event_name(event) == "exit"));
    }

    // `close_input` must close the descriptor, not just drop the handle: the
    // loop holds its own clone of the stream state, so dropping alone left the
    // child's standard input open. `cat` is the exact shape that exposes it —
    // it reads to EOF, so without the close it never exits and the bounded
    // wait below reports the timeout sentinel instead of a status.
    //
    // Three parts, each isolating one: the child must exit (a leaked
    // descriptor leaves it running), its status must be the real 0 (a killed
    // child would not be), and the input must arrive intact (a close that
    // truncated the queued write would lose it).
    #[test]
    fn close_input_gives_a_reading_child_eof_after_its_input() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(7, collected("cat")).unwrap();
        assert!(jobs.send(7, b"123".to_vec()).unwrap());
        assert!(jobs.close_input(7));
        let (status, _) = jobs.wait(&[7], 5_000).unwrap();
        assert_eq!(status, vec![0], "child did not exit: stdin was left open");
        let (stdout, _) = jobs.take_buffered_output(7).unwrap();
        assert_eq!(stdout, b"123", "queued input was truncated by the close");
    }

    // A large input cannot fit one pipe buffer, so the synchronous write leaves
    // a remainder queued. The close has to pump the loop until it drains rather
    // than discard it.
    #[test]
    fn close_input_flushes_a_write_larger_than_one_pipe_buffer() {
        let payload = vec![b'x'; 512 * 1024];
        let mut jobs = JobManager::new().unwrap();
        jobs.start(8, collected("cat")).unwrap();
        assert!(jobs.send(8, payload.clone()).unwrap());
        assert!(jobs.close_input(8));
        let (status, _) = jobs.wait(&[8], 20_000).unwrap();
        assert_eq!(status, vec![0]);
        let (stdout, _) = jobs.take_buffered_output(8).unwrap();
        assert_eq!(
            stdout.len(),
            payload.len(),
            "the queued remainder was dropped"
        );
    }

    // Dropping the manager terminates a child that is still running, so no
    // child outlives the parent that was waiting on it. A detached child is the
    // one exception upstream makes.
    #[test]
    fn dropping_the_manager_terminates_a_live_child_but_not_a_detached_one() {
        let live_pid;
        let detached_pid;
        {
            let mut jobs = JobManager::new().unwrap();
            live_pid = jobs.start(9, options("sleep 60", true)).unwrap();
            let mut detached = options("sleep 60", true);
            detached.detached = true;
            detached_pid = jobs.start(10, detached).unwrap();
            // One turn so both children are really running before the drop.
            jobs.wait(&[9, 10], 200).unwrap();
        }
        assert!(
            !process_is_alive(live_pid),
            "a live child outlived its manager"
        );
        assert!(
            process_is_alive(detached_pid),
            "a detached child was terminated"
        );
        let _ignored = ox_uv::process::kill(detached_pid, Some(9));
    }

    fn process_is_alive(pid: u32) -> bool {
        // A terminated child is reaped by the manager's waiter, so the pid is
        // gone rather than a zombie. Retry briefly: SIGTERM delivery and the
        // reap are not instantaneous.
        for _ in 0..50 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn stdin_write_reaches_the_child() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("grep -qx hello", false)).unwrap();
        assert!(jobs.send(3, b"hello\n".to_vec()).unwrap());
        let (status, _) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
    }

    #[test]
    fn timeout_invalid_ids_and_sigterm_status_match_jobwait_contract() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("sleep 10", false)).unwrap();
        let (status, _) = jobs.wait(&[3, 999], 5).unwrap();
        assert_eq!(status, vec![-1, -3]);
        assert!(!jobs.send(999, Vec::new()).unwrap());
        assert!(!jobs.stop(999).unwrap());
        assert!(jobs.stop(3).unwrap());
        let (status, _) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![143]);
    }

    #[test]
    fn cwd_and_environment_are_applied_to_the_spawn() {
        let mut job_options = options(
            "printenv OX_JOB_VALUE | grep -qx set && pwd | grep -qx /tmp",
            false,
        );
        let mut environment = std::env::vars_os().collect::<Vec<_>>();
        environment.push((OsString::from("OX_JOB_VALUE"), OsString::from("set")));
        job_options.environment = Some(environment);
        job_options.cwd = Some(PathBuf::from("/tmp"));
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, job_options).unwrap();
        let (status, _) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
    }

    // If a caller polls the reactor but has no callback host available, the
    // manager must keep those events and return them on the next poll so
    // `chansend`/`take_pty_output` paths never drop an `on_stdout`/`on_exit`.
    #[test]
    fn deferred_events_are_redelivered_by_the_next_poll() {
        let mut jobs = JobManager::new().unwrap();
        let options = Typval::dict(Vec::new());
        let Typval::Dict(reference) = options else {
            unreachable!()
        };
        let event = JobEvent {
            callback: Typval::String(OxStr::from("Callback")),
            receiver: reference,
            args: vec![
                Typval::Number(1),
                Typval::list(vec![Typval::String(OxStr::from("data"))]),
                Typval::String(OxStr::from("stdout")),
            ],
        };
        jobs.defer_events(vec![event.clone(), event.clone()]);
        let first = jobs.poll().unwrap();
        assert_eq!(
            first.len(),
            2,
            "deferred events must be returned on the next poll"
        );
        assert!(
            jobs.poll().unwrap().is_empty(),
            "deferred events must be drained after one poll"
        );
    }
}
