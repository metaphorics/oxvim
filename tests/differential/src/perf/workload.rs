//! Workload matrix, exact commands, and profile-dependent counts.
//!
//! The seven plan workload IDs (section 3 of the ticket plan) are:
//! - `startup` (W1, with plugin count in {0, 10, 50, 100})
//! - `input` (W2, `nvim_input "ix<Esc>x"`)
//! - `lua:pure` (W2a, `nvim_exec_lua` pure `LuaJIT`)
//! - `lua:api` (W2b, `nvim_exec_lua` calling `vim.api`)
//! - `open` (W3, large-buffer open to first flush)
//! - `edit` (W4, midpoint line replacement)
//! - `scroll` (W5, five-line scroll)
//!
//! Every steady-state shape is state-neutral (section 3.1): the editor's
//! observable state after sample k equals its state after sample 0. W4
//! alternates two equal-length replacement strings; W5 reverses the order of
//! paired five-line down/up movements while ending at the starting viewport.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use ox_types::Object;

use crate::perf::fixture::{self, FixtureError};
use crate::perf::session::{
    PerfError, PerfSession, Ready, SessionConfig, WorkloadCase as SessionWorkloadCase,
};

// ---------------------------------------------------------------------------
// WorkloadError
// ---------------------------------------------------------------------------

/// Workload matrix sizing failure.
#[derive(Debug)]
pub enum WorkloadError {
    /// The wall-cap multiplier `(warmup + samples + 4)` exceeded `u32` range.
    WallCapOverflow { factor: usize },
    /// Resolved fixture arguments do not belong to the workload cell.
    FixtureMismatch { workload: WorkloadId },
    /// An isolated session directory could not be created.
    IsolatedDir {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for WorkloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WallCapOverflow { factor } => {
                write!(f, "wall-cap multiplier {factor} overflows u32")
            }
            Self::FixtureMismatch { workload } => {
                write!(
                    f,
                    "resolved fixture arguments do not match workload {workload}"
                )
            }
            Self::IsolatedDir { path, source } => {
                write!(
                    f,
                    "cannot create isolated session directory {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for WorkloadError {}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

/// Run profile selecting process and sample counts.
///
/// `Full` is the baseline matrix; `Quick` is the mutation-proof and CI smoke
/// profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Full,
    Quick,
}

impl Profile {
    /// Plugin counts for W1 cells.
    #[must_use]
    pub const fn plugin_counts(self) -> &'static [usize] {
        match self {
            Self::Full => &[0, 10, 50, 100],
            Self::Quick => &[0, 50],
        }
    }

    /// Processes per engine for a workload kind.
    #[must_use]
    pub const fn processes_per_engine(self, kind: Kind) -> usize {
        match (self, kind) {
            (Self::Full, Kind::Startup) => 100,
            (Self::Full, Kind::SteadyState) => 8,
            (Self::Quick, Kind::Startup) => 12,
            (Self::Quick, Kind::SteadyState) => 4,
        }
    }

    /// Recorded samples per process (warmup already discarded).
    #[must_use]
    pub const fn samples_per_process(self, id: WorkloadId) -> usize {
        match id {
            WorkloadId::Startup { .. } | WorkloadId::Open => 1,
            _ => match self {
                Self::Full => 100,
                Self::Quick => 30,
            },
        }
    }

    /// Warmup windows per process, discarded before recording.
    #[must_use]
    pub const fn warmup_per_process(self, id: WorkloadId) -> usize {
        match id {
            WorkloadId::Startup { .. } | WorkloadId::Open => 0,
            _ => 5,
        }
    }

    /// Per-workload session timeout (section 3.6).
    #[must_use]
    pub fn timeout(self, cell: &WorkloadCell) -> Duration {
        match cell.id {
            WorkloadId::Startup { plugin_count } => {
                if plugin_count >= 50 {
                    Duration::from_secs(30)
                } else {
                    Duration::from_secs(15)
                }
            }
            WorkloadId::Input | WorkloadId::LuaPure | WorkloadId::LuaApi => Duration::from_secs(15),
            WorkloadId::Open | WorkloadId::Edit | WorkloadId::Scroll => Duration::from_secs(60),
        }
    }

    /// Whole-process wall cap: `timeout * (warmup + samples + 4)`.
    ///
    /// # Errors
    ///
    /// Returns [`WorkloadError::WallCapOverflow`] if the multiplier
    /// `(warmup + samples + 4)` does not fit in a `u32`. In practice the
    /// bound is `5 + 100 + 4 = 109`, well within `u32` range.
    pub fn wall_cap(self, cell: &WorkloadCell) -> Result<Duration, WorkloadError> {
        let warmup = self.warmup_per_process(cell.id);
        let samples = self.samples_per_process(cell.id);
        let factor_raw = warmup + samples + 4;
        let factor = u32::try_from(factor_raw)
            .map_err(|_| WorkloadError::WallCapOverflow { factor: factor_raw })?;
        Ok(self.timeout(cell) * factor)
    }
}

// ---------------------------------------------------------------------------
// WorkloadId
// ---------------------------------------------------------------------------

/// Stable workload identifier matching the report schema.
///
/// `Startup` carries its plugin count so the sample string can encode
/// `"startup:0"`, `"startup:50"`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadId {
    Startup { plugin_count: usize },
    Input,
    LuaPure,
    LuaApi,
    Open,
    Edit,
    Scroll,
}

impl WorkloadId {
    /// The seven workload type names (without plugin-count suffix).
    #[must_use]
    pub const fn type_name(self) -> &'static str {
        match self {
            Self::Startup { .. } => "startup",
            Self::Input => "input",
            Self::LuaPure => "lua:pure",
            Self::LuaApi => "lua:api",
            Self::Open => "open",
            Self::Edit => "edit",
            Self::Scroll => "scroll",
        }
    }

    /// Full cell identifier as it appears in `samples.ndjson`:
    /// `"startup:0"`, `"input"`, `"lua:pure"`, etc.
    #[must_use]
    pub fn cell_id(self) -> String {
        match self {
            Self::Startup { plugin_count } => format!("startup:{plugin_count}"),
            _ => self.type_name().to_owned(),
        }
    }

    /// Whether this is a startup (cold) or steady-state workload.
    #[must_use]
    pub const fn kind(self) -> Kind {
        match self {
            Self::Startup { .. } | Self::Open => Kind::Startup,
            Self::Input | Self::LuaPure | Self::LuaApi | Self::Edit | Self::Scroll => {
                Kind::SteadyState
            }
        }
    }

    /// The closed `WorkloadCase` used for spawn argument selection.
    #[must_use]
    pub const fn session_case(self) -> SessionWorkloadCase {
        match self {
            Self::Startup { plugin_count } => SessionWorkloadCase::PluginStartup {
                count: plugin_count,
            },
            Self::Input | Self::LuaPure | Self::LuaApi => SessionWorkloadCase::RpcInputToFlush,
            Self::Open => SessionWorkloadCase::LargeOpen,
            Self::Edit => SessionWorkloadCase::LargeEdit,
            Self::Scroll => SessionWorkloadCase::LargeScroll,
        }
    }
}

impl std::fmt::Display for WorkloadId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.cell_id())
    }
}

// ---------------------------------------------------------------------------
// Kind and EngineFamily
// ---------------------------------------------------------------------------

/// Whether a workload is a cold-startup or steady-state measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Startup,
    SteadyState,
}

/// Which engine family a cell runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineFamily {
    Neovim,
    Oxvim,
}

impl EngineFamily {
    /// Both engine families in ABBA-relevant order.
    #[must_use]
    pub const fn all() -> [Self; 2] {
        [Self::Neovim, Self::Oxvim]
    }

    /// Map to the session's `Engine`.
    #[must_use]
    pub const fn to_engine(self) -> crate::perf::session::Engine {
        match self {
            Self::Neovim => crate::perf::session::Engine::Neovim,
            Self::Oxvim => crate::perf::session::Engine::Oxvim,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkloadCell
// ---------------------------------------------------------------------------

/// Fixture material required before a workload cell can be spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixtureRequirement {
    None,
    LargeBuffer,
    PluginTree { count: usize },
}

/// Checked fixture arguments for one spawned session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureArguments {
    None,
    LargeBuffer { path: PathBuf },
    PluginTree { count: usize, path: PathBuf },
}

/// One cell of the matrix: a workload ID, an engine family, and fixture requirement.
#[derive(Debug, Clone)]
pub struct WorkloadCell {
    pub id: WorkloadId,
    pub family: EngineFamily,
    pub fixture: FixtureRequirement,
}

impl WorkloadCell {
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.id.kind()
    }

    /// Full cell identifier for `samples.ndjson`.
    #[must_use]
    pub fn cell_id(&self) -> String {
        self.id.cell_id()
    }

    /// The closed `WorkloadCase` for spawn argument selection.
    #[must_use]
    pub fn session_case(&self) -> SessionWorkloadCase {
        self.id.session_case()
    }

    /// Build the isolated session config for this cell from checked fixture arguments.
    ///
    /// # Errors
    ///
    /// Returns [`WorkloadError::FixtureMismatch`] if `arguments` were not
    /// resolved for this cell's exact fixture requirement.
    pub fn spawn_config(
        &self,
        root: &Path,
        profile: Profile,
        arguments: &FixtureArguments,
    ) -> Result<SessionConfig, WorkloadError> {
        let mut config = SessionConfig::isolated(root, root);
        config.timeout = profile.timeout(self);
        // Every spawn path funnels through here: materialize the isolated
        // environment up front, or the first instrumented child fails to
        // start — valgrind writes its client-cmdline file straight into
        // TMPDIR before the editor even execs.
        for dir in [
            &config.home,
            &config.xdg_config_home,
            &config.xdg_data_home,
            &config.xdg_state_home,
            &config.xdg_cache_home,
            &config.xdg_runtime_dir,
            &config.tmp_dir,
        ] {
            std::fs::create_dir_all(dir).map_err(|source| WorkloadError::IsolatedDir {
                path: dir.clone(),
                source,
            })?;
        }
        match (self.fixture, arguments) {
            (FixtureRequirement::None, FixtureArguments::None) => {}
            (FixtureRequirement::LargeBuffer, FixtureArguments::LargeBuffer { path }) => {
                config.extra_args.push(path.display().to_string());
            }
            (
                FixtureRequirement::PluginTree { count: required },
                FixtureArguments::PluginTree {
                    count: resolved,
                    path,
                },
            ) if required == *resolved => {
                config.extra_args.push("--cmd".to_owned());
                config
                    .extra_args
                    .push(format!("set rtp^={}", path.display()));
            }
            _ => {
                return Err(WorkloadError::FixtureMismatch { workload: self.id });
            }
        }
        Ok(config)
    }
}

// ---------------------------------------------------------------------------
// Matrix construction
// ---------------------------------------------------------------------------

/// Build the complete, declarative matrix for both engines.
#[must_use]
pub fn matrix(profile: Profile) -> Vec<WorkloadCell> {
    let mut cells = Vec::new();
    for family in EngineFamily::all() {
        for &count in profile.plugin_counts() {
            cells.push(WorkloadCell {
                id: WorkloadId::Startup {
                    plugin_count: count,
                },
                family,
                fixture: FixtureRequirement::PluginTree { count },
            });
        }
        cells.push(WorkloadCell {
            id: WorkloadId::Open,
            family,
            fixture: FixtureRequirement::LargeBuffer,
        });
        for id in [WorkloadId::Input, WorkloadId::LuaPure, WorkloadId::LuaApi] {
            cells.push(WorkloadCell {
                id,
                family,
                fixture: FixtureRequirement::None,
            });
        }
        for id in [WorkloadId::Edit, WorkloadId::Scroll] {
            cells.push(WorkloadCell {
                id,
                family,
                fixture: FixtureRequirement::LargeBuffer,
            });
        }
    }
    cells
}

/// Materialize a declarative fixture requirement under `fixture_root`.
///
/// # Errors
///
/// Propagates [`FixtureError`] without substituting missing fixture arguments.
pub fn resolve_fixture_arguments(
    requirement: FixtureRequirement,
    fixture_root: &Path,
) -> Result<FixtureArguments, FixtureError> {
    match requirement {
        FixtureRequirement::None => Ok(FixtureArguments::None),
        FixtureRequirement::LargeBuffer => {
            let generated = fixture::large_buffer(fixture_root)?;
            Ok(FixtureArguments::LargeBuffer {
                path: generated.path,
            })
        }
        FixtureRequirement::PluginTree { count } => {
            let generated = fixture::plugin_tree(fixture_root, count)?;
            Ok(FixtureArguments::PluginTree {
                count,
                path: generated.path,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Steady-state request construction and driving
// ---------------------------------------------------------------------------

/// One steady-state request: method name and positional parameters.
pub struct Request {
    pub method: &'static str,
    pub params: Vec<Object>,
}

/// One timed steady-state window.
pub struct TimedWindow {
    pub wall: Duration,
    pub response: Duration,
    pub flush: Duration,
}

/// Alternate W4's replacement text between samples (section 3.1).
///
/// Both strings are equal length so the rendered row width never changes.
#[must_use]
pub fn edit_alternation(sample_index: usize) -> &'static str {
    if sample_index.is_multiple_of(2) {
        "perf edit alpha"
    } else {
        "perf edit betaa"
    }
}

/// Alternate W5's state-neutral scroll order between samples (section 3.1).
#[must_use]
pub fn scroll_alternation(sample_index: usize) -> &'static str {
    if sample_index.is_multiple_of(2) {
        "<C-e><C-e><C-e><C-e><C-e><C-y><C-y><C-y><C-y><C-y>"
    } else {
        "<C-y><C-y><C-y><C-y><C-y><C-e><C-e><C-e><C-e><C-e>"
    }
}

/// Build the exact request for a steady-state workload at `sample_index`.
///
/// `sample_index` drives W4 and W5 alternation so every sample is a real
/// change and the grid is byte-identical at every even sample.
#[must_use]
pub fn request_params(id: WorkloadId, sample_index: usize) -> Request {
    match id {
        WorkloadId::Input => Request {
            method: "nvim_input",
            params: vec![Object::String("ix<Esc>x".into())],
        },
        WorkloadId::LuaPure => Request {
            method: "nvim_exec_lua",
            params: vec![
                Object::String("local s=0 for i=1,1000 do s=s+i end return s".into()),
                Object::Array(vec![]),
            ],
        },
        WorkloadId::LuaApi => Request {
            method: "nvim_exec_lua",
            params: vec![
                Object::String("return vim.api.nvim_buf_line_count(0)".into()),
                Object::Array(vec![]),
            ],
        },
        WorkloadId::Edit => Request {
            method: "nvim_buf_set_lines",
            params: vec![
                Object::Integer(0),
                Object::Integer(fixture::LARGE_MIDPOINT),
                Object::Integer(fixture::LARGE_MIDPOINT + 1),
                Object::Boolean(true),
                Object::Array(vec![Object::String(edit_alternation(sample_index).into())]),
            ],
        },
        WorkloadId::Scroll => Request {
            method: "nvim_input",
            params: vec![Object::String(scroll_alternation(sample_index).into())],
        },
        WorkloadId::Startup { .. } | WorkloadId::Open => Request {
            method: "",
            params: vec![],
        },
    }
}

/// Drive one steady-state window at `sample_index`.
///
/// Uses `request_to_flush_staged` for flush-terminated workloads (W2, W4, W5)
/// and `request_to_response_staged` for non-mutating Lua workloads (W2a,
/// W2b) where no flush endpoint arrives.
///
/// # Errors
///
/// Returns [`PerfError::Protocol`] for startup workloads (which cannot drive
/// steady-state windows), or propagates session errors.
pub fn run_steady_window(
    session: &mut PerfSession<Ready>,
    id: WorkloadId,
    sample_index: usize,
) -> Result<TimedWindow, PerfError> {
    match id {
        WorkloadId::Startup { .. } | WorkloadId::Open => Err(PerfError::Protocol(format!(
            "workload {id} is a startup workload and cannot drive steady-state windows"
        ))),
        WorkloadId::LuaPure | WorkloadId::LuaApi => {
            let request = request_params(id, sample_index);
            let (wall, result) =
                session.request_to_response_staged(request.method, request.params)?;
            validate_result(id, &result, None)?;
            Ok(TimedWindow {
                wall,
                response: wall,
                flush: Duration::ZERO,
            })
        }
        WorkloadId::Input | WorkloadId::Edit | WorkloadId::Scroll => {
            let request = request_params(id, sample_index);
            let submitted_bytes = match request.params.first() {
                Some(Object::String(submitted)) => Some(submitted.as_bytes().len()),
                _ => None,
            };
            let (staged, response_result) =
                session.request_to_flush_staged(request.method, request.params)?;
            let response = staged.response.ok_or_else(|| {
                PerfError::Protocol(format!(
                    "workload {id} completed a flush without its RPC response"
                ))
            })?;
            let result = response_result.ok_or_else(|| {
                PerfError::Protocol(format!(
                    "workload {id} completed a flush without an RPC result"
                ))
            })?;
            validate_result(id, &result, submitted_bytes)?;
            let flush = staged.total.checked_sub(response).ok_or_else(|| {
                PerfError::Protocol(format!(
                    "workload {id} response timestamp exceeds wall duration"
                ))
            })?;
            let accounted = response.checked_add(flush).ok_or_else(|| {
                PerfError::Protocol(format!(
                    "workload {id} response and flush durations overflow"
                ))
            })?;
            if accounted != staged.total {
                return Err(PerfError::Protocol(format!(
                    "workload {id} response and flush durations do not equal wall duration"
                )));
            }
            Ok(TimedWindow {
                wall: staged.total,
                response,
                flush,
            })
        }
    }
}

fn validate_result(
    id: WorkloadId,
    result: &Result<Object, String>,
    submitted_bytes: Option<usize>,
) -> Result<(), PerfError> {
    let value = result
        .as_ref()
        .map_err(|message| PerfError::Protocol(format!("workload {id} RPC error: {message}")))?;
    let valid = match id {
        WorkloadId::Input | WorkloadId::Scroll => {
            let expected = submitted_bytes.ok_or_else(|| {
                PerfError::Protocol(format!("workload {id} has no submitted-byte expectation"))
            })?;
            validate_submitted_count(id, value, expected)?
        }
        WorkloadId::LuaPure => matches!(value, Object::Integer(sum) if *sum == 500_500),
        WorkloadId::LuaApi => matches!(value, Object::Integer(count) if *count == 1),
        WorkloadId::Edit => matches!(value, Object::Nil),
        WorkloadId::Startup { .. } | WorkloadId::Open => false,
    };
    if valid {
        Ok(())
    } else {
        Err(PerfError::Protocol(format!(
            "workload {id} returned an unexpected RPC value: {value:?}"
        )))
    }
}

/// Validate an Input/Scroll response carrying the submitted byte count.
///
/// A count outside the `usize` range is a protocol violation, not a value
/// mismatch, so the conversion failure is reported as its own error instead
/// of being erased into the generic mismatch path.
///
/// # Errors
///
/// Returns [`PerfError::Protocol`] when the value is an integer that cannot
/// represent a byte count.
fn validate_submitted_count(
    id: WorkloadId,
    value: &Object,
    expected: usize,
) -> Result<bool, PerfError> {
    let Object::Integer(actual) = value else {
        return Ok(false);
    };
    let actual = usize::try_from(*actual).map_err(|error| {
        PerfError::Protocol(format!(
            "workload {id} returned integer {actual}, which is not a valid \
             byte count (expected {expected}): {error}"
        ))
    })?;
    Ok(actual == expected)
}

/// Steady-state driver tracking sample index for W4/W5 alternation.
#[derive(Debug)]
pub struct SteadyStateDriver<'a> {
    session: &'a mut PerfSession<Ready>,
    id: WorkloadId,
    sample_index: usize,
    completed: usize,
}

impl<'a> SteadyStateDriver<'a> {
    #[must_use]
    pub const fn new(session: &'a mut PerfSession<Ready>, id: WorkloadId) -> Self {
        Self {
            session,
            id,
            sample_index: 0,
            completed: 0,
        }
    }

    /// Drive one window with the correct alternation state for W4 and W5.
    ///
    /// # Errors
    ///
    /// Propagates [`PerfError`] from the underlying session call and rejects
    /// sample-accounting overflow before issuing another request.
    pub fn drive_window(&mut self) -> Result<TimedWindow, PerfError> {
        let next_sample = self
            .sample_index
            .checked_add(1)
            .ok_or_else(|| PerfError::Protocol("steady-state sample index overflow".to_owned()))?;
        let next_completed = self.completed.checked_add(1).ok_or_else(|| {
            PerfError::Protocol("steady-state completed-window count overflow".to_owned())
        })?;
        let window = run_steady_window(self.session, self.id, self.sample_index)?;
        self.sample_index = next_sample;
        self.completed = next_completed;
        Ok(window)
    }

    #[must_use]
    pub const fn completed(&self) -> usize {
        self.completed
    }
}

// ---------------------------------------------------------------------------
// ABBA ordering
// ---------------------------------------------------------------------------

/// ABBA block ordering: `[Neovim, Oxvim, Oxvim, Neovim]` (section 3.5).
///
/// Each block of four processes runs neovim, oxvim, oxvim, neovim. ABBA
/// cancels linear drift in host load exactly, since the two engines' mean
/// positions within the block are identical.
#[must_use]
pub const fn abba_block() -> [EngineFamily; 4] {
    [
        EngineFamily::Neovim,
        EngineFamily::Oxvim,
        EngineFamily::Oxvim,
        EngineFamily::Neovim,
    ]
}

/// Expand ABBA ordering for `process_count` processes per engine.
///
/// Returns a flat list of `EngineFamily` values in execution order. The
/// caller pairs each with a process index within its engine.
#[must_use]
pub fn abba_order(process_count: usize) -> Vec<EngineFamily> {
    let block = abba_block();
    let blocks = process_count.div_ceil(2);
    let mut order = Vec::with_capacity(blocks * 4);
    for _ in 0..blocks {
        order.extend(block);
    }
    order.truncate(process_count * 2);
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abba_order_is_balanced() {
        let order = abba_order(4);
        assert_eq!(order.len(), 8);
        let neovim_count = order.iter().filter(|&&f| f == EngineFamily::Neovim).count();
        let oxvim_count = order.iter().filter(|&&f| f == EngineFamily::Oxvim).count();
        assert_eq!(neovim_count, 4);
        assert_eq!(oxvim_count, 4);
        // First block: N, O, O, N
        assert_eq!(order[0], EngineFamily::Neovim);
        assert_eq!(order[1], EngineFamily::Oxvim);
        assert_eq!(order[2], EngineFamily::Oxvim);
        assert_eq!(order[3], EngineFamily::Neovim);
    }

    #[test]
    fn edit_alternation_is_equal_length() {
        assert_eq!(edit_alternation(0).len(), edit_alternation(1).len());
        assert_eq!(edit_alternation(0), "perf edit alpha");
        assert_eq!(edit_alternation(1), "perf edit betaa");
    }

    #[test]
    fn scroll_alternation_reverses_direction() {
        assert!(scroll_alternation(0).contains("<C-e>"));
        assert!(scroll_alternation(1).contains("<C-y>"));
    }

    #[test]
    fn cell_id_encodes_plugin_count() {
        assert_eq!(
            WorkloadId::Startup { plugin_count: 50 }.cell_id(),
            "startup:50"
        );
        assert_eq!(WorkloadId::Input.cell_id(), "input");
        assert_eq!(WorkloadId::LuaPure.cell_id(), "lua:pure");
    }

    #[test]
    fn session_case_maps_correctly() {
        assert_eq!(
            WorkloadId::Startup { plugin_count: 10 }.session_case(),
            SessionWorkloadCase::PluginStartup { count: 10 }
        );
        assert_eq!(
            WorkloadId::Input.session_case(),
            SessionWorkloadCase::RpcInputToFlush
        );
        assert_eq!(
            WorkloadId::Open.session_case(),
            SessionWorkloadCase::LargeOpen
        );
        assert_eq!(
            WorkloadId::Edit.session_case(),
            SessionWorkloadCase::LargeEdit
        );
        assert_eq!(
            WorkloadId::Scroll.session_case(),
            SessionWorkloadCase::LargeScroll
        );
    }
}
