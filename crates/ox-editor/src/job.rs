//! Reactor-driven child process channels used by Vimscript job control.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsString};
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
    /// Whether the channel carries msgpack-rpc.
    pub rpc: bool,
    /// Whether stdin is connected to a writable pipe.
    pub stdin_pipe: bool,
    /// Whether stdout is delivered once at EOF.
    pub stdout_buffered: bool,
    /// Whether stderr is delivered once at EOF.
    pub stderr_buffered: bool,
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
}

/// Owns job channels and the `ox-uv` loop which drives their process handles.
pub struct JobManager {
    loop_: UvLoop,
    jobs: HashMap<u64, Job>,
    raw: Arc<Mutex<VecDeque<RawEvent>>>,
}

impl JobManager {
    /// Create an isolated reactor-backed job table.
    pub fn new() -> Result<Self, String> {
        let loop_ = UvLoop::new().map_err(|error| error.to_string())?;
        Ok(Self { loop_, jobs: HashMap::new(), raw: Arc::new(Mutex::new(VecDeque::new())) })
    }

    /// Spawn a process and register it under the already-allocated channel id.
    pub fn start(&mut self, id: u64, options: JobStartOptions) -> Result<u32, String> {
        let mut spawn_options = SpawnOptions::new(options.program);
        spawn_options.args = options.args;
        spawn_options.environment = options.environment;
        spawn_options.cwd = options.cwd;
        spawn_options.detached = options.detached;
        spawn_options.stdio = [
            if options.stdin_pipe { StdioConfig::CreatePipe } else { StdioConfig::Ignore },
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
                PtySize { columns: 80, rows: 24 },
                on_exit,
            )
            .map_err(|error| error.to_string())?;
            let output_queue = Arc::clone(&self.raw);
            spawned.master.read_start(&mut self.loop_, move |_loop_, _handle, event| {
                queue_stream_event(&output_queue, id, StreamKind::Stdout, event);
            })
            .map_err(|error| error.to_string())?;
            (spawned.process, Some(JobInput::Pty(spawned.master)), None, None)
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
        self.jobs.insert(id, Job {
            process,
            input,
            _stdout_pipe: stdout_pipe,
            _stderr_pipe: stderr_pipe,
            callbacks: options.callbacks,
            stdout: StreamState { buffered: options.stdout_buffered, ..StreamState::default() },
            stderr: StreamState { buffered: options.stderr_buffered, ..StreamState::default() },
            status: -1,
            rpc: options.rpc,
        });
        Ok(pid)
    }

    /// Run one non-blocking reactor turn and return callback work queued by it.
    pub fn poll(&mut self) -> Result<Vec<JobEvent>, String> {
        self.loop_.run_nowait().map_err(|error| error.to_string())?;
        Ok(self.drain_raw())
    }

    /// Wait for the selected jobs, sharing one deadline across the list.
    pub fn wait(&mut self, ids: &[u64], timeout_ms: i64) -> Result<(Vec<i64>, Vec<JobEvent>), String> {
        let deadline = if timeout_ms < 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
        };
        let mut events = Vec::new();
        loop {
            events.extend(self.poll()?);
            if ids.iter().all(|id| self.jobs.get(id).is_none_or(|job| job.status >= 0)) {
                // EOF readiness can trail the waiter notification by one turn.
                for _ in 0..4 {
                    events.extend(self.poll()?);
                }
                break;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let statuses = ids.iter().map(|id| self.jobs.get(id).map_or(-3, |job| job.status)).collect();
        Ok((statuses, events))
    }

    /// Write raw bytes to the process stdin or PTY master.
    pub fn send(&mut self, id: u64, data: Vec<u8>) -> Result<bool, String> {
        let Some(job) = self.jobs.get_mut(&id) else { return Ok(false); };
        let Some(input) = job.input.as_mut() else { return Ok(false); };
        match input {
            JobInput::Pipe(pipe) => { pipe.write(&mut self.loop_, data).map_err(|error| error.to_string())?; }
            #[cfg(unix)]
            JobInput::Pty(pty) => { pty.write(&mut self.loop_, data).map_err(|error| error.to_string())?; }
        }
        Ok(true)
    }

    /// Close a job's writable input endpoint so readers observe EOF.
    pub fn close_input(&mut self, id: u64) -> bool {
        self.jobs.get_mut(&id).is_some_and(|job| job.input.take().is_some())
    }

    /// Take output accumulated by buffered streams after the job completes.
    pub fn take_buffered_output(&mut self, id: u64) -> Option<(Vec<u8>, Vec<u8>)> {
        let job = self.jobs.get_mut(&id)?;
        Some((std::mem::take(&mut job.stdout.bytes), std::mem::take(&mut job.stderr.bytes)))
    }

    /// Send SIGTERM to a live job. An already-reaped job is a successful no-op.
    pub fn stop(&mut self, id: u64) -> Result<bool, String> {
        let Some(job) = self.jobs.get(&id) else { return Ok(false); };
        if job.status < 0 {
            job.process.kill(None).map_err(|error| error.to_string())?;
        }
        Ok(true)
    }

    #[must_use]
    /// Return the child PID for a registered job.
    pub fn pid(&self, id: u64) -> Option<u32> { self.jobs.get(&id).map(|job| job.process.pid()) }

    #[must_use]
    /// Report whether a registered job channel carries msgpack-rpc.
    pub fn is_rpc(&self, id: u64) -> bool { self.jobs.get(&id).is_some_and(|job| job.rpc) }

    fn drain_raw(&mut self) -> Vec<JobEvent> {
        let raw = {
            let mut queue = lock_queue(&self.raw);
            queue.drain(..).collect::<Vec<_>>()
        };
        let mut callbacks = Vec::new();
        for event in raw {
            match event {
                RawEvent::Data(id, stream, bytes) => {
                    let Some(job) = self.jobs.get_mut(&id) else { continue; };
                    let (state, callback, name) = stream_parts(job, stream);
                    if state.buffered {
                        state.bytes.extend_from_slice(&bytes);
                    } else if let Some(callback) = callback {
                        callbacks.push(data_event(id, callback, &job.callbacks.options, name, bytes));
                    }
                }
                RawEvent::Eof(id, stream) => {
                    let Some(job) = self.jobs.get_mut(&id) else { continue; };
                    let (state, callback, name) = stream_parts(job, stream);
                    if state.eof { continue; }
                    state.eof = true;
                    if let Some(callback) = callback {
                        let bytes = if state.buffered { std::mem::take(&mut state.bytes) } else { Vec::new() };
                        callbacks.push(data_event(id, callback, &job.callbacks.options, name, bytes));
                    }
                }
                RawEvent::Exit(id, result) => {
                    let Some(job) = self.jobs.get_mut(&id) else { continue; };
                    let status = match result {
                        Ok(exit) if exit.signal != 0 => i64::from(128 + exit.signal),
                        Ok(exit) => exit.code,
                        Err(_) => -2,
                    };
                    job.status = status;
                    if let Some(callback) = job.callbacks.exit.clone() {
                        callbacks.push(JobEvent {
                            callback,
                            receiver: job.callbacks.options.clone(),
                            args: vec![
                                Typval::Number(id as i64),
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

fn stream_parts(job: &mut Job, stream: StreamKind) -> (&mut StreamState, Option<Typval>, &'static str) {
    match stream {
        StreamKind::Stdout => (&mut job.stdout, job.callbacks.stdout.clone(), "stdout"),
        StreamKind::Stderr => (&mut job.stderr, job.callbacks.stderr.clone(), "stderr"),
    }
}

fn data_event(id: u64, callback: Typval, receiver: &DictRef, name: &'static str, bytes: Vec<u8>) -> JobEvent {
    let lines = if bytes.is_empty() {
        vec![Typval::String(OxStr(Vec::new()))]
    } else {
        bytes.split(|byte| *byte == b'\n')
            .map(|line| Typval::String(OxStr(line.strip_suffix(b"\r").unwrap_or(line).to_vec())))
            .collect()
    };
    JobEvent {
        callback,
        receiver: receiver.clone(),
        args: vec![
            Typval::Number(id as i64),
            Typval::list(lines),
            Typval::String(OxStr::from(name)),
        ],
    }
}

fn queue_stream_event(queue: &Arc<Mutex<VecDeque<RawEvent>>>, id: u64, stream: StreamKind, event: NetEvent) {
    let event = match event {
        NetEvent::Read(bytes) => Some(RawEvent::Data(id, stream, bytes)),
        NetEvent::Eof => Some(RawEvent::Eof(id, stream)),
        NetEvent::Error(_) => Some(RawEvent::Eof(id, stream)),
        _ => None,
    };
    if let Some(event) = event {
        lock_queue(queue).push_back(event);
    }
}

fn lock_queue(queue: &Arc<Mutex<VecDeque<RawEvent>>>) -> std::sync::MutexGuard<'_, VecDeque<RawEvent>> {
    queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn options(command: &str, buffered: bool) -> JobStartOptions {
        let options = Typval::dict(Vec::new());
        let Typval::Dict(reference) = options else { unreachable!() };
        let callback = Typval::String(OxStr::from("Callback"));
        JobStartOptions {
            program: PathBuf::from("sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            environment: None,
            cwd: None,
            detached: false,
            pty: false,
            rpc: false,
            stdin_pipe: true,
            stdout_buffered: buffered,
            stderr_buffered: buffered,
            callbacks: JobCallbacks {
                options: reference,
                stdout: Some(callback.clone()),
                stderr: Some(callback.clone()),
                exit: Some(callback),
            },
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
        jobs.start(3, options("printf 'alpha\nbeta'; printf 'err' >&2", true)).unwrap();
        let (status, events) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
        assert!(events.iter().any(|event| event_name(event) == "stdout"));
        assert!(events.iter().any(|event| event_name(event) == "stderr"));
        assert!(events.iter().any(|event| event_name(event) == "exit"));
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
        let mut job_options = options("printenv OX_JOB_VALUE | grep -qx set && pwd | grep -qx /tmp", false);
        let mut environment = std::env::vars_os().collect::<Vec<_>>();
        environment.push((OsString::from("OX_JOB_VALUE"), OsString::from("set")));
        job_options.environment = Some(environment);
        job_options.cwd = Some(PathBuf::from("/tmp"));
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, job_options).unwrap();
        let (status, _) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
    }
}
