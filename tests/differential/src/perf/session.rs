//! Owned external editor process with timestamped RPC decode ordering.
//!
//! One [`PerfSession`] owns exactly one `std::process::Child`, its RPC
//! pipes, and a background decoder thread; the thread stamps every decoded
//! message with [`Instant::now`] at receipt, so a calling thread observing
//! a boundary sees the wall-clock moment the bytes crossed the pipe, not
//! the moment the queue was drained. Equal-timestamp messages stay ordered
//! in wire order via their arrival sequence.
//!
//! # Conventions
//!
//! Wire framing and message encoding are entirely [`ox_rpc`]'s
//! ([`IncrementalDecoder`], [`Message`]); UI state is entirely
//! [`ox_tui::TuiState`]. This module adds only process ownership, decode
//! timestamping, the setup fence, and Linux `/proc/<pid>/status` peak-RSS
//! reading. Process isolation reuses the historical plugin-probe's
//! discipline: a fresh `HOME`/`XDG_*`/`TMPDIR` set per process,
//! engine-specific `runtime` resolution, and `--embed --clean -n -i NONE`.
//!
//! # Setup fence
//!
//! After the UI attach, [`PerfSession::finish_setup`] proves the editor is
//! in a known, quiescent state before any timing window opens. It sends a
//! configured-size resize request `R` and then a distinct, non-mutating,
//! non-fast fence request `F = nvim_get_current_buf`, and consumes stdout
//! in wire order until the exact response to `F` decodes. Because `F` is
//! metadata-only (it generates no redraw traffic on either engine) and its
//! response can only exist after `R` was processed, every byte decoded
//! before the `F` response is either setup output of `R` or stale traffic —
//! and stale traffic cannot manufacture a response to the never-reused `F`
//! msgid. Stale frames may be decoded and applied, but they cannot satisfy
//! the fence: only the exact `F` response allocates the boundary, and at
//! that boundary grid 1 must compose at exactly the configured geometry.
//!
//! # Typestates
//!
//! [`PerfSession<Spawned>`] has attached no UI yet; [`PerfSession::attach_ui`]
//! consumes it into [`PerfSession<Attached>`]; [`PerfSession::finish_setup`]
//! consumes that into [`PerfSession<Ready>`]. Only `Ready` exposes the
//! timed request entry point, so a timing sample cannot start before the
//! fence. Any setup error consumes and terminates the session: a session
//! whose external geometry is not proven is not reusable.
//!
//! # Deadline scope
//!
//! The session thread performs no blocking pipe operation: encoded writes
//! move to a dedicated writer thread, and every wait — receive waits and
//! write-completion waits alike — is bounded by an absolute deadline sized
//! from the session budget.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ox_rpc::{IncrementalDecoder, Message, RedrawEvent};
use ox_tui::chrome::TimeMs;
use ox_tui::{MotionPolicy, TuiState};
use ox_types::{Dict, Object, OxStr};

/// One redraw event as decoded from a `"redraw"` notification:
/// `[name, argset, argset, …]` (mirrors [`ox_rpc::RedrawEvent`], owned).
#[derive(Debug, Clone)]
pub struct DecodedRedraw {
    /// Event name (e.g. `"grid_line"`).
    pub name: String,
    /// The argument sets bundled under `name`; each is one method call.
    pub argsets: Vec<Vec<Object>>,
}

/// Decode the params of a `"redraw"` notification into ordered events.
fn decode_redraw_params(params: &[Object]) -> Result<Vec<DecodedRedraw>, PerfError> {
    let mut events = Vec::with_capacity(params.len());
    for entry in params {
        let Object::Array(fields) = entry else {
            return Err(PerfError::Protocol(
                "redraw event entry must be an array".to_owned(),
            ));
        };
        let Some(Object::String(name)) = fields.first() else {
            return Err(PerfError::Protocol(
                "redraw event entry must start with a name string".to_owned(),
            ));
        };
        let mut argsets = Vec::with_capacity(fields.len().saturating_sub(1));
        for argset in &fields[1..] {
            let Object::Array(args) = argset else {
                return Err(PerfError::Protocol(format!(
                    "redraw event {name:?} argument set must be an array"
                )));
            };
            argsets.push(args.clone());
        }
        events.push(DecodedRedraw {
            name: String::from_utf8_lossy(name.as_bytes()).into_owned(),
            argsets,
        });
    }
    Ok(events)
}

/// One message decoded by the reader thread.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// `[1, msgid, ...]` reply to a harness request.
    Response {
        /// The answered request id.
        msgid: u32,
        /// `Ok(result)` or the API error message.
        result: Result<Object, String>,
    },
    /// A complete `"redraw"` batch in decode order.
    Redraw(Vec<DecodedRedraw>),
    /// Any other `[2, method, params]` notification.
    Notification {
        /// Notification method name.
        method: String,
        /// Notification parameters.
        params: Vec<Object>,
    },
    /// The stream carried bytes that do not decode; the reader stopped.
    DecodeError {
        /// The decoder's description of the failure.
        message: String,
    },
}

impl SessionEvent {
    /// Whether this event is a redraw batch whose final event is `flush`,
    /// i.e. a complete frame (upstream `flush_event`, `api/ui.c`).
    #[must_use]
    pub fn is_complete_flush(&self) -> bool {
        match self {
            Self::Redraw(events) => events.last().is_some_and(|event| event.name == "flush"),
            _ => false,
        }
    }
}

/// One event on the session's unified I/O channel.
///
/// Reader events and writer completions are multiplexed so the session
/// thread can wait on one deadline-bounded receiver and never perform a
/// blocking pipe operation of its own.
#[derive(Debug)]
enum SessionIo {
    /// A decoded reader message, stamped at receipt.
    Read(TimedMessage),
    /// The writer thread completed the write enqueued under this id.
    Written(u32),
    /// The writer thread's write for this id failed (broken stdin).
    WriteFailed(u32, String),
}

/// A queued message with its decode timestamp and arrival sequence.
#[derive(Debug, Clone)]
pub struct TimedMessage {
    /// Reader-thread `Instant::now()` when this chunk finished decoding.
    pub at: Instant,
    /// Monotonic arrival sequence within the session; orders equal stamps.
    pub seq: u64,
    /// The decoded message.
    pub event: SessionEvent,
}

/// Which binary a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    /// The pinned oracle `.references/neovim/build/bin/nvim`.
    Neovim,
    /// The rewrite `target/release/oxvim`.
    Oxvim,
}

impl Engine {
    /// Absolute binary path using the differential harness's existing conventions.
    #[must_use]
    pub fn command(self) -> PathBuf {
        match self {
            Self::Neovim => crate::binary(crate::ORACLE),
            Self::Oxvim => crate::binary(crate::OXVIM),
        }
    }

    fn runtime_env(self) -> (&'static str, PathBuf) {
        match self {
            Self::Neovim => (
                "VIMRUNTIME",
                crate::root().join(".references/neovim/runtime"),
            ),
            Self::Oxvim => ("OXVIM_RUNTIME", crate::root().join("runtime")),
        }
    }
}

/// A closed performance workload cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkloadCase {
    /// Lua plugin startup scaling at a fixed plugin count (loads plugins).
    PluginStartup {
        /// Deterministic number of `plugin/*.lua` scripts.
        count: usize,
    },
    /// Embedded `nvim_input` to trailing `flush`.
    RpcInputToFlush,
    /// Large-buffer open to first complete initial flush.
    LargeOpen,
    /// Large-buffer midpoint line replacement to trailing `flush`.
    LargeEdit,
    /// Large-buffer five-line scroll to trailing `flush`.
    LargeScroll,
}

/// Editor geometry accepted by `nvim_ui_attach`/`nvim_ui_try_resize`.
///
/// Upstream clamps screen dimensions to width `12..=10_000` and height
/// `2..=1_000` (`nvim/screen.c`), far below the `usize -> Integer -> C int`
/// conversion boundary where values would wrap. Constructing a [`UiSize`]
/// outside the clamp range is a constructor error, so the post-fence
/// geometry check compares like with like and an accepted attach can only
/// produce the exact configured size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiSize {
    width: u16,
    height: u16,
}

impl UiSize {
    /// Upstream's minimum screen width.
    pub const WIDTH_MIN: u32 = 12;
    /// Upstream's maximum screen width.
    pub const WIDTH_MAX: u32 = 10_000;
    /// Upstream's minimum screen height.
    pub const HEIGHT_MIN: u32 = 2;
    /// Upstream's maximum screen height.
    pub const HEIGHT_MAX: u32 = 1_000;

    /// Validate a geometry against upstream's exact screen clamps.
    ///
    /// # Errors
    ///
    /// [`PerfError::InvalidUiSize`] when `width` or `height` falls outside
    /// the clamp range.
    pub fn new(width: u32, height: u32) -> Result<Self, PerfError> {
        if !(Self::WIDTH_MIN..=Self::WIDTH_MAX).contains(&width)
            || !(Self::HEIGHT_MIN..=Self::HEIGHT_MAX).contains(&height)
        {
            return Err(PerfError::InvalidUiSize { width, height });
        }
        Ok(Self {
            width: u16::try_from(width).map_err(|_| PerfError::InvalidUiSize { width, height })?,
            height: u16::try_from(height)
                .map_err(|_| PerfError::InvalidUiSize { width, height })?,
        })
    }

    /// The validated screen width.
    #[must_use]
    pub const fn width(self) -> u16 {
        self.width
    }

    /// The validated screen height.
    #[must_use]
    pub const fn height(self) -> u16 {
        self.height
    }

    /// The validated `(width, height)` pair.
    #[must_use]
    pub const fn dimensions(self) -> (u16, u16) {
        (self.width, self.height)
    }
}

/// How the editor process is launched.
///
/// Both modes execute directly through [`Command`]; neither performs shell
/// parsing or executable lookup. A prefix receives its arguments, followed by
/// the absolute editor path and the editor's unchanged argument vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessLauncher {
    /// Execute the absolute editor binary directly.
    Direct,
    /// Execute a profiler or other process prefix around the editor.
    Prefix {
        /// Absolute path to the prefix executable.
        executable: PathBuf,
        /// Prefix arguments placed before the editor path.
        args: Vec<OsString>,
    },
}

impl ProcessLauncher {
    fn command(&self, editor: &Path) -> Result<Command, PerfError> {
        if !editor.is_absolute() {
            return Err(PerfError::ExecutableNotAbsolute {
                role: "editor",
                path: editor.to_path_buf(),
            });
        }

        match self {
            Self::Direct => Ok(Command::new(editor)),
            Self::Prefix { executable, args } => {
                if !executable.is_absolute() {
                    return Err(PerfError::ExecutableNotAbsolute {
                        role: "prefix",
                        path: executable.clone(),
                    });
                }
                let mut command = Command::new(executable);
                command.args(args).arg(editor);
                Ok(command)
            }
        }
    }
}

/// Per-process isolation and session tuning.
///
/// Build with [`SessionConfig::isolated`] for the standard fresh-directories
/// layout, or construct fields directly for a custom layout.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Fresh writable `HOME` for this process only.
    pub home: PathBuf,
    /// Fresh writable user config root (`XDG_CONFIG_HOME`).
    pub xdg_config_home: PathBuf,
    /// Fresh writable user data root (`XDG_DATA_HOME`).
    pub xdg_data_home: PathBuf,
    /// Fresh writable user state root (`XDG_STATE_HOME`).
    pub xdg_state_home: PathBuf,
    /// Fresh writable cache root (`XDG_CACHE_HOME`).
    pub xdg_cache_home: PathBuf,
    /// Fresh writable runtime dir (`XDG_RUNTIME_DIR`).
    pub xdg_runtime_dir: PathBuf,
    /// Fresh writable temp dir (`TMPDIR`).
    pub tmp_dir: PathBuf,
    /// Directory passed as the child's cwd (the harness fixture root).
    pub working_dir: PathBuf,
    /// Whole-session wait budget; every wait decomposes into finite steps.
    pub timeout: Duration,
    /// UI attachment size; validated against upstream clamps by
    /// [`UiSize::new`].
    pub ui_size: UiSize,
    /// Direct editor execution or an absolute executable prefix.
    pub launcher: ProcessLauncher,
    /// Extra engine arguments appended after the fixed isolation flags.
    /// Positional file arguments must be last within this vector.
    pub extra_args: Vec<String>,
}

impl SessionConfig {
    /// Standard isolation: every writable root is a distinct subdirectory of
    /// `root`, and the child runs in `working_dir` (usually the fixture root
    /// itself). Defaults: 80x24 geometry, ten-second session budget.
    #[must_use]
    pub fn isolated(root: &Path, working_dir: &Path) -> Self {
        let dir = |name: &str| root.join(name);
        Self {
            home: dir("home"),
            xdg_config_home: dir("config"),
            xdg_data_home: dir("data"),
            xdg_state_home: dir("state"),
            xdg_cache_home: dir("cache"),
            xdg_runtime_dir: dir("runtime"),
            tmp_dir: dir("tmp"),
            working_dir: working_dir.to_path_buf(),
            timeout: Duration::from_secs(10),
            ui_size: UiSize {
                width: 80,
                height: 24,
            },
            launcher: ProcessLauncher::Direct,
            extra_args: Vec::new(),
        }
    }
}

/// Typed finite failure modes; every fallible operation maps to one variant.
#[derive(Debug)]
pub enum PerfError {
    /// An editor or prefix executable path was not absolute.
    ExecutableNotAbsolute {
        /// Which executable path failed validation.
        role: &'static str,
        /// The rejected path.
        path: PathBuf,
    },
    /// The engine binary could not be spawned.
    Spawn { engine: Engine, source: io::Error },
    /// A required pipe was absent after spawn (child killed before return).
    MissingPipe { engine: Engine, pipe: &'static str },
    /// The reader thread could not be started (child killed before return).
    ReaderThread { source: io::Error },
    /// The stdin writer thread could not be started (child killed first).
    WriterThread { source: io::Error },
    /// The stderr capture thread could not be started.
    StderrThread { source: io::Error },
    /// Stderr capture failed after reading the retained partial text.
    StderrRead { source: io::Error, partial: String },
    /// A session helper thread panicked.
    ThreadPanicked { role: &'static str },
    /// A session helper thread did not finish before the cleanup deadline.
    ThreadJoinTimeout { role: &'static str },
    /// The child could not be killed and was still running afterward.
    ChildKill { source: io::Error },
    /// Polling the child exit status failed.
    ChildWait { source: io::Error },
    /// The child was not observed exiting before the cleanup deadline.
    ChildWaitTimeout,
    /// The editor returned an API error for an untimed request.
    Request { method: String, message: String },
    /// Writing a request failed (stdin closed or broken pipe).
    Write { source: io::Error },
    /// The checked request-id allocator reached `u32::MAX` and refuses to
    /// wrap; ids are never reused within a session.
    RequestIdExhausted,
    /// No matching message arrived before the session deadline.
    Timeout { waited: Duration },
    /// The child exited or the reader disconnected before the deadline.
    ChildExited,
    /// The wire carried bytes that do not decode to valid RPC.
    Decode { message: String },
    /// A decoded message violated the RPC shape contract.
    Protocol(String),
    /// The configured geometry is outside upstream's screen clamps.
    InvalidUiSize { width: u32, height: u32 },
    /// After the setup fence, grid 1 did not compose at the configured size.
    GeometryMismatch {
        /// The configured `(width, height)`.
        configured: (u16, u16),
        /// The composed grid dimensions observed at the fence.
        observed: (usize, usize),
    },
    /// `VmHWM` could not be read for the child pid.
    PeakRss { source: io::Error },
    /// `/proc/<pid>/status` exists but has no `VmHWM` line.
    PeakRssMissing,
    /// `VmHWM` exists but its value or `kB` unit is malformed.
    PeakRssMalformed { line: String },
    /// This host cannot measure peak RSS (no Linux `/proc`).
    PeakRssUnsupported,
    /// Shutdown observed a nonzero child exit; stderr is retained.
    ShutdownExit { code: i32, stderr: String },
    /// Shutdown observed signal termination; stderr is retained.
    ShutdownSignal { stderr: String },
    /// The child was still alive one full session budget after stdin closed;
    /// it was killed and its exit status could not be honored.
    ShutdownTimeout { stderr: String },
    /// Shutdown could not poll the child exit status; stderr is retained.
    ShutdownWait { source: io::Error, stderr: String },
    /// A primary lifecycle failure plus stderr and secondary cleanup failures.
    Lifecycle {
        primary: Box<PerfError>,
        stderr: String,
        cleanup: Vec<PerfError>,
    },
}

impl std::fmt::Display for PerfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExecutableNotAbsolute { .. }
            | Self::Spawn { .. }
            | Self::MissingPipe { .. }
            | Self::ReaderThread { .. }
            | Self::WriterThread { .. }
            | Self::StderrThread { .. }
            | Self::StderrRead { .. }
            | Self::ThreadPanicked { .. }
            | Self::ThreadJoinTimeout { .. }
            | Self::ChildKill { .. }
            | Self::ChildWait { .. }
            | Self::ChildWaitTimeout => self.fmt_process(f),
            Self::Request { .. }
            | Self::Write { .. }
            | Self::RequestIdExhausted
            | Self::Timeout { .. }
            | Self::ChildExited
            | Self::Decode { .. }
            | Self::Protocol(_)
            | Self::InvalidUiSize { .. }
            | Self::GeometryMismatch { .. } => self.fmt_rpc(f),
            Self::PeakRss { .. }
            | Self::PeakRssMissing
            | Self::PeakRssMalformed { .. }
            | Self::PeakRssUnsupported => self.fmt_evidence(f),
            Self::ShutdownExit { .. }
            | Self::ShutdownSignal { .. }
            | Self::ShutdownTimeout { .. }
            | Self::ShutdownWait { .. }
            | Self::Lifecycle { .. } => self.fmt_shutdown(f),
        }
    }
}

impl PerfError {
    fn fmt_process(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExecutableNotAbsolute { role, path } => write!(
                f,
                "{role} executable path is not absolute: {}",
                path.display()
            ),
            Self::Spawn { engine, source } => write!(f, "could not spawn {engine:?}: {source}"),
            Self::MissingPipe { engine, pipe } => {
                write!(f, "{engine:?} had no {pipe} after spawn")
            }
            Self::ReaderThread { source } => write!(f, "could not start reader thread: {source}"),
            Self::WriterThread { source } => {
                write!(f, "could not start stdin writer thread: {source}")
            }
            Self::StderrThread { source } => {
                write!(f, "could not start stderr capture thread: {source}")
            }
            Self::StderrRead { source, partial } => write!(
                f,
                "could not finish stderr capture: {source}; partial: {partial}"
            ),
            Self::ThreadPanicked { role } => write!(f, "{role} thread panicked"),
            Self::ThreadJoinTimeout { role } => {
                write!(f, "{role} thread did not finish before cleanup deadline")
            }
            Self::ChildKill { source } => write!(f, "could not kill child: {source}"),
            Self::ChildWait { source } => write!(f, "could not poll child status: {source}"),
            Self::ChildWaitTimeout => write!(f, "child did not exit before cleanup deadline"),
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_rpc(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request { method, message } => write!(f, "{method} request failed: {message}"),
            Self::Write { source } => write!(f, "request write failed: {source}"),
            Self::RequestIdExhausted => write!(
                f,
                "request ids exhausted; ids are never reused within a session"
            ),
            Self::Timeout { waited } => write!(f, "no matching message within {waited:?}"),
            Self::ChildExited => write!(f, "child exited before the wait completed"),
            Self::Decode { message } => write!(f, "decode failure: {message}"),
            Self::Protocol(message) => write!(f, "protocol violation: {message}"),
            Self::InvalidUiSize { width, height } => write!(
                f,
                "invalid session ui_size {width}x{height}: upstream clamps \
                 width to {}..={} and height to {}..={}",
                UiSize::WIDTH_MIN,
                UiSize::WIDTH_MAX,
                UiSize::HEIGHT_MIN,
                UiSize::HEIGHT_MAX
            ),
            Self::GeometryMismatch {
                configured,
                observed,
            } => write!(
                f,
                "setup fence geometry mismatch: configured {}x{}, observed {}x{}",
                configured.0, configured.1, observed.0, observed.1
            ),
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_evidence(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PeakRss { source } => write!(f, "could not read VmHWM: {source}"),
            Self::PeakRssMissing => write!(f, "/proc status had no VmHWM line"),
            Self::PeakRssMalformed { line } => write!(f, "malformed VmHWM line: {line:?}"),
            Self::PeakRssUnsupported => {
                write!(f, "peak RSS requires a Linux /proc/<pid>/status file")
            }
            _ => Err(std::fmt::Error),
        }
    }

    fn fmt_shutdown(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShutdownExit { code, stderr } => {
                write!(f, "child exited {code} at shutdown; stderr: {stderr}")
            }
            Self::ShutdownSignal { stderr } => {
                write!(
                    f,
                    "child terminated by signal at shutdown; stderr: {stderr}"
                )
            }
            Self::ShutdownTimeout { stderr } => write!(
                f,
                "child ignored stdin EOF for a full session budget; \
                           killed; stderr: {stderr}"
            ),
            Self::ShutdownWait { source, stderr } => write!(
                f,
                "shutdown could not read exit status: {source}; stderr: {stderr}"
            ),
            Self::Lifecycle {
                primary,
                stderr,
                cleanup,
            } => write!(
                f,
                "{primary}; stderr: {stderr}; cleanup failures: {cleanup:?}"
            ),
            _ => Err(std::fmt::Error),
        }
    }
}

impl std::error::Error for PerfError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. }
            | Self::ReaderThread { source }
            | Self::WriterThread { source }
            | Self::StderrThread { source }
            | Self::StderrRead { source, .. }
            | Self::ChildKill { source }
            | Self::ChildWait { source }
            | Self::Write { source }
            | Self::PeakRss { source }
            | Self::ShutdownWait { source, .. } => Some(source),
            Self::Lifecycle { primary, .. } => Some(primary.as_ref()),
            _ => None,
        }
    }
}

/// Current-window viewport with the raw window handle normalized away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedViewport {
    /// First buffer line at the window top.
    pub top_line: i64,
    /// Last buffer line at the window bottom.
    pub bottom_line: i64,
    /// Buffer line under the cursor.
    pub cursor_line: i64,
    /// Buffer column under the cursor.
    pub cursor_column: i64,
}

/// A normalized cross-engine UI observation built from applied redraws.
///
/// Identity details (`ext_*` channel ids, grid handles, highlight ids) are
/// absent by construction; text, cursor, mode, and viewport are exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSnapshot {
    /// Composed grid dimensions `(width, height)`.
    pub dimensions: (usize, usize),
    /// Byte-exact rendered grid text (rows joined with `\n`).
    pub rendered_grid: String,
    /// Cursor projected into composed coordinates, if visible.
    pub composed_cursor: Option<(usize, usize)>,
    /// Active mode name, if the server has announced one.
    pub mode_name: Option<String>,
    /// Viewport of the cursor's window, if announced.
    pub viewport: Option<NormalizedViewport>,
}

impl UiSnapshot {
    /// Compose a snapshot from headless state fed by applied redraw batches.
    ///
    /// # Errors
    ///
    /// Propagates composed-grid failure as [`PerfError::Protocol`]; the
    /// terminal grid must exist before a snapshot is meaningful.
    pub fn from_state(state: &TuiState) -> Result<Self, PerfError> {
        let grid = state
            .screen
            .composed_grid()
            .map_err(|error| PerfError::Protocol(format!("snapshot compose: {error}")))?;
        let viewport = state.screen.cursor().and_then(|cursor| {
            state
                .screen
                .viewport(cursor.grid)
                .map(|vp| NormalizedViewport {
                    top_line: vp.top_line,
                    bottom_line: vp.bottom_line,
                    cursor_line: vp.cursor_line,
                    cursor_column: vp.cursor_column,
                })
        });
        Ok(Self {
            dimensions: (grid.width(), grid.height()),
            rendered_grid: grid.render_to_string(),
            composed_cursor: state.screen.composed_cursor(),
            mode_name: state
                .screen
                .active_mode()
                .map(|mode| String::from_utf8_lossy(mode.name.as_bytes()).into_owned()),
            viewport,
        })
    }
}

/// The first complete flush decoded during a wait, if any, with the
/// [`UiSnapshot`] composed at that exact boundary.
pub type FlushBoundary = Option<(TimedMessage, UiSnapshot)>;

/// A timed window split at the exact response.
///
/// `total` spans `started` to the first complete flush decoded after the
/// exact response; `response` spans `started` to that response's decoder
/// stamp, when it was observed. The flush stage is their checked difference.
#[derive(Debug, Clone, Copy)]
pub struct StageTiming {
    /// `started` -> first complete flush after the exact response.
    pub total: Duration,
    /// `started` -> the exact response, when it was observed.
    pub response: Option<Duration>,
}

impl StageTiming {
    /// Response -> flush. `None` when the response stamp was not observed.
    #[must_use]
    pub fn flush(self) -> Option<Duration> {
        self.response.and_then(|r| self.total.checked_sub(r))
    }
}

/// Spawned: the child runs, but no UI is attached yet.
///
/// The only forward transition is [`PerfSession::attach_ui`].
#[derive(Debug, Clone, Copy)]
pub struct Spawned;

/// Attached: the UI is attached at the validated configured size, but the
/// setup fence has not crossed.
///
/// The only forward transition is [`PerfSession::finish_setup`].
#[derive(Debug, Clone, Copy)]
pub struct Attached;

/// Ready: the setup fence crossed and grid 1 composes at the configured
/// size. This is the only state exposing the timed request entry point.
#[derive(Debug, Clone, Copy)]
pub struct Ready;

/// The engine-, child-, and channel-owning state shared by every
/// [`PerfSession`] typestate.
#[derive(Debug)]
struct SessionCore {
    engine: Engine,
    child: Child,
    /// Job channel to the writer thread; `None` once stdin is closed.
    writer: Option<Sender<(u32, Vec<u8>)>>,
    writer_thread: Option<JoinHandle<()>>,
    incoming: Receiver<SessionIo>,
    reader: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<StderrRead>>,
    /// Messages deferred during waits, re-queued after each boundary in wire
    /// order and consumed by [`PerfSession::drain_deferred`].
    stash: VecDeque<TimedMessage>,
    msgid: CheckedMsgidCounter,
    config: SessionConfig,
    state: TuiState,
}

#[derive(Debug)]
struct StderrRead {
    bytes: Vec<u8>,
    error: Option<io::Error>,
}

struct SpawnPipes {
    input: std::process::ChildStdin,
    stdout: ChildStdout,
    stderr: std::process::ChildStderr,
}

struct SessionHelpers {
    writer: Sender<(u32, Vec<u8>)>,
    writer_thread: Option<JoinHandle<()>>,
    incoming: Receiver<SessionIo>,
    reader: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<StderrRead>>,
}

/// An owned, isolated external editor session in state `S`.
///
/// Construct with [`PerfSession::spawn`] (or the spawn-timing constructors
/// [`PerfSession::spawn_to_response`] / [`PerfSession::spawn_to_initial_flush`]);
/// drive through the typestates (`attach_ui`, `finish_setup`) and finish
/// with [`PerfSession::shutdown`]. Dropping forces an ungraceful kill.
#[derive(Debug)]
pub struct PerfSession<S> {
    core: SessionCore,
    _state: PhantomData<S>,
}

/// Monotonic, checked, non-reusing request-id allocator.
///
/// `0` is reserved for editor-initiated requests and is never allocated;
/// allocation stops with [`PerfError::RequestIdExhausted`] at `u32::MAX`
/// instead of wrapping into earlier requests' id space.
#[derive(Debug)]
struct CheckedMsgidCounter {
    next: Option<u32>,
}

impl CheckedMsgidCounter {
    fn new() -> Self {
        Self { next: Some(1) }
    }

    fn next_id(&mut self) -> Result<u32, PerfError> {
        let Some(id) = self.next else {
            return Err(PerfError::RequestIdExhausted);
        };
        self.next = id.checked_add(1);
        Ok(id)
    }
}

impl PerfSession<Spawned> {
    /// Spawn the engine under `--embed --clean -n -i NONE` with fully
    /// isolated per-process environment, start the decoder thread, and move
    /// stdin into a dedicated deadline-capable writer thread.
    ///
    /// Plugin-loading cases ([`WorkloadCase::PluginStartup`]) omit
    /// `--noplugin`; every other case passes it.
    ///
    /// # Errors
    ///
    /// Returns [`PerfError::Spawn`], [`PerfError::MissingPipe`],
    /// [`PerfError::ReaderThread`], [`PerfError::WriterThread`], or
    /// [`PerfError::StderrThread`]; post-spawn failures preserve their primary
    /// error and attach any bounded cleanup failures.
    pub fn spawn(
        engine: Engine,
        case: WorkloadCase,
        config: SessionConfig,
    ) -> Result<Self, PerfError> {
        let mut command = build_command(engine, case, &config)?;
        let mut child = command
            .spawn()
            .map_err(|source| PerfError::Spawn { engine, source })?;
        let pipes = take_spawn_pipes(engine, &mut child)?;
        let helpers = start_session_helpers(child.id(), &mut child, pipes)?;

        Ok(Self {
            core: SessionCore {
                engine,
                child,
                writer: Some(helpers.writer),
                writer_thread: helpers.writer_thread,
                incoming: helpers.incoming,
                reader: helpers.reader,
                stderr_thread: helpers.stderr_thread,
                stash: VecDeque::new(),
                msgid: CheckedMsgidCounter::new(),
                config,
                state: TuiState::new(None, MotionPolicy::Reduced),
            },
            _state: PhantomData,
        })
    }

    /// Send one RPC without opening a timing window or attaching a UI.
    ///
    /// This is available in the spawned typestate so callers can issue
    /// validation requests after the initial API-info response while keeping
    /// editor protocol ordering on the same session.
    ///
    /// # Errors
    ///
    /// Propagates request allocation, write, decode, timeout, child-exit, and
    /// protocol failures. An editor API error becomes [`PerfError::Request`].
    pub fn request(&mut self, method: &str, params: Vec<Object>) -> Result<Object, PerfError> {
        let deadline = Instant::now() + self.core.config.timeout;
        let (msgid, receipt) = self.core.request_raw(method, params, deadline)?;
        self.core
            .wait_response(msgid, receipt, deadline)?
            .result
            .map_err(|message| PerfError::Request {
                method: method.to_owned(),
                message,
            })
    }

    /// Spawn and time from immediately before `Command::spawn` to the
    /// decoder-stamped arrival of the response to one readiness request
    /// (the plugin-startup measurement shape).
    ///
    /// Returns the session (still alive, still [`Spawned`]), the elapsed
    /// duration, and the response result for correctness validation.
    ///
    /// # Errors
    ///
    /// Spawn errors, [`PerfError::RequestIdExhausted`],
    /// [`PerfError::Write`], or [`PerfError::Timeout`] /
    /// [`PerfError::ChildExited`] when the readiness response never arrives.
    pub fn spawn_to_response(
        engine: Engine,
        case: WorkloadCase,
        config: SessionConfig,
        method: &str,
        params: Vec<Object>,
    ) -> Result<(Self, Duration, Result<Object, String>), PerfError> {
        let spawn_started = Instant::now();
        let mut session = Self::spawn(engine, case, config)?;
        let deadline = spawn_started + session.core.config.timeout;
        let (msgid, receipt) = session.core.request_raw(method, params, deadline)?;
        let response = session.core.wait_response(msgid, receipt, deadline)?;
        let elapsed = response.at.duration_since(spawn_started);
        Ok((session, elapsed, response.result))
    }

    /// Spawn, attach the embedded UI at the configured size, cross the
    /// setup fence, and time from immediately before `Command::spawn` to
    /// the decoder-stamped arrival of the first complete initial flush
    /// (the large-open measurement shape).
    ///
    /// The attach response's first complete flush defines the timing
    /// boundary; that batch and everything decoded before it is applied to
    /// the headless state, and the returned snapshot is composed at that
    /// boundary. `finish_setup` runs after the captured stamp, so the
    /// fence cannot distort the initial-flush measurement.
    ///
    /// # Errors
    ///
    /// Spawn errors, attach failure ([`PerfError::Protocol`]), fence
    /// failure ([`PerfError::Protocol`], [`PerfError::GeometryMismatch`]),
    /// or [`PerfError::Timeout`] / [`PerfError::ChildExited`] when no
    /// complete flush arrives. Any failure terminates the session.
    pub fn spawn_to_initial_flush(
        engine: Engine,
        case: WorkloadCase,
        config: SessionConfig,
    ) -> Result<(PerfSession<Ready>, Duration, UiSnapshot), PerfError> {
        let spawn_started = Instant::now();
        let session = Self::spawn(engine, case, config)?;
        let deadline = spawn_started + session.core.config.timeout;
        let (mut session, boundary) = session.attach_ui()?;
        if let Some((flush, snapshot)) = boundary {
            let elapsed = flush.at.duration_since(spawn_started);
            // Composed at the flush boundary in `wait_response`; the stamp
            // and the state describe the same frame by construction.
            let ready = session.finish_setup()?;
            return Ok((ready, elapsed, snapshot));
        }
        // Quiescent editor: no complete frame arrived while attaching, so
        // wait for the first one without re-reading already-applied traffic.
        let (stamped, snapshot) = session.core.wait_initial_flush(deadline)?;
        let elapsed = stamped.duration_since(spawn_started);
        let ready = session.finish_setup()?;
        Ok((ready, elapsed, snapshot))
    }

    /// Attach the embedded UI with the differential harness's established
    /// capability set (`replay/sessions/ui_attach.yaml`), applying initial
    /// redraws to the headless state while waiting for the attach response.
    ///
    /// Returns the first complete flush decoded while waiting, if any, with
    /// its redraw batch applied and the [`UiSnapshot`] composed at that exact
    /// boundary; [`PerfSession::spawn_to_initial_flush`] uses the pair as the
    /// initial-frame boundary instead of waiting for a later flush that a
    /// quiescent editor never sends.
    ///
    /// Consumes the [`Spawned`] session: on success the session is
    /// [`Attached`]; on any failure the session is terminated and `self` is
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`PerfError::Protocol`] when the attach response is an error;
    /// otherwise the usual finite set while waiting.
    pub fn attach_ui(mut self) -> Result<(PerfSession<Attached>, FlushBoundary), PerfError> {
        let boundary = self.core.attach();
        match boundary {
            Ok(boundary) => Ok((
                PerfSession {
                    core: self.core,
                    _state: PhantomData,
                },
                boundary,
            )),
            Err(error) => Err(self.core.terminate(error)),
        }
    }
}

impl PerfSession<Attached> {
    /// Cross the setup fence and reach [`Ready`].
    ///
    /// Sends `R = nvim_ui_try_resize(configured)` and then the distinct,
    /// never-reused fence `F = nvim_get_current_buf` — a metadata-only
    /// request that generates no redraw traffic on either engine — and
    /// consumes stdout in wire order until the exact response to `F`
    /// decodes. Along the way every redraw batch is applied to the headless
    /// state exactly once, every unrelated response/notification is deferred
    /// in its original order, and the exact response to `R` must have been
    /// successful. At the `F` response the applied grid-1 geometry must
    /// compose at exactly the configured size.
    ///
    /// Stale frames cannot satisfy the fence: `F`'s msgid is unique and
    /// never reused, so no earlier traffic can carry its response, and both
    /// requests are enqueued to the writer thread in FIFO order under one
    /// absolute deadline.
    ///
    /// Consumes the session: on success the session is [`Ready`]; on any
    /// failure the session is terminated and dropped — a session whose
    /// external geometry is not proven is never reusable.
    ///
    /// # Errors
    ///
    /// [`PerfError::Protocol`] for `R`/`F` API errors or an out-of-order
    /// fence response, [`PerfError::GeometryMismatch`] when the applied
    /// grid does not compose at the configured size, and the usual finite
    /// set ([`PerfError::Write`], [`PerfError::Timeout`],
    /// [`PerfError::ChildExited`], [`PerfError::Decode`],
    /// [`PerfError::RequestIdExhausted`]) otherwise.
    pub fn finish_setup(self) -> Result<PerfSession<Ready>, PerfError> {
        let mut this = self;
        let deadline = Instant::now() + this.core.config.timeout;
        let outcome = (|| {
            let configured = this.core.config.ui_size;
            let resize_id = this.core.alloc_msgid()?;
            let resize_request = Message::Request {
                msgid: resize_id,
                method: "nvim_ui_try_resize".into(),
                params: vec![
                    Object::Integer(i64::from(configured.width())),
                    Object::Integer(i64::from(configured.height())),
                ],
            };
            let resize_receipt = this.core.send(resize_id, &resize_request, deadline)?;
            let fence_id = this.core.alloc_msgid()?;
            let fence_request = Message::Request {
                msgid: fence_id,
                method: "nvim_get_current_buf".into(),
                params: vec![],
            };
            let fence_receipt = this.core.send(fence_id, &fence_request, deadline)?;

            // Both jobs are enqueued; consume reader output in wire order
            // until the exact F response, sharing one absolute deadline.
            // R's reads predate F's, so the receipts concatenate into one
            // prelude drained ahead of the live channel: R's response or
            // redraws may sit in either receipt (the ack/read race), and
            // stale pre-request traffic can never appear in either.
            let mut reads = resize_receipt;
            reads.extend(fence_receipt);
            let timeout = this.core.config.timeout;
            let incoming = &this.core.incoming;
            let mut channel_next = move || next_read(incoming, timeout, deadline);
            let mut next = || next_with_receipt(&mut reads, &mut channel_next);
            let mut deferred = VecDeque::new();
            let fenced = setup_fence_core(
                &mut this.core.state,
                resize_id,
                fence_id,
                configured.dimensions(),
                &mut deferred,
                &mut next,
            );
            // Messages consumed before the boundary precede any unread
            // receipt tail. Restore both sides in that wire order.
            deferred.extend(reads);
            this.core.restore(deferred);
            fenced
        })();
        match outcome {
            Ok(()) => Ok(PerfSession {
                core: this.core,
                _state: PhantomData,
            }),
            // Setup failure consumes the session: kill the child so a
            // blocked writer is released, join every helper thread with a
            // bounded budget, and never return a reusable session.
            Err(error) => Err(this.core.terminate(error)),
        }
    }
}

impl PerfSession<Ready> {
    /// Time one `nvim_input`-shaped request from immediately before
    /// encoding through the decoder timestamp of the first complete flush
    /// decoded *after* the exact response, in stdout/`seq` order.
    ///
    /// Causal contract: the API response is not the endpoint — Neovim
    /// acknowledges `nvim_input` before processing it (`api/vim.c`) — but
    /// it is the marker proving the request's processing and redraw follow
    /// on the wire. A complete flush decoded before that response is stale
    /// traffic: it is applied to the headless state but never ends the
    /// window. Reachable only on [`PerfSession<Ready>`]: the setup fence
    /// crossed before this method can exist to call, so no timing sample
    /// can start before the fence boundary.
    ///
    /// The window drains this request's per-send receipt (reads observed
    /// before the write completed) ahead of the live channel; the general
    /// stash is never consulted. Returns the elapsed duration and the
    /// response when it arrived.
    ///
    /// # Errors
    ///
    /// [`PerfError::Timeout`] when no complete flush arrives within the
    /// session budget; otherwise the usual finite set.
    pub fn request_to_flush(
        &mut self,
        method: &str,
        params: Vec<Object>,
    ) -> Result<(Duration, Option<Result<Object, String>>), PerfError> {
        let (timing, response) = self.request_to_flush_staged(method, params)?;
        Ok((timing.total, response))
    }

    /// Time one `nvim_input`-shaped request from immediately before
    /// encoding through the decoder timestamp of the first complete flush
    /// decoded *after* the exact response, in stdout/`seq` order, split at
    /// the exact response stamp.
    ///
    /// Causal contract: the API response is not the endpoint — Neovim
    /// acknowledges `nvim_input` before processing it (`api/vim.c`) — but
    /// it is the marker proving the request's processing and redraw follow
    /// on the wire. A complete flush decoded before that response is stale
    /// traffic: it is applied to the headless state but never ends the
    /// window. Reachable only on [`PerfSession<Ready>`]: the setup fence
    /// crossed before this method can exist to call, so no timing sample
    /// can start before the fence boundary.
    ///
    /// The window drains this request's per-send receipt (reads observed
    /// before the write completed) ahead of the live channel; the general
    /// stash is never consulted. Returns the stage timing and the response
    /// when it arrived.
    ///
    /// # Errors
    ///
    /// [`PerfError::Timeout`] when no complete flush arrives within the
    /// session budget; otherwise the usual finite set.
    pub fn request_to_flush_staged(
        &mut self,
        method: &str,
        params: Vec<Object>,
    ) -> Result<(StageTiming, Option<Result<Object, String>>), PerfError> {
        let started = Instant::now();
        let deadline = started + self.core.config.timeout;
        let (msgid, mut receipt) = self.core.request_raw(method, params, deadline)?;
        let timeout = self.core.config.timeout;
        let incoming = &self.core.incoming;
        let mut channel_next = move || next_read(incoming, timeout, deadline);
        let mut deferred = VecDeque::new();
        let outcome = request_to_flush_core(
            &mut self.core.state,
            started,
            msgid,
            &mut receipt,
            &mut deferred,
            &mut channel_next,
        );
        // Messages consumed before the boundary precede any unread receipt
        // tail. Restore both sides in that wire order.
        deferred.extend(receipt);
        self.core.restore(deferred);
        outcome
    }

    /// Time one request from immediately before encoding to the
    /// decoder-stamped arrival of its exact response. For calls that
    /// produce no redraw (e.g. `nvim_exec_lua`), where a flush endpoint
    /// never arrives and [`Self::request_to_flush`] would time out.
    ///
    /// Reuses `request_raw` plus `wait_response`, the same pair
    /// [`PerfSession::spawn_to_response`] composes, and restores deferred
    /// traffic the same way [`Self::request_to_flush`] does.
    ///
    /// # Errors
    ///
    /// [`PerfError::Timeout`] when the response does not arrive within the
    /// session budget; otherwise the usual finite set.
    pub fn request_to_response_staged(
        &mut self,
        method: &str,
        params: Vec<Object>,
    ) -> Result<(Duration, Result<Object, String>), PerfError> {
        let started = Instant::now();
        let deadline = started + self.core.config.timeout;
        let (msgid, receipt) = self.core.request_raw(method, params, deadline)?;
        let response = self.core.wait_response(msgid, receipt, deadline)?;
        let elapsed = response.at.duration_since(started);
        Ok((elapsed, response.result))
    }

    /// Take every message set aside during earlier waits, in arrival order.
    ///
    /// Waits defer non-matching traffic and re-queue it here once their
    /// boundary resolves, so no decoded message is dropped; call this
    /// between measurement windows to consume the backlog.
    #[must_use]
    pub fn drain_deferred(&mut self) -> Vec<TimedMessage> {
        self.core.stash.drain(..).collect()
    }
}

impl<S> PerfSession<S> {
    /// The actual launched child pid, valid while the session is alive.
    ///
    /// For [`ProcessLauncher::Prefix`], this is the prefix process; for
    /// [`ProcessLauncher::Direct`], it is the editor process.
    #[must_use]
    pub fn child_pid(&self) -> u32 {
        self.core.child.id()
    }

    /// The engine this session runs.
    #[must_use]
    pub const fn engine(&self) -> Engine {
        self.core.engine
    }

    /// Snapshot the composed UI state.
    ///
    /// # Errors
    ///
    /// Propagates [`UiSnapshot::from_state`] composition failure.
    pub fn snapshot(&self) -> Result<UiSnapshot, PerfError> {
        UiSnapshot::from_state(&self.core.state)
    }

    /// Read the child's high-water-mark RSS in KiB from
    /// `/proc/<pid>/status:VmHWM`.
    ///
    /// Call this immediately after a timed endpoint and before validation
    /// traffic: validation RPCs allocate inside the child and would inflate
    /// the peak.
    ///
    /// # Errors
    ///
    /// [`PerfError::PeakRss`] for I/O failure, [`PerfError::PeakRssMissing`]
    /// when the field is absent, [`PerfError::PeakRssMalformed`] for a
    /// non-numeric value or a non-`kB` unit, and
    /// [`PerfError::PeakRssUnsupported`] on hosts without Linux `/proc`.
    pub fn peak_rss_kib(&self) -> Result<u64, PerfError> {
        let path = Path::new("/proc")
            .join(self.core.child.id().to_string())
            .join("status");
        let file = std::fs::File::open(&path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound && !Path::new("/proc").is_dir() {
                PerfError::PeakRssUnsupported
            } else {
                PerfError::PeakRss { source }
            }
        })?;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|source| PerfError::PeakRss { source })?;
            if let Some(value) = parse_vmhwm_line(&line)? {
                return Ok(value);
            }
        }
        Err(PerfError::PeakRssMissing)
    }

    /// Graceful shutdown from any state: close the writer job channel (the
    /// writer drains its FIFO, then stdin drops and the editor sees EOF),
    /// wait for exit within the session budget, join the helper threads,
    /// and require a zero exit code.
    ///
    /// Consumes the session.
    ///
    /// # Errors
    ///
    /// Returns [`PerfError::ShutdownExit`] with retained stderr on nonzero
    /// exit, [`PerfError::ShutdownSignal`] on signal termination,
    /// [`PerfError::ShutdownTimeout`] when the child must be killed, and
    /// [`PerfError::ShutdownWait`] when the status is unreadable. Helper and
    /// child-control failures never mask these primaries; they attach to
    /// them via [`PerfError::Lifecycle`].
    pub fn shutdown(self) -> Result<(), PerfError> {
        let mut core = self.core;
        // WHY: closing stdin alone makes the oracle exit 1; an explicit
        // `qa!` makes it exit 0 — but only while stdin stays open. If EOF
        // lands before the editor dispatches the quit (reliably so under
        // valgrind's slowdown), the exit is 1 again. So: quit first with
        // stdin held open; fall back to stdin EOF only when the quit was
        // never honored; kill only when both fail.
        enqueue_quit(&mut core.msgid, core.writer.as_ref());
        let budget = core.config.timeout;
        let outcome = Self::await_exit(&mut core, budget);
        let outcome = match outcome {
            Err(PerfError::ChildWaitTimeout) => {
                // Fallback: close the job FIFO (the writer drains it, then
                // drops stdin) and wait once more against a fresh budget.
                core.writer.take();
                Self::await_exit(&mut core, budget)
            }
            outcome => outcome,
        };
        // The writer thread ends only when every job sender drops: release
        // ours before joining helpers, whatever the exit shape.
        core.writer.take();
        match outcome {
            Ok(status) => {
                // Attempt every helper regardless of the exit shape: a zero
                // exit is success only if they all complete normally.
                let deadline = Instant::now() + core.config.timeout;
                let (stderr, failures) = core.join_helpers(deadline);
                match finish(status, stderr.clone()) {
                    Ok(()) => match failures.into_iter().next() {
                        Some(failure) => Err(failure),
                        None => Ok(()),
                    },
                    Err(primary) => Err(lifecycle(primary, stderr, failures)),
                }
            }
            Err(PerfError::ChildWaitTimeout) => {
                // Grace expired: kill, reap, and finalize every helper
                // against a fresh bounded cleanup deadline.
                let deadline = Instant::now() + CLEANUP_BUDGET;
                let mut failures = Vec::new();
                if let Err(error) = kill_child_until(&mut core.child, deadline) {
                    failures.push(error);
                }
                let (stderr, helper_failures) = core.join_helpers(deadline);
                failures.extend(helper_failures);
                Err(lifecycle(
                    PerfError::ShutdownTimeout {
                        stderr: stderr.clone(),
                    },
                    stderr,
                    failures,
                ))
            }
            Err(PerfError::ChildWait { source }) => {
                // Status polling failed; the remaining cleanup is still
                // attempted so its evidence is not lost.
                let deadline = Instant::now() + core.config.timeout;
                let (stderr, failures) = core.join_helpers(deadline);
                Err(lifecycle(
                    PerfError::ShutdownWait {
                        source,
                        stderr: stderr.clone(),
                    },
                    stderr,
                    failures,
                ))
            }
            Err(other) => Err(other),
        }
    }

    /// Wait, bounded by one session budget, for the child's natural exit.
    fn await_exit(core: &mut SessionCore, budget: Duration) -> Result<ExitStatus, PerfError> {
        wait_child_until(&mut core.child, Instant::now() + budget)
    }
}

fn build_command(
    engine: Engine,
    case: WorkloadCase,
    config: &SessionConfig,
) -> Result<Command, PerfError> {
    let editor = engine.command();
    let mut command = config.launcher.command(&editor)?;
    command
        .arg("--embed")
        .arg("--clean")
        .arg("-n")
        .arg("-i")
        .arg("NONE");
    if !matches!(case, WorkloadCase::PluginStartup { .. }) {
        command.arg("--noplugin");
    }
    command.args(&config.extra_args);
    let (env_key, runtime_dir) = engine.runtime_env();
    command
        .env_clear()
        .env("HOME", &config.home)
        .env("XDG_CONFIG_HOME", &config.xdg_config_home)
        .env("XDG_DATA_HOME", &config.xdg_data_home)
        .env("XDG_STATE_HOME", &config.xdg_state_home)
        .env("XDG_CACHE_HOME", &config.xdg_cache_home)
        .env("XDG_RUNTIME_DIR", &config.xdg_runtime_dir)
        .env("TMPDIR", &config.tmp_dir)
        .env(env_key, runtime_dir)
        .current_dir(&config.working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn take_spawn_pipes(engine: Engine, child: &mut Child) -> Result<SpawnPipes, PerfError> {
    let input = child.stdin.take().ok_or_else(|| {
        cleanup_failed_spawn(
            PerfError::MissingPipe {
                engine,
                pipe: "stdin",
            },
            child,
            None,
            None,
            None,
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        cleanup_failed_spawn(
            PerfError::MissingPipe {
                engine,
                pipe: "stdout",
            },
            child,
            None,
            None,
            None,
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        cleanup_failed_spawn(
            PerfError::MissingPipe {
                engine,
                pipe: "stderr",
            },
            child,
            None,
            None,
            None,
        )
    })?;
    Ok(SpawnPipes {
        input,
        stdout,
        stderr,
    })
}

fn start_session_helpers(
    pid: u32,
    child: &mut Child,
    pipes: SpawnPipes,
) -> Result<SessionHelpers, PerfError> {
    let stderr_thread = match thread::Builder::new()
        .name(format!("perf-stderr-{pid}"))
        .spawn(move || {
            let mut bytes = Vec::new();
            let error = BufReader::new(pipes.stderr).read_to_end(&mut bytes).err();
            StderrRead { bytes, error }
        }) {
        Ok(handle) => Some(handle),
        Err(source) => {
            return Err(cleanup_failed_spawn(
                PerfError::StderrThread { source },
                child,
                None,
                None,
                None,
            ));
        }
    };

    let (sender, incoming) = mpsc::channel();
    let reader = match thread::Builder::new()
        .name(format!("perf-rpc-reader-{pid}"))
        .spawn({
            let sender = sender.clone();
            move || reader_loop(pipes.stdout, &sender)
        }) {
        Ok(handle) => Some(handle),
        Err(source) => {
            return Err(cleanup_failed_spawn(
                PerfError::ReaderThread { source },
                child,
                None,
                None,
                stderr_thread,
            ));
        }
    };

    // The writer owns the child's stdin: encoded jobs are enqueued by the
    // session thread and the only blocking pipe writes happen here, off the
    // session's deadline path.
    let (writer, jobs) = mpsc::channel();
    let writer_thread = match thread::Builder::new()
        .name(format!("perf-stdin-writer-{pid}"))
        .spawn(move || writer_loop(pipes.input, jobs, &sender))
    {
        Ok(handle) => Some(handle),
        Err(source) => {
            return Err(cleanup_failed_spawn(
                PerfError::WriterThread { source },
                child,
                reader,
                None,
                stderr_thread,
            ));
        }
    };

    Ok(SessionHelpers {
        writer,
        writer_thread,
        incoming,
        reader,
        stderr_thread,
    })
}

/// The writer thread is closed: no further write can be delivered.
fn writer_closed_error() -> PerfError {
    PerfError::Write {
        source: io::Error::new(io::ErrorKind::BrokenPipe, "stdin writer closed"),
    }
}

/// Enqueue `nvim_command ["qa!"]` without waiting for its write completion
/// or response.
///
/// WHY fire-and-forget: the oracle exits 1 when its stdin merely hits EOF,
/// but exits 0 when it is asked to quit first — and it may exit before
/// answering the quit request, so neither the write report nor the response
/// is awaited. Best-effort by contract: a closed writer, a dropped receiver,
/// or an exhausted id space skips the quit, and the shutdown exit-code gate
/// remains the authoritative check. Returns whether the request was queued.
fn enqueue_quit(msgid: &mut CheckedMsgidCounter, writer: Option<&Sender<(u32, Vec<u8>)>>) -> bool {
    // Id allocation failure (u32::MAX exhausted) is not a shutdown error:
    // skip the quit; `finish` still demands exit 0.
    let Ok(id) = msgid.next_id() else {
        return false;
    };
    let Some(jobs) = writer else {
        return false;
    };
    let request = Message::Request {
        msgid: id,
        method: "nvim_command".into(),
        params: vec![Object::String(OxStr::from("qa!"))],
    };
    jobs.send((id, request.encode_bytes())).is_ok()
}

/// The outcome of waiting for a specific response: the receipt stamp, the
/// response result, and the first complete flush boundary decoded meanwhile.
struct WaitResponse {
    at: Instant,
    result: Result<Object, String>,
    boundary: FlushBoundary,
}

impl SessionCore {
    /// Allocate a request id that is never reused within the session.
    fn alloc_msgid(&mut self) -> Result<u32, PerfError> {
        self.msgid.next_id()
    }

    /// Encode and enqueue one message to the writer thread, then wait,
    /// bounded by `deadline`, for that exact write to complete.
    ///
    /// Returns the ordered per-send receipt: every reader event decoded
    /// between the enqueue and the completion of this write, with its
    /// original timestamps and arrival sequence. The response to this very
    /// request can beat the writer's completion report to the channel, so
    /// the caller must drain the receipt before `incoming` — stale
    /// pre-request traffic can never be smuggled through, because only
    /// reads observed after *this* send's enqueue are on the receipt.
    ///
    /// The session thread performs no blocking pipe write: a full stdin
    /// buffer blocks the writer thread, and this wait expires with the
    /// deadline. On success the receipt is handed to the caller; on error
    /// the observed reads are restored to the general stash (they are older
    /// than anything still queued) and the error propagates.
    ///
    /// # Errors
    ///
    /// [`PerfError::Write`] when the writer is closed or the write failed,
    /// [`PerfError::Timeout`] when the write does not complete in time, and
    /// the usual receive-path finite set otherwise.
    fn send(
        &mut self,
        id: u32,
        message: &Message,
        deadline: Instant,
    ) -> Result<VecDeque<TimedMessage>, PerfError> {
        let bytes = message.encode_bytes();
        let jobs = self.writer.as_ref().ok_or_else(writer_closed_error)?;
        jobs.send((id, bytes)).map_err(|_| writer_closed_error())?;

        let timeout = self.config.timeout;
        let incoming = &self.incoming;
        let mut deferred = VecDeque::new();
        let mut next = move || next_io_message(incoming, timeout, deadline);
        let outcome = await_write(id, &mut deferred, &mut next);
        match outcome {
            Ok(()) => Ok(deferred),
            Err(error) => {
                self.restore(deferred);
                Err(error)
            }
        }
    }

    /// Encode, send, and return the allocated request id together with the
    /// per-send receipt (see [`SessionCore::send`]).
    fn request_raw(
        &mut self,
        method: &str,
        params: Vec<Object>,
        deadline: Instant,
    ) -> Result<(u32, VecDeque<TimedMessage>), PerfError> {
        let msgid = self.alloc_msgid()?;
        let request = Message::Request {
            msgid,
            method: method.into(),
            params,
        };
        let receipt = self.send(msgid, &request, deadline)?;
        Ok((msgid, receipt))
    }

    /// Attach the embedded UI and capture the initial-flush boundary, if
    /// any, from the attach window.
    fn attach(&mut self) -> Result<FlushBoundary, PerfError> {
        let deadline = Instant::now() + self.config.timeout;
        let (width, height) = self.config.ui_size.dimensions();
        let options = Dict(vec![
            ("ext_linegrid".into(), Object::Boolean(true)),
            ("ext_multigrid".into(), Object::Boolean(true)),
            ("ext_cmdline".into(), Object::Boolean(true)),
            ("ext_messages".into(), Object::Boolean(true)),
            ("ext_popupmenu".into(), Object::Boolean(true)),
            ("ext_hlstate".into(), Object::Boolean(true)),
            ("rgb".into(), Object::Boolean(true)),
            ("ext_termcolors".into(), Object::Boolean(true)),
        ]);
        let (msgid, receipt) = self.request_raw(
            "nvim_ui_attach",
            vec![
                Object::Integer(i64::from(width)),
                Object::Integer(i64::from(height)),
                Object::Dict(options),
            ],
            deadline,
        )?;
        let response = self.wait_response(msgid, receipt, deadline)?;
        response
            .result
            .map(|_| response.boundary)
            .map_err(|error| PerfError::Protocol(format!("ui_attach: {error}")))
    }

    /// Wait for the first complete flush when attach saw none (a quiescent
    /// editor), returning its stamp and a snapshot composed at that frame.
    fn wait_initial_flush(
        &mut self,
        deadline: Instant,
    ) -> Result<(Instant, UiSnapshot), PerfError> {
        let mut deferred = VecDeque::new();
        let outcome = (|| {
            loop {
                let message = self.next_live(deadline)?;
                let complete = message.event.is_complete_flush();
                match message.event {
                    SessionEvent::Notification { method, params } => {
                        deferred.push_back(TimedMessage {
                            at: message.at,
                            seq: message.seq,
                            event: SessionEvent::Notification { method, params },
                        });
                    }
                    event => self.handle(event)?,
                }
                if complete {
                    let snapshot = UiSnapshot::from_state(&self.state)?;
                    return Ok((message.at, snapshot));
                }
            }
        })();
        self.restore(deferred);
        outcome
    }

    /// Wait for the response with exactly `msgid`.
    ///
    /// `receipt` holds the reads observed before this request's write
    /// completed ([`SessionCore::send`]); they are drained first, in wire
    /// order, before the live channel — the response can beat the write
    /// completion to the channel, and skipping the receipt would lose it.
    ///
    /// Redraws decoded meanwhile are applied; the first complete one is also
    /// reported with a snapshot composed at that exact boundary, so attach
    /// keeps its initial-frame state. Every other message is deferred
    /// locally. On resolution, messages consumed before the boundary are
    /// restored ahead of the unread receipt tail.
    fn wait_response(
        &mut self,
        msgid: u32,
        mut receipt: VecDeque<TimedMessage>,
        deadline: Instant,
    ) -> Result<WaitResponse, PerfError> {
        let mut deferred = VecDeque::new();
        let mut flush = None;
        let timeout = self.config.timeout;
        let incoming = &self.incoming;
        let mut channel_next = move || next_read(incoming, timeout, deadline);
        let outcome = (|| {
            loop {
                let message = next_with_receipt(&mut receipt, &mut channel_next)?;
                match message.event {
                    SessionEvent::Response {
                        msgid: found,
                        result,
                    } if found == msgid => {
                        return Ok(WaitResponse {
                            at: message.at,
                            result,
                            boundary: flush.take(),
                        });
                    }
                    SessionEvent::Redraw(events) => {
                        let complete = events.last().is_some_and(|event| event.name == "flush");
                        if complete && flush.is_none() {
                            // Apply first, then compose the snapshot, so the
                            // reported stamp and state describe one frame.
                            let snapshot = boundary_snapshot(&mut self.state, events.clone())?;
                            flush = Some((
                                TimedMessage {
                                    at: message.at,
                                    seq: message.seq,
                                    event: SessionEvent::Redraw(events),
                                },
                                snapshot,
                            ));
                        } else {
                            apply_batch(&mut self.state, events)?;
                        }
                    }
                    event => deferred.push_back(TimedMessage {
                        at: message.at,
                        seq: message.seq,
                        event,
                    }),
                }
            }
        })();
        // Messages consumed before the boundary precede any unread receipt
        // tail. Restore both sides in that wire order.
        deferred.extend(receipt);
        self.restore(deferred);
        outcome
    }

    /// Apply one session event to the headless state; stashes nothing.
    fn handle(&mut self, event: SessionEvent) -> Result<(), PerfError> {
        match event {
            SessionEvent::Redraw(events) => self.apply_redraw_batch(events),
            SessionEvent::DecodeError { message } => Err(PerfError::Decode { message }),
            SessionEvent::Response { .. } | SessionEvent::Notification { .. } => Ok(()),
        }
    }

    fn apply_redraw_batch(&mut self, events: Vec<DecodedRedraw>) -> Result<(), PerfError> {
        apply_batch(&mut self.state, events)
    }

    /// Next decoded reader event from the unified channel, bounded by
    /// `deadline`.
    fn next_live(&mut self, deadline: Instant) -> Result<TimedMessage, PerfError> {
        next_read(&self.incoming, self.config.timeout, deadline)
    }

    /// Re-queue messages deferred by a wait after its boundary resolved,
    /// preserving wire order: they are older than anything still queued.
    fn restore(&mut self, deferred: VecDeque<TimedMessage>) {
        self.stash.extend(deferred);
    }

    /// Attempt every helper finalization against `deadline`, returning the
    /// captured stderr and every helper failure in order. Each step runs
    /// even after an earlier one failed.
    fn join_helpers(&mut self, deadline: Instant) -> (String, Vec<PerfError>) {
        let mut failures = Vec::new();
        let stderr = match self.take_stderr(deadline) {
            Ok(stderr) => stderr,
            Err(error) => {
                failures.push(error);
                String::new()
            }
        };
        if let Err(error) = self.join_reader(deadline) {
            failures.push(error);
        }
        if let Err(error) = self.join_writer(deadline) {
            failures.push(error);
        }
        (stderr, failures)
    }

    /// Take the stderr capture result: the retained text on success, or a
    /// [`PerfError::StderrRead`] carrying the partial bytes on read failure.
    /// A lost or crashed capture thread is reported, never defaulted.
    fn take_stderr(&mut self, deadline: Instant) -> Result<String, PerfError> {
        let Some(handle) = self.stderr_thread.take() else {
            return Ok(String::new());
        };
        match wait_join(handle, "stderr", deadline) {
            Ok(Some(capture)) => match capture.error {
                None => Ok(String::from_utf8_lossy(&capture.bytes).into_owned()),
                Some(source) => Err(PerfError::StderrRead {
                    source,
                    partial: String::from_utf8_lossy(&capture.bytes).into_owned(),
                }),
            },
            Ok(None) => Err(PerfError::ThreadJoinTimeout { role: "stderr" }),
            Err(error) => Err(error),
        }
    }

    fn join_reader(&mut self, deadline: Instant) -> Result<(), PerfError> {
        let Some(reader) = self.reader.take() else {
            return Ok(());
        };
        match wait_join(reader, "reader", deadline) {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn join_writer(&mut self, deadline: Instant) -> Result<(), PerfError> {
        let Some(writer) = self.writer_thread.take() else {
            return Ok(());
        };
        match wait_join(writer, "writer", deadline) {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Consume-on-error teardown: kill and reap the child (closing the pipe
    /// and releasing a blocked writer), close the job FIFO, and join every
    /// helper thread with a bounded budget. Never called twice. Returns
    /// `primary` unchanged when cleanup and evidence are complete;
    /// otherwise wraps it with the collected secondary failures.
    fn terminate(&mut self, primary: PerfError) -> PerfError {
        let deadline = Instant::now() + CLEANUP_BUDGET;
        let mut cleanup = Vec::new();
        if let Err(error) = kill_child_until(&mut self.child, deadline) {
            cleanup.push(error);
        }
        self.writer.take();
        let (stderr, failures) = self.join_helpers(deadline);
        cleanup.extend(failures);
        lifecycle(primary, stderr, cleanup)
    }
}

impl Drop for SessionCore {
    fn drop(&mut self) {
        self.writer.take();
        let deadline = Instant::now() + CLEANUP_BUDGET;
        // Best-effort: the typed results are intentionally discarded at the
        // destructor boundary; every step is deadline-bounded and can
        // neither panic nor block past `deadline`.
        let _ = kill_child_until(&mut self.child, deadline);
        let _ = self.take_stderr(deadline);
        let _ = self.join_reader(deadline);
        let _ = self.join_writer(deadline);
    }
}

fn finish(status: std::process::ExitStatus, stderr: String) -> Result<(), PerfError> {
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(PerfError::ShutdownExit { code, stderr }),
        None => Err(PerfError::ShutdownSignal { stderr }),
    }
}

/// Bounded budget for best-effort teardown work (kill/reap and helper
/// finalization) on error and drop paths.
const CLEANUP_BUDGET: Duration = Duration::from_secs(1);

/// Poll `child` for exit until `deadline` without killing it. Reports a
/// polling failure as [`PerfError::ChildWait`] and a deadline expiry as
/// [`PerfError::ChildWaitTimeout`] instead of blocking indefinitely.
fn wait_child_until(child: &mut Child, deadline: Instant) -> Result<ExitStatus, PerfError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err(PerfError::ChildWaitTimeout);
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(source) => return Err(PerfError::ChildWait { source }),
        }
    }
}

/// Kill `child` and reap it by `deadline`. A failed kill is retried through
/// one status poll: an already-exited child resolves the race, so the stale
/// kill failure is not reported as a defect.
fn kill_child_until(child: &mut Child, deadline: Instant) -> Result<ExitStatus, PerfError> {
    if let Err(source) = child.kill() {
        // The kill may have raced with an exit we had not yet observed.
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            // The kill failure remains the actionable evidence either way.
            Ok(None) | Err(_) => return Err(PerfError::ChildKill { source }),
        }
    }
    wait_child_until(child, deadline)
}

/// Wait for a session helper thread up to `deadline`.
///
/// `std` cannot join with a timeout, so the handle is polled with
/// [`JoinHandle::is_finished`]; if it finishes in time the value (and any
/// panic) is reported, and past the deadline the handle is detached — the
/// thread finishes in the background — and [`PerfError::ThreadJoinTimeout`]
/// is returned. Drop paths may intentionally discard this result.
fn wait_join<T: Send + 'static>(
    handle: JoinHandle<T>,
    role: &'static str,
    deadline: Instant,
) -> Result<Option<T>, PerfError> {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            // Detach: the thread finishes in the background; we cannot wait.
            return Err(PerfError::ThreadJoinTimeout { role });
        }
        thread::sleep(Duration::from_millis(1));
    }
    match handle.join() {
        Ok(value) => Ok(Some(value)),
        Err(_) => Err(PerfError::ThreadPanicked { role }),
    }
}

/// Clean up after a failed spawn: boundedly kill/reap the child and join the
/// already-started helpers, then wrap `primary` with any secondary evidence.
fn cleanup_failed_spawn(
    primary: PerfError,
    child: &mut Child,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<StderrRead>>,
) -> PerfError {
    let deadline = Instant::now() + CLEANUP_BUDGET;
    let mut cleanup = Vec::new();
    if let Err(error) = kill_child_until(child, deadline) {
        cleanup.push(error);
    }
    let mut stderr = String::new();
    if let Some(handle) = stderr_thread {
        match wait_join(handle, "stderr", deadline) {
            Ok(Some(capture)) => match capture.error {
                None => stderr = String::from_utf8_lossy(&capture.bytes).into_owned(),
                Some(source) => cleanup.push(PerfError::StderrRead {
                    source,
                    partial: String::from_utf8_lossy(&capture.bytes).into_owned(),
                }),
            },
            // Unreachable for a `StderrRead` handle: a successful join
            // always yields the capture.
            Ok(None) => {}
            Err(error) => cleanup.push(error),
        }
    }
    if let Some(handle) = reader
        && let Err(error) = wait_join(handle, "reader", deadline).map(|_| ())
    {
        cleanup.push(error);
    }
    if let Some(handle) = writer
        && let Err(error) = wait_join(handle, "writer", deadline).map(|_| ())
    {
        cleanup.push(error);
    }
    lifecycle(primary, stderr, cleanup)
}

/// Attach stderr and secondary cleanup failures to `primary`, or return it
/// unchanged when no secondary evidence exists. The primary is never
/// masked: it stays the `source` of the wrapper.
fn lifecycle(primary: PerfError, stderr: String, cleanup: Vec<PerfError>) -> PerfError {
    if stderr.is_empty() && cleanup.is_empty() {
        primary
    } else {
        PerfError::Lifecycle {
            primary: Box::new(primary),
            stderr,
            cleanup,
        }
    }
}

/// Reader-thread body: decode every frame, stamp it, and forward it.
///
/// All messages decoded from one read are stamped with the same receipt
/// instant; their relative order is preserved by `seq`. A decode failure is
/// forwarded as [`SessionEvent::DecodeError`] and ends the reader.
fn reader_loop(stdout: ChildStdout, sender: &Sender<SessionIo>) {
    let mut stdout = stdout;
    let mut decoder = IncrementalDecoder::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut seq: u64 = 0;
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) | Err(_) => return, // EOF or read error: reader stops.
            Ok(n) => {
                let stamped = Instant::now();
                match decoder.feed(&buffer[..n]) {
                    Ok(messages) => {
                        for message in messages {
                            let Some(event) = session_event(message) else {
                                continue;
                            };
                            seq = seq.wrapping_add(1);
                            let timed = TimedMessage {
                                at: stamped,
                                seq,
                                event,
                            };
                            if sender.send(SessionIo::Read(timed)).is_err() {
                                return; // Session dropped: nothing to serve.
                            }
                        }
                    }
                    Err(error) => {
                        seq = seq.wrapping_add(1);
                        let _ = sender.send(SessionIo::Read(TimedMessage {
                            at: stamped,
                            seq,
                            event: SessionEvent::DecodeError {
                                message: error.to_string(),
                            },
                        }));
                        return;
                    }
                }
            }
        }
    }
}

/// Writer-thread body: the only blocking pipe writes in the module.
///
/// Owns the child's stdin and processes `(id, bytes)` jobs in FIFO order,
/// reporting each completion as [`SessionIo::Written`] or
/// [`SessionIo::WriteFailed`]. When the job channel closes, `sink` drops
/// and stdin closes (the editor's EOF path).
fn writer_loop<W: Write + Send + 'static>(
    mut sink: W,
    jobs: Receiver<(u32, Vec<u8>)>,
    reports: &Sender<SessionIo>,
) {
    for (id, bytes) in jobs {
        let result = sink.write_all(&bytes).and_then(|()| sink.flush());
        let event = match result {
            Ok(()) => SessionIo::Written(id),
            Err(source) => SessionIo::WriteFailed(id, source.to_string()),
        };
        if reports.send(event).is_err() {
            return; // Session dropped: nothing to report.
        }
    }
}

/// Pull the next unified-channel event, bounded by the absolute `deadline`;
/// the reported `waited` budget is the session `timeout`.
fn next_io_message(
    incoming: &Receiver<SessionIo>,
    timeout: Duration,
    deadline: Instant,
) -> Result<SessionIo, PerfError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(PerfError::Timeout { waited: timeout });
    }
    match incoming.recv_timeout(deadline.saturating_duration_since(now)) {
        Ok(event) => Ok(event),
        Err(RecvTimeoutError::Timeout) => Err(PerfError::Timeout { waited: timeout }),
        Err(RecvTimeoutError::Disconnected) => Err(PerfError::ChildExited),
    }
}

/// Pull the next message for a bounded wait: the per-send receipt first
/// (reads observed before the write completed, in wire order), then the
/// live channel.
///
/// This is what keeps the ack/read race closed without touching the general
/// stash: a receipt only ever holds reads observed after *this* send's
/// enqueue, so draining it before `incoming` can never let pre-request
/// stale traffic satisfy a later window.
fn next_with_receipt(
    receipt: &mut VecDeque<TimedMessage>,
    next: &mut dyn FnMut() -> Result<TimedMessage, PerfError>,
) -> Result<TimedMessage, PerfError> {
    match receipt.pop_front() {
        Some(message) => Ok(message),
        None => next(),
    }
}

/// Next decoded reader event, bounded by `deadline`.
///
/// Writer completions for abandoned writes are skipped; writer failure
/// surfaces as a broken pipe (every later send fails the same way, so the
/// session becomes unusable at its next request rather than silently).
fn next_read(
    incoming: &Receiver<SessionIo>,
    timeout: Duration,
    deadline: Instant,
) -> Result<TimedMessage, PerfError> {
    loop {
        match next_io_message(incoming, timeout, deadline)? {
            SessionIo::Read(timed) => return Ok(timed),
            SessionIo::Written(_) => {}
            SessionIo::WriteFailed(..) => {
                return Err(PerfError::Write {
                    source: io::Error::new(io::ErrorKind::BrokenPipe, "stdin writer failed"),
                });
            }
        }
    }
}

/// Wait, bounded by the caller's absolute deadline, for the writer's
/// completion of exactly `id`.
///
/// Reader events decoded meanwhile are deferred in wire order; completions
/// of earlier abandoned writes are skipped. A failed write for this id
/// surfaces as [`PerfError::Write`]. The session thread blocks only on the
/// deadline-bounded receive, never on the pipe itself.
fn await_write(
    id: u32,
    deferred: &mut VecDeque<TimedMessage>,
    next: &mut dyn FnMut() -> Result<SessionIo, PerfError>,
) -> Result<(), PerfError> {
    loop {
        match next()? {
            SessionIo::Read(timed) => deferred.push_back(timed),
            SessionIo::Written(found) if found == id => return Ok(()),
            SessionIo::WriteFailed(found, message) if found == id => {
                return Err(PerfError::Write {
                    source: io::Error::new(io::ErrorKind::BrokenPipe, message),
                });
            }
            SessionIo::Written(_) | SessionIo::WriteFailed(..) => {}
        }
    }
}

/// Consume stdout in wire order from `R`'s send until the exact `F`
/// response, then prove the geometry.
///
/// - Every redraw batch decoded before `F` is applied exactly once, in wire
///   order (both engines may emit `R`'s redraws before or after `R`'s
///   response; the fence does not depend on that ordering).
/// - The exact response to `R` must arrive and be successful.
/// - Only the exact response to the never-reused `F` msgid crosses the
///   fence; `F` before `R` is a protocol violation.
/// - Unrelated responses and notifications are deferred in their original
///   order; bytes after `F` remain queued.
/// - At the fence, grid 1 must compose at exactly the configured geometry.
///
/// `next` pulls the next [`TimedMessage`]: the session wraps
/// `next_read` (absolute-deadline bounded), while the deterministic tests
/// drain a preloaded transcript.
fn setup_fence_core(
    state: &mut TuiState,
    resize_msgid: u32,
    fence_msgid: u32,
    configured: (u16, u16),
    deferred: &mut VecDeque<TimedMessage>,
    next: &mut dyn FnMut() -> Result<TimedMessage, PerfError>,
) -> Result<(), PerfError> {
    let mut resize_ok = false;
    loop {
        let message = next()?;
        match message.event {
            SessionEvent::Redraw(events) => apply_batch(state, events)?,
            SessionEvent::DecodeError { message } => {
                return Err(PerfError::Decode { message });
            }
            SessionEvent::Response {
                msgid: found,
                result,
            } if found == resize_msgid => {
                result.map_err(|error| {
                    PerfError::Protocol(format!("setup resize response: {error}"))
                })?;
                resize_ok = true;
            }
            SessionEvent::Response {
                msgid: found,
                result,
            } if found == fence_msgid => {
                if !resize_ok {
                    return Err(PerfError::Protocol(
                        "setup fence response decoded before the resize response".to_owned(),
                    ));
                }
                result.map_err(|error| {
                    PerfError::Protocol(format!("setup fence response: {error}"))
                })?;
                return require_grid_dimensions(state, configured);
            }
            other => deferred.push_back(TimedMessage {
                at: message.at,
                seq: message.seq,
                event: other,
            }),
        }
    }
}

/// Post-fence geometry proof: grid 1 must compose at exactly the configured
/// dimensions once `F` crossed the fence.
fn require_grid_dimensions(state: &TuiState, configured: (u16, u16)) -> Result<(), PerfError> {
    let snapshot = UiSnapshot::from_state(state)?;
    let observed = snapshot.dimensions;
    let expected = (usize::from(configured.0), usize::from(configured.1));
    if observed == expected {
        Ok(())
    } else {
        Err(PerfError::GeometryMismatch {
            configured,
            observed,
        })
    }
}

/// Consume one timed request window over an injected source.
///
/// `receipt` holds the reads observed before the request's write completed;
/// it is drained ahead of `next` (the live channel) via
/// [`next_with_receipt`], so the output belonging to this very request can
/// arrive before the writer's completion report without being lost — and
/// without the general stash ever being consulted.
///
/// Causal ordering (the `nvim_input` workload shape): the editor
/// acknowledges the request *before* processing it (`api/vim.c`), so the
/// exact non-reused response is the marker that the request's processing
/// and redraw follow on the wire. A complete flush decoded before that
/// response is stale traffic: it is applied to the state but never ends
/// the window. The endpoint is the first complete flush after the exact
/// response, in stdout/`seq` order.
fn request_to_flush_core(
    state: &mut TuiState,
    started: Instant,
    msgid: u32,
    receipt: &mut VecDeque<TimedMessage>,
    deferred: &mut VecDeque<TimedMessage>,
    next: &mut dyn FnMut() -> Result<TimedMessage, PerfError>,
) -> Result<(StageTiming, Option<Result<Object, String>>), PerfError> {
    let mut response = None;
    let mut responded = false;
    let mut responded_at = None;
    loop {
        let message = next_with_receipt(receipt, next)?;
        match message.event {
            SessionEvent::Response {
                msgid: found,
                result,
            } if found == msgid => {
                responded_at = Some(message.at);
                response = Some(result);
                responded = true;
            }
            SessionEvent::Redraw(events) => {
                let complete = events.last().is_some_and(|event| event.name == "flush");
                apply_batch(state, events)?;
                if responded && complete {
                    let total = message.at.duration_since(started);
                    let response_dur =
                        responded_at.and_then(|at| at.checked_duration_since(started));
                    return Ok((
                        StageTiming {
                            total,
                            response: response_dur,
                        },
                        response.take(),
                    ));
                }
            }
            event => match event {
                SessionEvent::DecodeError { message } => {
                    return Err(PerfError::Decode { message });
                }
                other => deferred.push_back(TimedMessage {
                    at: message.at,
                    seq: message.seq,
                    event: other,
                }),
            },
        }
    }
}

/// Convert decoded redraw events and apply them to the headless screen.
fn apply_batch(state: &mut TuiState, events: Vec<DecodedRedraw>) -> Result<(), PerfError> {
    let batch: Vec<RedrawEvent> = events
        .into_iter()
        .map(|event| RedrawEvent {
            name: OxStr::from(event.name.as_str()),
            argsets: event.argsets,
        })
        .collect();
    state
        .apply_redraw(&batch, TimeMs(0))
        .map_err(|error| PerfError::Protocol(format!("redraw: {error}")))
}

/// Apply one redraw batch and compose the snapshot from exactly that state,
/// so a flush stamp and its snapshot describe the same frame.
fn boundary_snapshot(
    state: &mut TuiState,
    events: Vec<DecodedRedraw>,
) -> Result<UiSnapshot, PerfError> {
    apply_batch(state, events)?;
    UiSnapshot::from_state(state)
}

/// Parse one `/proc/<pid>/status` line, yielding `Ok(None)` when it is not
/// a `VmHWM` entry.
///
/// # Errors
///
/// [`PerfError::PeakRssMalformed`] when the line names `VmHWM` but its value
/// is not a `u64` followed by exactly `kB`.
fn parse_vmhwm_line(line: &str) -> Result<Option<u64>, PerfError> {
    let Some(rest) = line.strip_prefix("VmHWM:") else {
        return Ok(None);
    };
    let mut fields = rest.split_whitespace();
    let Some(value) = fields.next() else {
        return Err(PerfError::PeakRssMalformed {
            line: line.to_owned(),
        });
    };
    let value = value
        .parse::<u64>()
        .map_err(|_| PerfError::PeakRssMalformed {
            line: line.to_owned(),
        })?;
    match fields.next() {
        Some("kB") if fields.next().is_none() => Ok(Some(value)),
        _ => Err(PerfError::PeakRssMalformed {
            line: line.to_owned(),
        }),
    }
}

/// Convert a wire message to its session-level event.
///
/// Redraw conversion errors are [`PerfError::Protocol`] by contract, so a
/// malformed redraw batch ends the session at first consumption rather than
/// being silently dropped here; `None` is reserved for editor-initiated
/// requests, which embedded editors do not send.
fn session_event(message: Message) -> Option<SessionEvent> {
    match message {
        Message::Response { msgid, result } => Some(SessionEvent::Response {
            msgid,
            result: result.map_err(|error| error.message().to_owned()),
        }),
        Message::Notification { method, params } => {
            if method.as_bytes() == b"redraw" {
                match decode_redraw_params(&params) {
                    Ok(events) => Some(SessionEvent::Redraw(events)),
                    Err(PerfError::Protocol(message)) => Some(SessionEvent::DecodeError {
                        message: format!("redraw batch: {message}"),
                    }),
                    Err(_) => None,
                }
            } else {
                Some(SessionEvent::Notification {
                    method: String::from_utf8_lossy(method.as_bytes()).into_owned(),
                    params,
                })
            }
        }
        Message::Request { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_types::ApiError;
    use std::sync::{Arc, Mutex};

    /// An argset of plain integers.
    fn ints(values: &[i64]) -> Object {
        Object::Array(values.iter().map(|value| Object::Integer(*value)).collect())
    }

    /// One decoded redraw event entry: `[name, argset, …]`.
    fn entry(name: &str, argsets: Vec<Object>) -> Object {
        let mut parts = vec![Object::String(name.into())];
        parts.extend(argsets);
        Object::Array(parts)
    }

    /// A `"redraw"` notification whose params are the event entries.
    fn redraw_message(entries: Vec<Object>) -> Message {
        Message::Notification {
            method: OxStr::from("redraw"),
            params: entries,
        }
    }

    /// A complete batch: resize grid 1 to 4x2, write `text` at row `row`,
    /// park the cursor at (0, 0), and end with `flush`.
    fn complete_grid_batch(text: &str, row: i64) -> Vec<DecodedRedraw> {
        decode_redraw_params(&[
            entry("grid_resize", vec![ints(&[1, 4, 2])]),
            entry(
                "grid_line",
                vec![Object::Array(vec![
                    Object::Integer(1),
                    Object::Integer(row),
                    Object::Integer(0),
                    Object::Array(vec![Object::Array(vec![
                        Object::String(text.into()),
                        Object::Integer(0),
                    ])]),
                    Object::Boolean(true),
                ])],
            ),
            entry("grid_cursor_goto", vec![ints(&[1, 0, 0])]),
            entry("flush", vec![]),
        ])
        .expect("fixture batch decodes")
    }

    fn response_event(msgid: u32, result: Result<Object, String>) -> SessionEvent {
        SessionEvent::Response { msgid, result }
    }

    /// A complete batch that resizes grid 1 to `width x height` and ends in
    /// `flush`: the setup output shape for one resize request.
    fn resize_batch(width: i64, height: i64, text: &str) -> Vec<DecodedRedraw> {
        decode_redraw_params(&[
            entry("grid_resize", vec![ints(&[1, width, height])]),
            entry(
                "grid_line",
                vec![Object::Array(vec![
                    Object::Integer(1),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Array(vec![Object::Array(vec![
                        Object::String(text.into()),
                        Object::Integer(0),
                    ])]),
                    Object::Boolean(true),
                ])],
            ),
            entry("grid_cursor_goto", vec![ints(&[1, 0, 0])]),
            entry("flush", vec![]),
        ])
        .expect("fixture batch decodes")
    }

    /// Queue `events` as a complete redraw message at the back of `queue`.
    fn enqueue(queue: &mut VecDeque<TimedMessage>, seq: u64, events: Vec<DecodedRedraw>) {
        queue.push_back(TimedMessage {
            at: Instant::now(),
            seq,
            event: SessionEvent::Redraw(events),
        });
    }

    /// Queue one non-redraw event at the back of `queue`.
    fn enqueue_event(queue: &mut VecDeque<TimedMessage>, seq: u64, event: SessionEvent) {
        queue.push_back(TimedMessage {
            at: Instant::now(),
            seq,
            event,
        });
    }

    /// A `next` source for the fence that drains a preloaded transcript and
    /// reports the shared timeout error once it is empty.
    fn queue_next(
        queue: &mut VecDeque<TimedMessage>,
    ) -> impl FnMut() -> Result<TimedMessage, PerfError> + '_ {
        move || {
            queue.pop_front().ok_or(PerfError::Timeout {
                waited: Duration::ZERO,
            })
        }
    }

    const CONFIGURED: (u16, u16) = (80, 24);
    const RESIZE_ID: u32 = 7;
    const FENCE_ID: u32 = 9;

    #[test]
    fn session_event_converts_response_results() {
        let event = session_event(Message::Response {
            msgid: 7,
            result: Ok(Object::Integer(3)),
        })
        .expect("success response converts");
        match event {
            SessionEvent::Response { msgid, result } => {
                assert_eq!(msgid, 7);
                assert_eq!(result, Ok(Object::Integer(3)));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn session_event_maps_error_response_message() {
        let event = session_event(Message::Response {
            msgid: 1,
            result: Err(ApiError::exception("boom")),
        })
        .expect("error response converts");
        match event {
            SessionEvent::Response { msgid, result } => {
                assert_eq!(msgid, 1);
                assert_eq!(result, Err("boom".to_owned()));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn session_event_decodes_redraw_batches_in_order() {
        let event = session_event(redraw_message(vec![
            entry("grid_resize", vec![ints(&[1, 4, 2])]),
            entry("flush", vec![]),
        ]))
        .expect("redraw converts");
        match event {
            SessionEvent::Redraw(events) => {
                let names: Vec<&str> = events.iter().map(|redraw| redraw.name.as_str()).collect();
                assert_eq!(names, ["grid_resize", "flush"]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn decode_redraw_params_decodes_notification_entries_directly_in_order() {
        let events = decode_redraw_params(&[
            entry("grid_resize", vec![ints(&[1, 4, 2])]),
            entry("flush", vec![]),
        ])
        .expect("real notification params decode");
        let names: Vec<&str> = events.iter().map(|redraw| redraw.name.as_str()).collect();
        assert_eq!(names, ["grid_resize", "flush"]);
        assert_eq!(
            events[0].argsets,
            vec![vec![
                Object::Integer(1),
                Object::Integer(4),
                Object::Integer(2),
            ]]
        );
        assert!(events[1].argsets.is_empty());
    }

    #[test]
    fn decode_redraw_params_rejects_scalar_entry() {
        let error = decode_redraw_params(&[Object::Integer(7)]).expect_err("scalar entry fails");
        match error {
            PerfError::Protocol(message) => {
                assert_eq!(message, "redraw event entry must be an array");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn decode_redraw_params_treats_empty_params_as_empty_batch() {
        let events = decode_redraw_params(&[]).expect("empty params are a valid empty batch");
        assert!(events.is_empty());
    }

    #[test]
    fn session_event_flags_malformed_redraw_as_decode_error() {
        let event = session_event(Message::Notification {
            method: OxStr::from("redraw"),
            params: vec![Object::Nil],
        });
        match event {
            Some(SessionEvent::DecodeError { message }) => {
                assert_eq!(message, "redraw batch: redraw event entry must be an array");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn session_event_passes_through_other_notifications() {
        let event = session_event(Message::Notification {
            method: OxStr::from("nvim_out_write"),
            params: vec![Object::Array(vec![Object::String("hi".into())])],
        })
        .expect("notification converts");
        match event {
            SessionEvent::Notification { method, params } => {
                assert_eq!(method, "nvim_out_write");
                assert_eq!(params.len(), 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn session_event_drops_editor_initiated_requests() {
        assert!(
            session_event(Message::Request {
                msgid: 3,
                method: OxStr::from("nvim_eval"),
                params: vec![],
            })
            .is_none()
        );
    }

    #[test]
    fn complete_flush_detection_matches_trailing_flush() {
        assert!(SessionEvent::Redraw(complete_grid_batch("abcd", 0)).is_complete_flush());
        let partial = SessionEvent::Redraw(
            decode_redraw_params(&[
                entry("grid_line", vec![ints(&[1, 0, 0, 1])]),
                entry("flush", vec![]),
            ])
            .expect("partial batch decodes"),
        );
        assert!(partial.is_complete_flush());
        assert!(!response_event(1, Ok(Object::Nil)).is_complete_flush());
        assert!(
            !SessionEvent::Notification {
                method: "redraw".to_owned(),
                params: vec![],
            }
            .is_complete_flush()
        );
        assert!(
            !SessionEvent::DecodeError {
                message: String::new()
            }
            .is_complete_flush()
        );
    }

    #[test]
    fn boundary_snapshot_captures_the_flush_frame_not_later_state() {
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let boundary = boundary_snapshot(&mut state, complete_grid_batch("abcd", 0))
            .expect("boundary composes");
        assert!(boundary.rendered_grid.contains("abcd"));

        // A later frame changes the state and must not leak into the
        // boundary snapshot.
        apply_batch(&mut state, complete_grid_batch("xy", 1)).expect("later batch applies");
        let after = UiSnapshot::from_state(&state).expect("post frame composes");
        assert_ne!(after, boundary);

        // Recomposing from a fresh state fed only the boundary batch
        // reproduces the captured snapshot exactly: state and stamp
        // describe the same frame.
        let mut replay = TuiState::new(None, MotionPolicy::Reduced);
        let rebuilt = boundary_snapshot(&mut replay, complete_grid_batch("abcd", 0))
            .expect("replay composes");
        assert_eq!(rebuilt, boundary);
    }

    #[test]
    fn parse_vmhwm_line_reads_kib_values_and_skips_other_lines() {
        assert_eq!(
            parse_vmhwm_line("VmHWM:\t  1234 kB").expect("parses"),
            Some(1234)
        );
        assert_eq!(parse_vmhwm_line("VmHWM: 0 kB").expect("parses"), Some(0));
        assert_eq!(
            parse_vmhwm_line("VmRSS:      9000 kB").expect("skipped"),
            None
        );
        assert_eq!(parse_vmhwm_line("Name:\tobs").expect("skipped"), None);
    }

    #[test]
    fn parse_vmhwm_line_rejects_malformed_lines() {
        for line in [
            "VmHWM: 12 mB",
            "VmHWM: 12 kB extra",
            "VmHWM: x kB",
            "VmHWM:",
        ] {
            match parse_vmhwm_line(line) {
                Err(PerfError::PeakRssMalformed { line: found }) => assert_eq!(found, line),
                other => panic!("unexpected parse result for {line:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn fence_completes_in_neovim_order_response_then_redraws() {
        // Neovim serializes the response after the handler returns; the
        // resize redraws flush around the next loop iteration. The fence
        // does not depend on redraws preceding the responses: it requires
        // R's success and, at F's response, the applied geometry.
        let mut queue = VecDeque::new();
        enqueue_event(&mut queue, 0, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 1, resize_batch(80, 24, "ready"));
        enqueue_event(&mut queue, 2, response_event(FENCE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect("neovim order crosses the fence");
        assert!(deferred.is_empty());
        let snapshot = UiSnapshot::from_state(&state).expect("state composes");
        assert_eq!(snapshot.dimensions, (80, 24));
    }

    #[test]
    fn fence_completes_in_oxvim_order_redraws_then_response() {
        // Oxvim writes a request's entire returned write vector before
        // processing the next request, so R's redraws precede the
        // responses. Same fence, same outcome.
        let mut queue = VecDeque::new();
        enqueue(&mut queue, 0, resize_batch(80, 24, "ready"));
        enqueue_event(&mut queue, 1, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue_event(&mut queue, 2, response_event(FENCE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect("oxvim order crosses the fence");
        assert!(deferred.is_empty());
    }

    #[test]
    fn fence_applies_stale_and_foreign_traffic_once_in_order() {
        // Stale traffic decoded before R's response — including an exact
        // configured-size grid_resize frame from earlier traffic — is
        // applied but cannot complete the fence: only the exact F response
        // does. Unrelated traffic defers in wire order.
        let mut queue = VecDeque::new();
        enqueue(&mut queue, 0, resize_batch(80, 24, "stale"));
        enqueue_event(
            &mut queue,
            1,
            SessionEvent::Notification {
                method: "nvim_out_write".to_owned(),
                params: vec![],
            },
        );
        enqueue_event(
            &mut queue,
            2,
            response_event(11, Err("a foreign response".to_owned())),
        );
        enqueue_event(&mut queue, 3, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 4, resize_batch(80, 24, "live"));
        enqueue_event(&mut queue, 5, response_event(FENCE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect("stale traffic cannot block the fence");
        let seqs: Vec<u64> = deferred.iter().map(|message| message.seq).collect();
        assert_eq!(seqs, [1, 2], "unrelated traffic defers in wire order");
        let snapshot = UiSnapshot::from_state(&state).expect("state composes");
        assert_eq!(snapshot.dimensions, (80, 24));
        // Both frames were applied exactly once, in wire order: the last
        // write wins the row text.
        assert!(snapshot.rendered_grid.contains("live"));
    }

    #[test]
    fn fence_requires_the_exact_fence_response() {
        // R success alone, arbitrary flushes, foreign responses, and stale
        // responses never complete the fence: only the exact F response
        // does. The transcript ends without it: timeout, not success.
        let mut queue = VecDeque::new();
        enqueue_event(&mut queue, 0, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 1, resize_batch(80, 24, "ready"));
        enqueue_event(&mut queue, 2, response_event(11, Ok(Object::Integer(1))));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect_err("an arbitrary response never crosses the fence");
        assert!(matches!(error, PerfError::Timeout { .. }));
        // Unrelated traffic survives for the caller's restore.
        let seqs: Vec<u64> = deferred.iter().map(|message| message.seq).collect();
        assert_eq!(seqs, [2]);
    }

    #[test]
    fn fence_rejects_fence_response_before_resize_response() {
        let mut queue = VecDeque::new();
        enqueue_event(&mut queue, 0, response_event(FENCE_ID, Ok(Object::Nil)));
        enqueue_event(&mut queue, 1, response_event(RESIZE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect_err("F before R is a protocol violation");
        match error {
            PerfError::Protocol(message) => {
                assert!(message.contains("before the resize response"));
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn fence_rejects_resize_api_error() {
        let mut queue = VecDeque::new();
        enqueue_event(
            &mut queue,
            0,
            response_event(RESIZE_ID, Err("resize refused".to_owned())),
        );
        enqueue_event(&mut queue, 1, response_event(FENCE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect_err("an R API error fails the fence");
        match error {
            PerfError::Protocol(message) => {
                assert!(message.contains("setup resize response"));
                assert!(message.contains("resize refused"));
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn fence_rejects_fence_api_error() {
        let mut queue = VecDeque::new();
        enqueue_event(&mut queue, 0, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue_event(
            &mut queue,
            1,
            response_event(FENCE_ID, Err("fence refused".to_owned())),
        );
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect_err("an F API error fails the fence");
        match error {
            PerfError::Protocol(message) => {
                assert!(message.contains("setup fence response"));
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn fence_propagates_decode_errors() {
        let mut queue = VecDeque::new();
        enqueue_event(
            &mut queue,
            0,
            SessionEvent::DecodeError {
                message: "truncated".to_owned(),
            },
        );
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect_err("decode error propagates");
        assert!(matches!(error, PerfError::Decode { .. }));
    }

    #[test]
    fn fence_propagates_child_exit() {
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let mut next = || -> Result<TimedMessage, PerfError> { Err(PerfError::ChildExited) };
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut next,
        )
        .expect_err("EOF fails the fence");
        assert!(matches!(error, PerfError::ChildExited));
    }

    #[test]
    fn fence_times_out_when_the_fence_response_never_arrives() {
        let mut queue = VecDeque::new();
        enqueue_event(&mut queue, 0, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 1, resize_batch(80, 24, "ready"));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect_err("an exhausted transcript times out");
        assert!(matches!(error, PerfError::Timeout { .. }));
    }

    #[test]
    fn fence_checks_geometry_at_the_fence() {
        // The resize applied 81x24 while the session is configured for
        // 80x24: the fence refuses Ready rather than silently accepting a
        // clamped or foreign geometry.
        let mut queue = VecDeque::new();
        enqueue_event(&mut queue, 0, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 1, resize_batch(81, 24, "wrong"));
        enqueue_event(&mut queue, 2, response_event(FENCE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let error = setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect_err("a foreign geometry fails the fence");
        match error {
            PerfError::GeometryMismatch {
                configured,
                observed,
            } => {
                assert_eq!(configured, (80, 24));
                assert_eq!(observed, (81, 24));
            }
            other => panic!("expected GeometryMismatch, got {other:?}"),
        }
    }

    #[test]
    fn fence_leaves_bytes_after_the_fence_queued() {
        // Stop exactly at F: bytes after the fence remain queued for later
        // waits, and nothing beyond F is applied here.
        let mut queue = VecDeque::new();
        enqueue_event(&mut queue, 0, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 1, resize_batch(80, 24, "ready"));
        enqueue_event(&mut queue, 2, response_event(FENCE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 3, resize_batch(99, 99, "post"));
        enqueue_event(&mut queue, 4, response_event(12, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect("fence crosses at F");
        assert_eq!(queue.len(), 2, "post-fence bytes stay queued");
        assert!(deferred.is_empty());
        let snapshot = UiSnapshot::from_state(&state).expect("state composes");
        assert!(
            !snapshot.rendered_grid.contains("post"),
            "post-fence redraws are not applied by the fence"
        );
    }

    #[test]
    fn fence_prepends_existing_deferred_entries() {
        // Deferred entries already held by the caller predate anything the
        // fence defers: appending preserves their wire order.
        let mut queue = VecDeque::new();
        enqueue_event(
            &mut queue,
            3,
            SessionEvent::Notification {
                method: "nvim_out_write".to_owned(),
                params: vec![],
            },
        );
        enqueue_event(&mut queue, 4, response_event(RESIZE_ID, Ok(Object::Nil)));
        enqueue(&mut queue, 5, resize_batch(80, 24, "ready"));
        enqueue_event(&mut queue, 6, response_event(FENCE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        for seq in 0..3u64 {
            enqueue_event(
                &mut deferred,
                seq,
                SessionEvent::Notification {
                    method: "earlier".to_owned(),
                    params: vec![],
                },
            );
        }
        setup_fence_core(
            &mut state,
            RESIZE_ID,
            FENCE_ID,
            CONFIGURED,
            &mut deferred,
            &mut queue_next(&mut queue),
        )
        .expect("fence crosses with deferred order preserved");
        let seqs: Vec<u64> = deferred.iter().map(|message| message.seq).collect();
        assert_eq!(seqs, [0, 1, 2, 3]);
    }

    #[test]
    fn ui_size_accepts_the_upstream_clamp_range() {
        for (width, height, expected) in [
            (12u32, 2u32, (12u16, 2u16)),
            (80, 24, (80, 24)),
            (10_000, 1_000, (10_000, 1_000)),
        ] {
            let size = UiSize::new(width, height).expect("inside the clamp range");
            assert_eq!(size.dimensions(), expected);
        }
    }

    #[test]
    fn ui_size_rejects_out_of_range_geometry() {
        for (width, height) in [
            (0, 24),
            (1, 24),
            (11, 24),
            (10_001, 24),
            (i32::MAX as u32, 24),
            (i32::MAX as u32 + 1, 24),
            (u32::MAX, 24),
            (80, 0),
            (80, 1),
            (80, 1_001),
            (80, i32::MAX as u32),
            (80, u32::MAX),
        ] {
            match UiSize::new(width, height) {
                Err(PerfError::InvalidUiSize {
                    width: found_w,
                    height: found_h,
                }) => {
                    assert_eq!((found_w, found_h), (width, height));
                }
                other => panic!("unexpected result for {width}x{height}: {other:?}"),
            }
        }
    }

    #[test]
    fn msgid_counter_allocates_checked_ids_without_wrapping() {
        let mut fresh = CheckedMsgidCounter::new();
        assert_eq!(fresh.next_id().expect("first id"), 1);

        // ids are strictly increasing and never reuse 0 or a past value.
        let mut counter = CheckedMsgidCounter {
            next: Some(u32::MAX - 1),
        };
        assert_eq!(counter.next_id().expect("penultimate id"), u32::MAX - 1);
        assert_eq!(counter.next_id().expect("final id"), u32::MAX);
        assert!(matches!(
            counter.next_id(),
            Err(PerfError::RequestIdExhausted)
        ));
        assert!(matches!(
            counter.next_id(),
            Err(PerfError::RequestIdExhausted)
        ));
    }

    /// A sink that records every `write_all` payload.
    struct RecordingSink {
        log: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Write for RecordingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.log.lock().expect("log lock").push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A sink that always fails, modeling a broken pipe.
    struct FailingSink;

    impl Write for FailingSink {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken"))
        }
    }

    #[test]
    fn writer_loop_reports_fifo_completions() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (reports_tx, reports_rx) = mpsc::channel();
        let sink = RecordingSink {
            log: Arc::clone(&log),
        };
        let worker = thread::spawn(move || writer_loop(sink, jobs_rx, &reports_tx));
        jobs_tx.send((7, b"a".to_vec())).expect("job 1 queued");
        jobs_tx.send((9, b"bb".to_vec())).expect("job 2 queued");
        drop(jobs_tx); // Close the FIFO: the writer drains and exits.
        match reports_rx.recv().expect("first report") {
            SessionIo::Written(id) => assert_eq!(id, 7),
            other => panic!("unexpected first report: {other:?}"),
        }
        match reports_rx.recv().expect("second report") {
            SessionIo::Written(id) => assert_eq!(id, 9),
            other => panic!("unexpected second report: {other:?}"),
        }
        worker.join().expect("writer exits when jobs close");
        assert_eq!(
            *log.lock().expect("log lock"),
            vec![b"a".to_vec(), b"bb".to_vec()],
            "jobs reach the sink in FIFO order"
        );
    }

    #[test]
    fn enqueue_quit_sends_fire_and_forget_command() {
        let mut counter = CheckedMsgidCounter::new();
        let (jobs_tx, jobs_rx) = mpsc::channel();
        assert!(enqueue_quit(&mut counter, Some(&jobs_tx)));
        let (id, bytes) = jobs_rx.recv().expect("quit request queued");
        assert_eq!(id, 1, "quit uses the session id space");
        // Round-trip the encoded frame: it must decode back to exactly one
        // nvim_command request carrying ["qa!"].
        let mut decoder = IncrementalDecoder::new();
        let messages = decoder.feed(&bytes).expect("quit frame decodes");
        let Message::Request {
            msgid,
            method,
            params,
        } = &messages[0]
        else {
            panic!("quit must be a request, got {:?}", messages[0]);
        };
        assert_eq!(*msgid, 1);
        assert_eq!(method.to_string_lossy(), "nvim_command");
        assert_eq!(params.as_slice(), &[Object::String(OxStr::from("qa!"))]);
    }

    #[test]
    fn enqueue_quit_is_best_effort_when_writer_is_gone() {
        let mut counter = CheckedMsgidCounter::new();
        // No writer at all: skipped, not an error.
        assert!(!enqueue_quit(&mut counter, None));
        // Dropped receiver: the send fails, still not an error.
        let (jobs_tx, jobs_rx) = mpsc::channel();
        drop(jobs_rx);
        assert!(!enqueue_quit(&mut counter, Some(&jobs_tx)));
        // Exhausted id space: skipped the same way.
        let mut exhausted = CheckedMsgidCounter { next: None };
        assert!(!enqueue_quit(&mut exhausted, Some(&jobs_tx)));
    }

    #[test]
    fn writer_loop_reports_write_failures() {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (reports_tx, reports_rx) = mpsc::channel();
        let worker = thread::spawn(move || writer_loop(FailingSink, jobs_rx, &reports_tx));
        jobs_tx.send((3, b"x".to_vec())).expect("job queued");
        drop(jobs_tx);
        match reports_rx.recv().expect("failure report") {
            SessionIo::WriteFailed(id, message) => {
                assert_eq!(id, 3);
                assert!(message.contains("broken"), "message: {message}");
            }
            other => panic!("unexpected report: {other:?}"),
        }
        worker.join().expect("writer exits when jobs close");
    }

    #[test]
    fn await_write_skips_foreign_completions() {
        let mut io = VecDeque::from(vec![
            SessionIo::Written(1), // abandoned earlier write: skipped.
            SessionIo::Written(5), // ours: completes the wait.
        ]);
        let mut deferred = VecDeque::new();
        let mut next = move || {
            io.pop_front().ok_or(PerfError::Timeout {
                waited: Duration::ZERO,
            })
        };
        await_write(5, &mut deferred, &mut next).expect("matching completion ends the wait");
        assert!(deferred.is_empty());
    }

    #[test]
    fn await_write_surfaces_matching_write_failure() {
        let mut io = VecDeque::from(vec![SessionIo::WriteFailed(5, "broken pipe".to_owned())]);
        let mut deferred = VecDeque::new();
        let mut next = move || {
            io.pop_front().ok_or(PerfError::Timeout {
                waited: Duration::ZERO,
            })
        };
        let error =
            await_write(5, &mut deferred, &mut next).expect_err("matching failure surfaces");
        match error {
            PerfError::Write { source } => {
                assert!(source.to_string().contains("broken pipe"));
            }
            other => panic!("expected Write error, got {other:?}"),
        }
    }

    #[test]
    fn await_write_times_out_without_blocking_past_the_deadline() {
        // A blocked writer never reports; reader events keep the session
        // thread fed and the shared deadline ends the wait deterministically.
        let mut io = VecDeque::from(vec![
            SessionIo::Written(1), // foreign completion: skipped.
            SessionIo::Read(TimedMessage {
                at: Instant::now(),
                seq: 0,
                event: SessionEvent::Notification {
                    method: "nvim_out_write".to_owned(),
                    params: vec![],
                },
            }),
        ]);
        let mut deferred = VecDeque::new();
        let mut next = move || {
            io.pop_front().ok_or(PerfError::Timeout {
                waited: Duration::ZERO,
            })
        };
        let error =
            await_write(5, &mut deferred, &mut next).expect_err("exhausted source times out");
        assert!(matches!(error, PerfError::Timeout { .. }));
        assert_eq!(deferred.len(), 1, "reader events defer in wire order");
    }

    #[test]
    fn await_write_defers_matching_reads_before_completion() {
        // The ack/read race shape: the editor's response and redraw decode
        // before the writer's completion report reaches the channel. The
        // wait ends on Written(5) and hands back both reads in wire order
        // as the per-send receipt.
        let mut io = VecDeque::from(vec![
            SessionIo::Read(TimedMessage {
                at: Instant::now(),
                seq: 0,
                event: response_event(5, Ok(Object::Integer(1))),
            }),
            SessionIo::Read(TimedMessage {
                at: Instant::now(),
                seq: 1,
                event: SessionEvent::Redraw(complete_grid_batch("abcd", 0)),
            }),
            SessionIo::Written(5),
        ]);
        let mut deferred = VecDeque::new();
        let mut next = move || {
            io.pop_front().ok_or(PerfError::Timeout {
                waited: Duration::ZERO,
            })
        };
        await_write(5, &mut deferred, &mut next).expect("matching completion ends the wait");
        assert_eq!(deferred.len(), 2, "both pre-completion reads are receipted");
        match &deferred[0].event {
            SessionEvent::Response { msgid, .. } => assert_eq!(*msgid, 5),
            other => panic!("unexpected receipt head: {other:?}"),
        }
        assert!(matches!(&deferred[1].event, SessionEvent::Redraw(_)));
    }

    #[test]
    fn next_with_receipt_drains_receipt_before_channel() {
        let mut receipt = VecDeque::new();
        enqueue_event(&mut receipt, 0, response_event(7, Ok(Object::Integer(1))));
        let mut channel = VecDeque::new();
        enqueue_event(&mut channel, 1, response_event(8, Ok(Object::Nil)));
        enqueue_event(&mut channel, 2, response_event(9, Ok(Object::Nil)));
        let mut next = move || next_with_receipt(&mut receipt, &mut queue_next(&mut channel));
        for expected_seq in 0..=2u64 {
            let message = next().expect("next message");
            assert_eq!(message.seq, expected_seq, "receipt drains before channel");
        }
    }

    #[test]
    fn request_to_flush_core_times_a_receipt_flush_without_panicking() {
        // Causal order: the exact response acks the request first, then the
        // post-response flush ends the window. Both decode during the send
        // window and arrive on the receipt; the stamp travels with the
        // endpoint and the duration is panic-free.
        let started = Instant::now();
        let mut receipt = VecDeque::new();
        enqueue_event(&mut receipt, 0, response_event(7, Ok(Object::Integer(1))));
        let flush_stamp = Instant::now();
        receipt.push_back(TimedMessage {
            at: flush_stamp,
            seq: 1,
            event: SessionEvent::Redraw(complete_grid_batch("abcd", 0)),
        });
        let mut channel = VecDeque::new();
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let (timing, response) = request_to_flush_core(
            &mut state,
            started,
            7,
            &mut receipt,
            &mut deferred,
            &mut queue_next(&mut channel),
        )
        .expect("the post-response flush ends the timed window");
        assert_eq!(response, Some(Ok(Object::Integer(1))));
        assert_eq!(
            timing.total,
            flush_stamp.duration_since(started),
            "the stamp travels with the endpoint"
        );
        let response_dur = timing.response.expect("the response stamp was observed");
        assert!(
            response_dur <= timing.total,
            "response must not exceed total"
        );
        assert!(receipt.is_empty(), "the whole receipt is drained");
        assert!(deferred.is_empty());
    }

    #[test]
    fn request_to_flush_core_applies_stale_flush_but_keeps_waiting() {
        // A complete flush decoded before the exact response is stale
        // traffic: it is applied to the state but cannot end the window.
        // The window ends only on the post-response flush, measured from
        // that flush's own stamp.
        let started = Instant::now();
        let mut receipt = VecDeque::new();
        enqueue(&mut receipt, 0, complete_grid_batch("old!", 1));
        let mut channel = VecDeque::new();
        enqueue_event(&mut channel, 1, response_event(7, Ok(Object::Integer(1))));
        let flush_stamp = Instant::now();
        channel.push_back(TimedMessage {
            at: flush_stamp,
            seq: 2,
            event: SessionEvent::Redraw(
                decode_redraw_params(&[
                    entry(
                        "grid_line",
                        vec![Object::Array(vec![
                            Object::Integer(1),
                            Object::Integer(0),
                            Object::Integer(0),
                            Object::Array(vec![Object::Array(vec![
                                Object::String("abcd".into()),
                                Object::Integer(0),
                            ])]),
                            Object::Boolean(true),
                        ])],
                    ),
                    entry("grid_cursor_goto", vec![ints(&[1, 0, 0])]),
                    entry("flush", vec![]),
                ])
                .expect("fixture batch decodes"),
            ),
        });
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let (timing, response) = request_to_flush_core(
            &mut state,
            started,
            7,
            &mut receipt,
            &mut deferred,
            &mut queue_next(&mut channel),
        )
        .expect("the post-response flush ends the timed window");
        assert_eq!(response, Some(Ok(Object::Integer(1))));
        assert_eq!(
            timing.total,
            flush_stamp.duration_since(started),
            "the endpoint is the post-response flush, not the stale one"
        );
        let response_dur = timing.response.expect("the response stamp was observed");
        assert!(
            response_dur <= timing.total,
            "response must not exceed total"
        );
        // The stale frame was still applied exactly once.
        let snapshot = UiSnapshot::from_state(&state).expect("state composes");
        assert!(snapshot.rendered_grid.contains("old!"));
        assert!(snapshot.rendered_grid.contains("abcd"));
        assert!(receipt.is_empty() && channel.is_empty() && deferred.is_empty());
    }

    #[test]
    fn request_to_flush_core_completes_split_across_receipt_and_channel() {
        // Response and flush both decode before Written(id): the response
        // rides the receipt, the flush rides the channel, and the window
        // still ends on the channel flush.
        let started = Instant::now();
        let mut receipt = VecDeque::new();
        enqueue_event(&mut receipt, 0, response_event(7, Ok(Object::Integer(1))));
        let mut channel = VecDeque::new();
        let flush_stamp = Instant::now();
        channel.push_back(TimedMessage {
            at: flush_stamp,
            seq: 1,
            event: SessionEvent::Redraw(complete_grid_batch("abcd", 0)),
        });
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        let (timing, response) = request_to_flush_core(
            &mut state,
            started,
            7,
            &mut receipt,
            &mut deferred,
            &mut queue_next(&mut channel),
        )
        .expect("the channel flush ends the timed window");
        assert_eq!(response, Some(Ok(Object::Integer(1))));
        assert_eq!(timing.total, flush_stamp.duration_since(started));
        let response_dur = timing.response.expect("the response stamp was observed");
        assert!(
            response_dur <= timing.total,
            "response must not exceed total"
        );
        assert!(receipt.is_empty() && channel.is_empty() && deferred.is_empty());
    }

    #[test]
    fn fence_completes_with_output_split_across_both_receipts() {
        // R's redraw lands in R's receipt, R's response in F's receipt, and
        // F's response on the live channel: the concatenated prelude drains
        // ahead of the channel in wire order and the fence completes.
        let mut resize_receipt = VecDeque::new();
        enqueue(&mut resize_receipt, 0, resize_batch(80, 24, "ready"));
        let mut fence_receipt = VecDeque::new();
        enqueue_event(
            &mut fence_receipt,
            1,
            response_event(RESIZE_ID, Ok(Object::Nil)),
        );
        let mut reads = resize_receipt;
        reads.extend(fence_receipt);
        let mut channel = VecDeque::new();
        enqueue_event(&mut channel, 2, response_event(FENCE_ID, Ok(Object::Nil)));
        let mut state = TuiState::new(None, MotionPolicy::Reduced);
        let mut deferred = VecDeque::new();
        {
            let mut next = || next_with_receipt(&mut reads, &mut queue_next(&mut channel));
            setup_fence_core(
                &mut state,
                RESIZE_ID,
                FENCE_ID,
                CONFIGURED,
                &mut deferred,
                &mut next,
            )
            .expect("split receipts cross the fence");
        }
        // The closure's borrow of `reads` and `channel` ended with the block.
        assert!(reads.is_empty(), "the prelude is fully drained");
        assert!(channel.is_empty(), "F's channel response is consumed");
        let snapshot = UiSnapshot::from_state(&state).expect("state composes");
        assert_eq!(snapshot.dimensions, (80, 24));
    }
}
