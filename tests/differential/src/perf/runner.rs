//! ABBA process execution, profiles, dry-run, and artifact orchestration.
//!
//! The runner executes the workload matrix in ABBA-ordered blocks
//! (neovim, oxvim, oxvim, neovim), keeps every [`Sample`] paired with its
//! correctness [`UiSnapshot`], and writes typed sonic-rs artifacts under the
//! run directory when the collection completes. Verdict computation is
//! delegated to [`crate::perf::report`].
//!
//! # Artifacts
//!
//! Everything lands under `target/perf/<run_id>/`:
//!
//! | File | Contents |
//! |------|----------|
//! | `manifest.json` | typed run manifest: mode, contract, engine identities, measurement channels, calibration/profiler provenance |
//! | `samples.ndjson` | one [`Sample`] per line, in production order |
//! | `processes.ndjson` | one [`ProcessRun`] per line |
//! | `allocations.ndjson` | one [`AllocationRun`] per line (comparison only) |
//! | `noise.json` | typed [`NoiseRunSummary`] (calibration mode; comparison copies its validated calibration) |
//! | `summary.json` | [`RunSummary`] tagged by mode |
//! | `divergences.json` | typed mismatch records preserving both actual `UiSnapshot`s |
//! | `stages.json` | aggregated startup and steady-state stage artifact from [`crate::perf::report::summarize_stages`] (comparison) |
//! | `startuptime.json` | per-process parsed `--startuptime` logs or explicit not-collected (comparison) |
//! | `startuptime/`, `allocations/` | raw engine logs and DHAT outputs, referenced by hash |

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::perf::report::{
    AllocationRun, AlternationOrder, CellContract, ComparisonRunSummary, EngineLabel,
    FileIdentity as ReportFileIdentity, HostFingerprint, MeasurementContract, NoiseRunSummary,
    NoiseVerdict, ProcessRun, ReportError, RunSummary, Sample, StageContract, StagesArtifact,
    StartupStageMark, StartupStageRun, UiContract, judge_comparison, summarize_noise,
    summarize_stages,
};
use crate::perf::session::{
    Engine, PerfError, PerfSession, ProcessLauncher, Ready, SessionConfig, UiSnapshot, WorkloadCase,
};
use crate::perf::startuptime;
use crate::perf::{
    DhatError, allocation,
    fixture::{self, FixtureError},
};
use ox_types::{Object, OxStr};

use crate::perf::workload::{
    self, EngineFamily, FixtureArguments, FixtureRequirement, Kind, Profile, WorkloadCell,
    WorkloadError, WorkloadId, abba_order,
};

// ---------------------------------------------------------------------------
// Execution mode
// ---------------------------------------------------------------------------

/// Which execution mode the runner operates in.
#[derive(Debug, Clone)]
pub enum ExecutionMode {
    /// Print the expanded matrix and exit without spawning engines or creating
    /// fixtures. Produces a [`DryRunPlan`] rather than a [`RunSummary`].
    DryRun,
    /// Run the oracle-vs-oracle noise-calibration matrix using Neovim on both
    /// sides. Produces a [`crate::perf::report::NoiseRunSummary`] wrapped in
    /// [`RunSummary::NoiseCalibration`].
    NoiseCalibration,
    /// Run the full cross-engine comparison. Produces a [`RunSummary`].
    Comparison {
        /// Path to a noise-calibration JSON artifact produced by a prior
        /// [`ExecutionMode::NoiseCalibration`] run.
        noise_path: PathBuf,
        /// Path to the `valgrind` binary (or wrapper) used for memory
        /// instrumentation. If `None`, the run is rejected before any process
        /// spawns: the report demands exact DHAT allocation coverage.
        valgrind_path: Option<PathBuf>,
    },
}

// ---------------------------------------------------------------------------
// Run configuration and errors
// ---------------------------------------------------------------------------

/// Runner configuration built from CLI arguments.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Which execution mode to use.
    pub mode: ExecutionMode,
    /// Profile selecting process and sample counts.
    pub profile: Profile,
    /// Filter to specific workload type names; empty = all.
    pub workload_filter: Vec<String>,
    pub startuptime: bool,
    pub output_root: PathBuf,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::Comparison {
                noise_path: PathBuf::new(),
                valgrind_path: None,
            },
            profile: Profile::Full,
            workload_filter: Vec::new(),
            startuptime: false,
            output_root: Path::new("target/perf").to_path_buf(),
        }
    }
}

/// Typed runner failure.
#[derive(Debug)]
pub enum RunError {
    Fixture(FixtureError),
    Session(PerfError),
    Workload(WorkloadError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    SystemTime {
        context: &'static str,
        source: std::time::SystemTimeError,
    },
    Formatting {
        context: &'static str,
        source: fmt::Error,
    },
    Environment {
        variable: &'static str,
        source: env::VarError,
    },
    IntegerConversion {
        context: &'static str,
        source: std::num::TryFromIntError,
    },
    PrecisionLoss {
        context: &'static str,
        value: u64,
    },
    /// Harness built in debug; verdict refused (section 10.2).
    DebugHarness,
    /// A workload cell produced fewer samples than declared.
    Insufficient {
        workload: String,
        engine: String,
        got: usize,
        want: usize,
    },
    /// The `--startuptime` side pass failed.
    Startuptime {
        engine: String,
        detail: String,
    },
    InvalidWorkloadKind {
        workload: String,
        expected: &'static str,
    },
    /// CPU parallelism could not be determined.
    CpuCount {
        source: std::io::Error,
    },
    /// Hostname could not be determined.
    Hostname,
    /// A filter name is empty after trimming.
    EmptyFilter {
        index: usize,
    },
    /// A filter name appears more than once.
    DuplicateFilter {
        name: String,
    },
    /// A filter name does not match any known workload type.
    UnknownFilter {
        name: String,
    },
    /// The `(WorkloadId, EngineFamily)` pair appears more than once.
    DuplicateCell {
        workload: String,
        engine: String,
    },
    /// The profiler preflight (`--version`) failed or the path was not absolute.
    ProfilerPreflight {
        path: PathBuf,
        detail: String,
    },
    /// The profiler-wrapped process exited abnormally.
    ProfilerExit {
        workload: String,
        engine: String,
        source: PerfError,
    },
    /// The profiler output file could not be read.
    ProfilerRead {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The DHAT document could not be decoded or validated.
    ProfilerDecode {
        workload: String,
        engine: String,
        source: DhatError,
    },
    /// The noise-calibration artifact could not be decoded.
    NoiseDecode {
        path: PathBuf,
        source: sonic_rs::Error,
    },
    /// The noise calibration cannot back this comparison run.
    NoiseIncompatible {
        path: PathBuf,
        reason: String,
    },
    /// The `--startuptime` side pass failed to spawn, read, or parse a log.
    StartuptimeSide {
        engine: String,
        workload: String,
        detail: String,
    },
    /// A typed artifact could not be encoded as JSON.
    JsonEncode {
        /// Artifact path that failed to encode.
        path: PathBuf,
        /// sonic-rs serializer error.
        source: sonic_rs::Error,
    },
    /// A typed artifact could not be created or written.
    JsonWrite {
        /// Artifact path that failed to write.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A primary failure was propagated, and the session shutdown that
    /// followed it also failed; neither failure masks the other.
    Lifecycle {
        /// The original measurement failure.
        primary: Box<RunError>,
        /// The follow-up shutdown failure.
        shutdown: Box<RunError>,
    },
    /// Report construction or validation failed.
    Report(ReportError),
    /// matrix after filtering.
    MissingCell {
        workload: String,
        engine: String,
    },
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixture(e) => write!(f, "fixture error: {e}"),
            Self::Session(e) => write!(f, "session error: {e}"),
            Self::Workload(e) => write!(f, "workload error: {e}"),
            Self::Io { path, source } => write_io_context(f, "I/O at", path, source),
            Self::SystemTime { context, source } => {
                write!(f, "system time for {context}: {source}")
            }
            Self::Formatting { context, source } => write!(f, "formatting {context}: {source}"),
            Self::Environment { variable, source } => {
                write!(f, "environment variable {variable}: {source}")
            }
            Self::IntegerConversion { context, source } => {
                write!(f, "integer conversion for {context}: {source}")
            }
            Self::PrecisionLoss { context, value } => {
                write!(f, "cannot represent {value} exactly as f64 for {context}")
            }
            Self::DebugHarness => {
                write!(f, "harness built in debug; verdict refused (section 10.2)")
            }
            Self::Insufficient {
                workload,
                engine,
                got,
                want,
            } => {
                write!(
                    f,
                    "insufficient samples for {workload}/{engine}: got {got}, want {want}"
                )
            }
            Self::Startuptime { engine, detail } => {
                write!(f, "startuptime side pass failed for {engine}: {detail}")
            }
            Self::InvalidWorkloadKind { workload, expected } => {
                write!(f, "workload {workload} is not a {expected} workload")
            }
            Self::CpuCount { source } => write!(f, "cannot determine CPU parallelism: {source}"),
            Self::Hostname => write!(f, "cannot determine hostname"),
            Self::EmptyFilter { index } => write!(f, "filter at index {index} is empty"),
            Self::DuplicateFilter { name } => write!(f, "filter \"{name}\" appears more than once"),
            Self::UnknownFilter { name } => {
                write!(
                    f,
                    "filter \"{name}\" does not match any known workload type"
                )
            }
            Self::DuplicateCell { workload, engine } => {
                write!(
                    f,
                    "duplicate cell for {workload}/{engine} in prepared matrix"
                )
            }
            Self::MissingCell { workload, engine } => {
                write!(f, "missing cell for {workload}/{engine} in prepared matrix")
            }
            Self::Report(source) => write!(f, "report error: {source}"),
            Self::ProfilerPreflight { .. }
            | Self::ProfilerExit { .. }
            | Self::ProfilerRead { .. }
            | Self::ProfilerDecode { .. } => self.fmt_profiler(f),
            Self::NoiseDecode { path, source } => {
                write_io_context(f, "cannot decode noise calibration at", path, source)
            }
            Self::NoiseIncompatible { path, reason } => write!(
                f,
                "noise calibration at {} cannot back this run: {reason}",
                path.display()
            ),
            Self::StartuptimeSide {
                engine,
                workload,
                detail,
            } => write!(
                f,
                "startuptime side pass failed for {engine}/{workload}: {detail}"
            ),
            Self::JsonEncode { path, source } => {
                write_io_context(f, "cannot encode JSON artifact at", path, source)
            }
            Self::JsonWrite { path, source } => {
                write_io_context(f, "cannot write JSON artifact at", path, source)
            }
            Self::Lifecycle { primary, shutdown } => {
                write!(
                    f,
                    "primary error: {primary}; shutdown also failed: {shutdown}"
                )
            }
        }
    }
}

/// Write a path-prefixed I/O failure: `<prefix> <path>: <source>`.
///
/// Every path-bearing [`RunError`] variant renders its location through
/// this one shape, so artifact and profiler paths always format alike.
fn write_io_context(
    f: &mut fmt::Formatter<'_>,
    prefix: &str,
    path: &Path,
    source: &(dyn std::error::Error + 'static),
) -> fmt::Result {
    write!(f, "{prefix} {}: {source}", path.display())
}

/// Format the profiler-related failure variants.
///
/// Called only from the [`fmt::Display`] match for the grouped profiler
/// arms; every rendered string is byte-identical to the former inline arms.
impl RunError {
    fn fmt_profiler(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProfilerPreflight { path, detail } => write!(
                f,
                "profiler preflight failed for {}: {detail}",
                path.display()
            ),
            Self::ProfilerExit {
                workload,
                engine,
                source,
            } => write!(
                f,
                "profiler process exited abnormally for {workload}/{engine}: {source}"
            ),
            Self::ProfilerRead { path, source } => {
                write_io_context(f, "cannot read profiler output at", path, source)
            }
            Self::ProfilerDecode {
                workload,
                engine,
                source,
            } => {
                write!(f, "cannot decode DHAT for {workload}/{engine}: {source}")
            }
            _ => unreachable!("fmt_profiler reserved for profiler variants"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fixture(source) => Some(source),
            Self::Session(source) | Self::ProfilerExit { source, .. } => Some(source),
            Self::Workload(source) => Some(source),
            Self::Io { source, .. }
            | Self::CpuCount { source }
            | Self::ProfilerRead { source, .. }
            | Self::JsonWrite { source, .. } => Some(source),
            Self::SystemTime { source, .. } => Some(source),
            Self::Formatting { source, .. } => Some(source),
            Self::Environment { source, .. } => Some(source),
            Self::IntegerConversion { source, .. } => Some(source),
            Self::Report(source) => Some(source),
            Self::ProfilerDecode { source, .. } => Some(source),
            Self::NoiseDecode { source, .. } | Self::JsonEncode { source, .. } => Some(source),
            Self::Lifecycle { primary, .. } => Some(&**primary),
            Self::PrecisionLoss { .. }
            | Self::DebugHarness
            | Self::Insufficient { .. }
            | Self::Startuptime { .. }
            | Self::InvalidWorkloadKind { .. }
            | Self::Hostname
            | Self::EmptyFilter { .. }
            | Self::DuplicateFilter { .. }
            | Self::UnknownFilter { .. }
            | Self::DuplicateCell { .. }
            | Self::MissingCell { .. }
            | Self::ProfilerPreflight { .. }
            | Self::NoiseIncompatible { .. }
            | Self::StartuptimeSide { .. } => None,
        }
    }
}

impl From<FixtureError> for RunError {
    fn from(e: FixtureError) -> Self {
        Self::Fixture(e)
    }
}
impl From<PerfError> for RunError {
    fn from(e: PerfError) -> Self {
        Self::Session(e)
    }
}
impl From<WorkloadError> for RunError {
    fn from(e: WorkloadError) -> Self {
        Self::Workload(e)
    }
}
impl From<ReportError> for RunError {
    fn from(e: ReportError) -> Self {
        Self::Report(e)
    }
}

// ---------------------------------------------------------------------------
// Run outcome
// ---------------------------------------------------------------------------

/// The result of a runner invocation.
#[derive(Debug)]
pub enum RunOutcome {
    /// A dry-run was requested; the deterministic plan is returned instead of
    /// any measured data. No [`RunSummary`] or verdict is produced.
    DryRun(DryRunPlan),
    /// A completed measurement run.
    Completed(Box<RunSummary>),
}

// ---------------------------------------------------------------------------
// Dry-run plan
// ---------------------------------------------------------------------------

/// One cell's deterministic counts and fixture requirement as printed by a
/// dry-run.
#[derive(Debug, Clone)]
pub struct DryRunCell {
    /// The workload identifier.
    pub id: WorkloadId,
    /// Which engine family this cell targets.
    pub family: EngineFamily,
    /// What fixture must be materialised before this cell can run.
    pub fixture: FixtureRequirement,
    /// Number of processes to run per engine for this cell's kind.
    pub processes: usize,
    /// Warmup windows discarded before recording for this cell.
    pub warmup: usize,
    /// Recorded samples collected per process for this cell.
    pub samples: usize,
    /// Session timeout for each process of this cell.
    pub timeout: Duration,
}

/// Deterministic preview of a run without spawning engines or creating fixtures.
///
/// Built entirely from the profile and CLI filter arguments; contains no
/// measured data, no fixture files, and no performance verdict.
#[derive(Debug, Clone)]
pub struct DryRunPlan {
    /// The selected run profile.
    pub profile: Profile,
    /// Validated filter names (empty = all workloads).
    pub filters: Vec<String>,
    /// One entry per selected matrix cell.
    pub cells: Vec<DryRunCell>,
}

// ---------------------------------------------------------------------------
// Prepared matrix
// ---------------------------------------------------------------------------

/// The fully resolved measurement matrix: selected cells, resolved fixture
/// arguments, file hashes, and an indexed `(WorkloadId, EngineFamily)` pair list.
///
/// Produced by [`Runner::prepare_matrix`]; consumed by
/// [`Runner::run_noise_calibration`] and [`Runner::run_comparison`].
struct PreparedMatrix {
    /// All selected matrix cells (one per `(WorkloadId, EngineFamily)` pair).
    cells: Vec<WorkloadCell>,
    /// Resolved fixture material for each unique requirement.
    fixtures: HashMap<FixtureRequirement, PreparedFixture>,
    /// Exact `(WorkloadId, EngineFamily)` pairs in execution order.
    pairs: Vec<(WorkloadId, EngineFamily)>,
}

/// Resolved fixture material: checked arguments and the fixture file's SHA-256
/// digest.
struct PreparedFixture {
    arguments: FixtureArguments,
    sha256: Option<String>,
}

impl PreparedMatrix {
    /// Select each workload's Neovim cell and prepared fixture, in matrix
    /// execution order: the reference arm every contract row describes.
    fn neovim_cells(&self) -> Result<Vec<(&WorkloadCell, &PreparedFixture)>, RunError> {
        self.pairs
            .iter()
            .filter(|(_, family)| *family == EngineFamily::Neovim)
            .map(|(id, _)| id)
            .map(|id| {
                let cell = self.neovim_head_cell(id)?;
                let fixture = self.fixture_of(cell, "neovim")?;
                Ok((cell, fixture))
            })
            .collect()
    }

    /// The Neovim reference cell of one workload; it carries the workload's
    /// process count for every ABBA schedule.
    fn neovim_head_cell(&self, workload_id: &WorkloadId) -> Result<&WorkloadCell, RunError> {
        self.cells
            .iter()
            .find(|cell| cell.id == *workload_id && cell.family == EngineFamily::Neovim)
            .ok_or_else(|| RunError::MissingCell {
                workload: workload_id.cell_id(),
                engine: "neovim".to_owned(),
            })
    }

    /// Resolve the cell and prepared fixture for one ABBA slot.
    fn abba_cell(
        &self,
        workload_id: &WorkloadId,
        family: EngineFamily,
    ) -> Result<(&WorkloadCell, &PreparedFixture), RunError> {
        let engine = engine_name(family);
        let cell = self
            .cells
            .iter()
            .find(|cell| cell.id == *workload_id && cell.family == family)
            .ok_or_else(|| RunError::MissingCell {
                workload: workload_id.cell_id(),
                engine: engine.clone(),
            })?;
        let fixture = self.fixture_of(cell, &engine)?;
        Ok((cell, fixture))
    }

    /// The prepared fixture of one cell; a cell whose requirement was never
    /// resolved means the matrix is incomplete.
    fn fixture_of(&self, cell: &WorkloadCell, engine: &str) -> Result<&PreparedFixture, RunError> {
        self.fixtures
            .get(&cell.fixture)
            .ok_or_else(|| RunError::MissingCell {
                workload: cell.cell_id(),
                engine: engine.to_owned(),
            })
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// The performance harness runner.
pub struct Runner {
    config: RunConfig,
    /// The run identifier, populated only for non-dry-run modes.
    run_id: Option<String>,
    /// The run directory, populated only for non-dry-run modes.
    run_dir: Option<PathBuf>,
    fixture_root: PathBuf,
}

/// One collected sample with its snapshot for divergence detection.
struct CollectedSample {
    sample: Sample,
    snapshot: Option<UiSnapshot>,
}

/// One process's complete output.
struct ProcessOutput {
    process: ProcessRun,
    samples: Vec<CollectedSample>,
}

struct ProcessMeasurements {
    samples: Vec<CollectedSample>,
    peak_rss_kib: u64,
    pid: u32,
}
/// Immutable descriptor of one scheduled process execution: which cell and
/// fixture run, under which engine label and process slot, launched how,
/// identified by the enclosing run.
struct ProcessInvocation<'a> {
    /// The workload cell being measured.
    cell: &'a WorkloadCell,
    /// The resolved fixture material for `cell`.
    fixture: &'a PreparedFixture,
    /// Owning run identity.
    run_id: &'a str,
    /// Report label for this engine arm.
    engine_label: EngineLabel,
    /// Process slot within the workload's ABBA schedule.
    process_index: usize,
    /// How the engine process is launched.
    launcher: ProcessLauncher,
}

/// One resolved process execution: the spawn configuration and reporting
/// identity of a single engine session, immutable once spawned.
struct ProcessExecution<'a> {
    /// The workload cell being measured.
    cell: &'a WorkloadCell,
    /// Engine binary to spawn.
    engine: Engine,
    /// Closed workload case for spawn argument selection.
    case: WorkloadCase,
    /// Fully resolved session configuration.
    config: SessionConfig,
    /// Active measurement profile.
    profile: Profile,
    /// Owning run identity.
    run_id: &'a str,
    /// Report label for this engine arm.
    engine_label: EngineLabel,
    /// Process slot within the workload's ABBA schedule.
    process_index: usize,
}

impl<'a> ProcessExecution<'a> {
    /// Resolve one scheduled invocation into a spawn-ready execution.
    fn resolve(
        invocation: ProcessInvocation<'a>,
        profile: Profile,
        fixture_root: &Path,
    ) -> Result<Self, RunError> {
        let ProcessInvocation {
            cell,
            fixture,
            run_id,
            engine_label,
            process_index,
            launcher,
        } = invocation;
        let engine = cell.family.to_engine();
        let case = cell.session_case();
        let mut config = cell.spawn_config(fixture_root, profile, &fixture.arguments)?;
        config.launcher = launcher;
        Ok(Self {
            cell,
            engine,
            case,
            config,
            profile,
            run_id,
            engine_label,
            process_index,
        })
    }
}

/// The direct (uninstrumented) pass output: every timed process record and
/// its snapshot-paired samples, in ABBA walk order.
#[derive(Default)]
struct DirectCollection {
    /// One [`ProcessRun`] per scheduled slot, in walk order.
    processes: Vec<ProcessRun>,
    /// Every collected sample, in walk order.
    samples: Vec<CollectedSample>,
}

/// One DHAT-instrumented execution slot: the scheduling invocation plus the
/// output locations of its raw profiler artifacts.
struct InstrumentedRun<'a> {
    /// The scheduled invocation; its launcher wraps the process in valgrind.
    invocation: ProcessInvocation<'a>,
    /// Lowercased engine name for stems and error contexts.
    engine: String,
    /// DHAT output document for the process.
    dhat_path: PathBuf,
    /// Valgrind log for the process.
    valgrind_log_path: PathBuf,
}

/// One `--startuptime` log collection slot: the resolved spawn inputs and
/// the raw log's output location for one engine/workload/process triple.
struct StartupLogRun<'a> {
    /// The startup-kind cell being measured.
    cell: &'a WorkloadCell,
    /// Engine binary to spawn.
    engine: Engine,
    /// Report label for the engine.
    engine_label: EngineLabel,
    /// Process slot within the workload.
    process_index: usize,
    /// Fully resolved session configuration (env isolation, fixture args).
    config: SessionConfig,
    /// Directory receiving the raw log.
    startup_dir: &'a Path,
    /// File name of the raw log.
    log_name: String,
}

/// One startup-time log record preserved with its raw log path.
struct StartuptimeRecord {
    engine: EngineLabel,
    workload: String,
    process_index: usize,
    log: startuptime::StartupLog,
    raw_log_path: PathBuf,
}

/// The outcome of the `--startuptime` side pass.
enum StartuptimeSidePass {
    /// Collected records, one per engine/workload/process.
    Collected(Vec<StartuptimeRecord>),
    /// The side pass was disabled in [`RunConfig`]; no logs were collected.
    NotCollected,
}
/// The noise-calibration artifact's on-disk identity, preserved for the
/// manifest writer.
#[derive(Debug, Clone, Serialize)]
struct CalibrationSource {
    /// Path the calibration was loaded from.
    path: PathBuf,
    /// SHA-256 of the exact source bytes on disk.
    sha256: String,
    /// The calibration's own run identity.
    run_id: String,
}

// ---------------------------------------------------------------------------
// Typed on-disk artifacts (sonic-rs)
// ---------------------------------------------------------------------------

/// Manifest schema version.
const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// Divergences-artifact schema version.
const DIVERGENCES_SCHEMA_VERSION: u32 = 1;
/// Startuptime-artifact schema version.
const STARTUPTIME_SCHEMA_VERSION: u32 = 1;

/// Which completed mode a run manifest was written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ManifestMode {
    NoiseCalibration,
    Comparison,
}

/// The engine binaries that every timed process runs directly.
#[derive(Debug, Clone, Serialize)]
struct ManifestEngines {
    /// Neovim oracle binary; both calibration arms and the comparison
    /// reference side run this binary without any profiler wrapper.
    oracle: ReportFileIdentity,
    /// Oxvim candidate binary; runs directly in the comparison latency/RSS
    /// passes.
    candidate: ReportFileIdentity,
}

/// Which channel produced one measurement family.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
enum MeasurementChannel {
    /// A direct, uninstrumented session; no profiler wrapper.
    Direct,
    /// A Valgrind DHAT-instrumented process; its timing and RSS are
    /// discarded and only allocation totals are kept.
    Dhat { profiler: ProfilerProvenance },
}

/// The typed statement that latency and peak RSS are direct measurements
/// while allocation totals (comparison runs) come from DHAT.
#[derive(Debug, Clone, Serialize)]
struct ManifestChannels {
    /// Latency windows come from direct sessions.
    latency: MeasurementChannel,
    /// Peak RSS comes from the same direct sessions (`VmHWM`).
    peak_rss: MeasurementChannel,
    /// Allocation totals; `None` when no DHAT pass ran (noise calibration).
    allocations: Option<MeasurementChannel>,
}

/// The resolved profiler identity recorded in the manifest.
#[derive(Debug, Clone, Serialize)]
struct ProfilerProvenance {
    /// Absolute valgrind path that passed the `--version` preflight.
    path: PathBuf,
    /// Trimmed `valgrind --version` output.
    version: String,
    /// Fixed profiler options applied to every instrumented process.
    options: Vec<String>,
    /// Where per-process outputs land, relative to the run directory.
    output_pattern: String,
}

/// The typed manifest written for every completed run.
#[derive(Debug, Clone, Serialize)]
struct RunManifest {
    /// Manifest schema version.
    schema_version: u32,
    /// This run's identity; shared by every artifact of the run.
    run_id: String,
    /// Completed mode.
    mode: ManifestMode,
    /// Unix seconds when collection started.
    started_unix: u64,
    /// The exact measurement contract (host, binaries, UI, cells, order).
    contract: MeasurementContract,
    /// Directly timed engine identities.
    engines: ManifestEngines,
    /// Which channel produced each measurement family.
    channels: ManifestChannels,
    /// The loaded calibration backing a comparison run; `None` for a
    /// calibration run, which produces its own calibration and has no
    /// candidate or profiler source.
    calibration: Option<CalibrationSource>,
    /// The resolved DHAT profiler; `None` when no allocation pass ran.
    valgrind: Option<ProfilerProvenance>,
}

/// A serializable copy of one correctness [`UiSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SnapshotRecord {
    /// Composed grid width.
    width: usize,
    /// Composed grid height.
    height: usize,
    /// Byte-exact rendered grid text (rows joined with `\n`).
    rendered_grid: String,
    /// Cursor in composed coordinates, if visible.
    composed_cursor: Option<(usize, usize)>,
    /// Active mode name, if announced.
    mode_name: Option<String>,
    /// Cursor window viewport, if announced.
    viewport: Option<ViewportRecord>,
}

/// Serializable copy of one [`crate::perf::session::NormalizedViewport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ViewportRecord {
    top_line: i64,
    bottom_line: i64,
    cursor_line: i64,
    cursor_column: i64,
}

impl SnapshotRecord {
    /// Copy the full observable content of a snapshot.
    fn capture(snapshot: &UiSnapshot) -> Self {
        Self {
            width: snapshot.dimensions.0,
            height: snapshot.dimensions.1,
            rendered_grid: snapshot.rendered_grid.clone(),
            composed_cursor: snapshot.composed_cursor,
            mode_name: snapshot.mode_name.clone(),
            viewport: snapshot.viewport.as_ref().map(|viewport| ViewportRecord {
                top_line: viewport.top_line,
                bottom_line: viewport.bottom_line,
                cursor_line: viewport.cursor_line,
                cursor_column: viewport.cursor_column,
            }),
        }
    }
}

/// One exact left/right sample pair whose grid hashes disagree.
///
/// Both actual snapshots are preserved (as explicit `null` when a side
/// legitimately carries none, e.g. the no-UI startup workload) so every
/// mismatch can be diagnosed offline without re-running the engines.
#[derive(Debug, Clone, Serialize)]
struct DivergenceRecord {
    /// Owning run identity.
    run_id: String,
    /// Workload cell the pair belongs to.
    workload: String,
    /// Process index within the workload for both sides.
    process_index: usize,
    /// Sample index within the process for both sides.
    sample_index: usize,
    /// Left-side engine label.
    left_engine: EngineLabel,
    /// Right-side engine label.
    right_engine: EngineLabel,
    /// Left-side correctness hash (as judged by the report).
    left_grid_sha256: String,
    /// Right-side correctness hash (as judged by the report).
    right_grid_sha256: String,
    /// Left-side actual snapshot, if one was captured.
    left_snapshot: Option<SnapshotRecord>,
    /// Right-side actual snapshot, if one was captured.
    right_snapshot: Option<SnapshotRecord>,
}

/// The typed `divergences.json` artifact.
#[derive(Debug, Clone, Serialize)]
struct DivergencesArtifact {
    /// Artifact schema version.
    schema_version: u32,
    /// Owning run identity.
    run_id: String,
    /// Left engine label of every pair.
    left_engine: EngineLabel,
    /// Right engine label of every pair.
    right_engine: EngineLabel,
    /// Number of exactly paired samples compared.
    compared: usize,
    /// Number of pairs whose grid hashes disagree.
    mismatched: usize,
    /// Walk ordinal of the first mismatch, if any.
    first_mismatch_ordinal: Option<usize>,
    /// Full mismatch records in pairing-walk order.
    records: Vec<DivergenceRecord>,
}

/// Serializable per-process `--startuptime` evidence.
#[derive(Debug, Clone, Serialize)]
struct StartuptimeRecordArtifact {
    /// Owning run identity.
    run_id: String,
    /// Engine the log belongs to.
    engine: EngineLabel,
    /// Workload cell the log belongs to.
    workload: String,
    /// Process index within the workload.
    process_index: usize,
    /// Path of the verbatim engine log preserved under `startuptime/`.
    raw_log_path: String,
    /// Parsed log (strict integer-microsecond marks).
    log: startuptime::StartupLog,
}

/// The `--startuptime` side-pass outcome.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum StartuptimeState {
    /// The side pass ran; one record per engine/workload/process follows.
    Collected {
        /// Parsed logs with their raw-file provenance.
        records: Vec<StartuptimeRecordArtifact>,
    },
    /// The side pass was disabled (or is not part of the mode); nothing was
    /// collected. Serialized explicitly so absence is never silent.
    NotCollected,
}

/// The typed `startuptime.json` artifact.
#[derive(Debug, Clone, Serialize)]
struct StartuptimeArtifact {
    /// Artifact schema version.
    schema_version: u32,
    /// Owning run identity.
    run_id: String,
    /// Side-pass outcome.
    #[serde(rename = "startuptime")]
    state: StartuptimeState,
}
/// The resolved external inputs of a comparison run, borrowed from the
/// runner's configuration: the backing calibration artifact and the profiler
/// binary.
struct ComparisonInputs<'a> {
    /// Noise-calibration JSON backing this run.
    noise_path: &'a Path,
    /// Absolute valgrind binary for the allocation pass.
    valgrind_path: &'a Path,
}

/// The complete typed artifact set of a comparison run, written in
/// dependency order with the summary last.
struct ComparisonArtifacts<'a> {
    /// The run manifest.
    manifest: &'a RunManifest,
    /// Typed report samples.
    samples: &'a [Sample],
    /// Direct-pass process records.
    processes: &'a [ProcessRun],
    /// Allocation-pass records.
    allocations: &'a [AllocationRun],
    /// Backing calibration.
    calibration: &'a NoiseRunSummary,
    /// Divergence pairing artifact.
    divergences: &'a DivergencesArtifact,
    /// Stage summary artifact.
    stages: &'a StagesArtifact,
    /// Startup side-pass artifact.
    startuptime: &'a StartuptimeArtifact,
    /// The judged summary, written last.
    summary: &'a RunSummary,
}

impl ComparisonArtifacts<'_> {
    /// Write every artifact under `run_dir`; the summary lands only after
    /// every supporting artifact is on disk.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when any artifact cannot be encoded or written.
    fn write_all(&self, run_dir: &Path) -> Result<(), RunError> {
        write_artifact_json(&run_dir.join("manifest.json"), self.manifest)?;
        write_artifact_ndjson(&run_dir.join("samples.ndjson"), self.samples)?;
        write_artifact_ndjson(&run_dir.join("processes.ndjson"), self.processes)?;
        write_artifact_ndjson(&run_dir.join("allocations.ndjson"), self.allocations)?;
        write_artifact_json(&run_dir.join("noise.json"), self.calibration)?;
        write_artifact_json(&run_dir.join("divergences.json"), self.divergences)?;
        write_artifact_json(&run_dir.join("stages.json"), self.stages)?;
        write_artifact_json(&run_dir.join("startuptime.json"), self.startuptime)?;
        // The summary is the final claim: it lands only after every
        // supporting artifact is on disk.
        write_artifact_json(&run_dir.join("summary.json"), self.summary)?;
        Ok(())
    }
}

impl StartuptimeSidePass {
    /// Assemble the typed `startuptime.json` artifact from the side-pass
    /// outcome; a disabled pass serializes explicit not-collected state.
    fn into_artifact(self, run_id: &str) -> StartuptimeArtifact {
        let state = match self {
            StartuptimeSidePass::Collected(records) => StartuptimeState::Collected {
                records: records
                    .into_iter()
                    .map(|record| StartuptimeRecordArtifact {
                        run_id: run_id.to_owned(),
                        engine: record.engine,
                        workload: record.workload,
                        process_index: record.process_index,
                        raw_log_path: record.raw_log_path.display().to_string(),
                        log: record.log,
                    })
                    .collect(),
            },
            StartuptimeSidePass::NotCollected => StartuptimeState::NotCollected,
        };
        StartuptimeArtifact {
            schema_version: STARTUPTIME_SCHEMA_VERSION,
            run_id: run_id.to_owned(),
            state,
        }
    }
}

impl StartuptimeArtifact {
    /// One report stage run per collected startup log, in collection order.
    fn stage_runs(&self) -> Vec<StartupStageRun> {
        match &self.state {
            StartuptimeState::Collected { records } => records
                .iter()
                .map(|record| StartupStageRun {
                    run_id: record.run_id.clone(),
                    workload: record.workload.clone(),
                    engine: record.engine,
                    process_index: record.process_index,
                    raw_source: record.raw_log_path.clone(),
                    marks: record
                        .log
                        .marks
                        .iter()
                        .enumerate()
                        .map(|(ordinal, mark)| StartupStageMark {
                            ordinal,
                            label: mark.label.clone(),
                            delta_us: mark.delta_us,
                            clock_us: mark.clock_us,
                        })
                        .collect(),
                })
                .collect(),
            StartuptimeState::NotCollected => Vec::new(),
        }
    }
}

impl Runner {
    /// Construct a runner with the given configuration.
    ///
    /// Validation covers only the mode, profile, and filter arguments.
    /// No run identifier or run directory is created here.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] if the mode is invalid or if the system clock is
    /// before the Unix epoch.
    pub fn new(config: RunConfig) -> Result<Self, RunError> {
        // Validate profile — only Full/Quick are supported.
        match config.profile {
            Profile::Full | Profile::Quick => {}
        }

        // Validate filter names before any I/O.
        let _ = Self::validate_filters(&config.workload_filter)?;

        // Sessions run with the fixture root as their cwd, so every path
        // derived from the output root (session TMPDIR, DHAT outputs, raw
        // logs) must be absolute: a relative TMPDIR resolves against the
        // child's cwd and breaks instrumented spawns. `absolute` needs no
        // filesystem access, unlike `canonicalize`.
        let mut config = config;
        config.output_root =
            std::path::absolute(&config.output_root).map_err(|source| RunError::Io {
                path: config.output_root.clone(),
                source,
            })?;

        let fixture_root = config.output_root.join("fixtures");

        // Generate run ID and directory only for completed modes.
        let (run_id, run_dir) = match config.mode {
            ExecutionMode::DryRun => (None, None),
            ExecutionMode::NoiseCalibration | ExecutionMode::Comparison { .. } => {
                let id = generate_run_id()?;
                let dir = config.output_root.join(&id);
                (Some(id), Some(dir))
            }
        };

        Ok(Self {
            config,
            run_id,
            run_dir,
            fixture_root,
        })
    }

    /// Execute the configured run and return its outcome.
    ///
    /// For [`ExecutionMode::DryRun`] this returns a [`DryRunPlan`] without
    /// spawning any processes or creating fixtures. For completed modes this
    /// returns a [`RunSummary`] produced by the respective calibration or
    /// comparison routine.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] for fixture, session, I/O, or matrix failures.
    pub fn run(&self) -> Result<RunOutcome, RunError> {
        match &self.config.mode {
            ExecutionMode::DryRun => {
                let plan = self.build_dry_run_plan()?;
                Ok(RunOutcome::DryRun(plan))
            }
            ExecutionMode::NoiseCalibration => {
                let matrix = self.prepare_matrix()?;
                let summary = self.run_noise_calibration(&matrix)?;
                Ok(RunOutcome::Completed(Box::new(summary)))
            }
            ExecutionMode::Comparison { .. } => {
                let matrix = self.prepare_matrix()?;
                let summary = self.run_comparison(&matrix)?;
                Ok(RunOutcome::Completed(Box::new(summary)))
            }
        }
    }

    /// The run identifier, if this runner was constructed for a completed mode.
    #[must_use]
    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    /// The run directory path, if this runner was constructed for a completed
    /// mode.
    #[must_use]
    pub fn run_dir(&self) -> Option<&Path> {
        self.run_dir.as_deref()
    }

    // ------------------------------------------------------------------
    // Filter validation
    // ------------------------------------------------------------------

    /// Validates a list of raw filter strings.
    ///
    /// Each entry is trimmed. Empty entries, duplicates, and unknown workload
    /// type names are rejected.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::EmptyFilter`], [`RunError::DuplicateFilter`], or
    /// [`RunError::UnknownFilter`] on the first violation.
    fn validate_filters(raw: &[String]) -> Result<Vec<String>, RunError> {
        const KNOWN_TYPES: [&str; 7] = [
            "startup", "input", "lua:pure", "lua:api", "open", "edit", "scroll",
        ];

        let mut seen: HashMap<&str, usize> = HashMap::new();
        let mut out = Vec::with_capacity(raw.len());

        for (i, raw_name) in raw.iter().enumerate() {
            let name = raw_name.trim();
            if name.is_empty() {
                return Err(RunError::EmptyFilter { index: i });
            }
            if seen.insert(name, i).is_some() {
                return Err(RunError::DuplicateFilter {
                    name: name.to_owned(),
                });
            }
            if !KNOWN_TYPES.contains(&name) {
                return Err(RunError::UnknownFilter {
                    name: name.to_owned(),
                });
            }
            out.push(name.to_owned());
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Dry-run
    // ------------------------------------------------------------------

    /// Build a deterministic dry-run plan from the profile and validated filters.
    ///
    /// No fixture files are created and no engine processes are spawned.
    fn build_dry_run_plan(&self) -> Result<DryRunPlan, RunError> {
        let filters = Self::validate_filters(&self.config.workload_filter)?;

        let all_cells = workload::matrix(self.config.profile);

        let cells: Vec<WorkloadCell> = if filters.is_empty() {
            all_cells
        } else {
            all_cells
                .into_iter()
                .filter(|c| filters.iter().any(|f| c.id.type_name() == f.as_str()))
                .collect()
        };

        let plan_cells: Vec<DryRunCell> = cells
            .iter()
            .map(|c| DryRunCell {
                id: c.id,
                family: c.family,
                fixture: c.fixture,
                processes: self.config.profile.processes_per_engine(c.kind()),
                warmup: self.config.profile.warmup_per_process(c.id),
                samples: self.config.profile.samples_per_process(c.id),
                timeout: self.config.profile.timeout(c),
            })
            .collect();

        Ok(DryRunPlan {
            profile: self.config.profile,
            filters,
            cells: plan_cells,
        })
    }

    // ------------------------------------------------------------------
    // Matrix preparation
    // ------------------------------------------------------------------

    /// Validate, filter, and resolve the full measurement matrix.
    ///
    /// Validates filter names, selects cells from the declarative matrix,
    /// resolves each unique fixture requirement once, records checked
    /// [`FixtureArguments`] and file hashes, and indexes exact
    /// `(WorkloadId, EngineFamily)` pairs.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] on filter validation failure, pair duplication,
    /// missing engine pair, or I/O errors when resolving fixture arguments.
    fn prepare_matrix(&self) -> Result<PreparedMatrix, RunError> {
        let filters = Self::validate_filters(&self.config.workload_filter)?;

        // The fixture root is every session's working directory, including
        // cells whose `FixtureRequirement::None` never materializes a file:
        // create it once here so `current_dir` cannot ENOENT at spawn.
        fs::create_dir_all(&self.fixture_root).map_err(|source| RunError::Io {
            path: self.fixture_root.clone(),
            source,
        })?;

        let all_cells = workload::matrix(self.config.profile);

        let cells: Vec<WorkloadCell> = if filters.is_empty() {
            all_cells
        } else {
            all_cells
                .into_iter()
                .filter(|c| filters.iter().any(|f| c.id.type_name() == f.as_str()))
                .collect()
        };

        // Index cells by (WorkloadId, EngineFamily); error on duplicate.
        let mut pair_index: HashMap<(WorkloadId, EngineFamily), &WorkloadCell> = HashMap::new();
        for cell in &cells {
            let key = (cell.id, cell.family);
            if pair_index.insert(key, cell).is_some() {
                return Err(RunError::DuplicateCell {
                    workload: cell.id.cell_id(),
                    engine: engine_name(cell.family),
                });
            }
        }

        // Collect unique fixture requirements.
        let reqs: Vec<FixtureRequirement> = {
            let mut seen: HashSet<FixtureRequirement> = HashSet::new();
            for cell in &cells {
                seen.insert(cell.fixture);
            }
            seen.into_iter().collect()
        };

        // Resolve each requirement once, capturing path and hash.
        let mut fixtures: HashMap<FixtureRequirement, PreparedFixture> = HashMap::new();
        for &req in &reqs {
            let arguments = workload::resolve_fixture_arguments(req, &self.fixture_root)?;
            let sha256 = Self::hash_fixture(&arguments)?;
            fixtures.insert(req, PreparedFixture { arguments, sha256 });
        }

        // Build ordered pair list; error on missing engine for any workload.
        let mut workload_ids: Vec<WorkloadId> = cells.iter().map(|c| c.id).collect();
        // Dedup on the full cell id: a type_name sort leaves equal ids
        // non-adjacent (each engine family contributes its own copy), so
        // every workload would survive twice and duplicate contract cells.
        workload_ids.sort_by_key(|id| id.cell_id());
        workload_ids.dedup();

        let mut pairs: Vec<(WorkloadId, EngineFamily)> = Vec::with_capacity(cells.len());
        for id in &workload_ids {
            for family in EngineFamily::all() {
                let key = (*id, family);
                if !pair_index.contains_key(&key) {
                    return Err(RunError::MissingCell {
                        workload: id.cell_id(),
                        engine: engine_name(family),
                    });
                }
                pairs.push(key);
            }
        }

        Ok(PreparedMatrix {
            cells,
            fixtures,
            pairs,
        })
    }

    /// Return the SHA-256 hex digest of the file referenced by `arguments`.
    fn hash_fixture(arguments: &FixtureArguments) -> Result<Option<String>, RunError> {
        let path = match arguments {
            FixtureArguments::None => return Ok(None),
            // A plugin tree is a directory; its content digest is
            // deterministic per count and re-derived by `plugin_tree` on
            // every materialization, so re-reading the tree here would be
            // both wrong (EISDIR) and redundant.
            FixtureArguments::PluginTree { count, .. } => {
                return Ok(Some(fixture::plugin_hash(*count)));
            }
            FixtureArguments::LargeBuffer { path } => path,
        };
        let hash = fixture::hash_file(path).map_err(RunError::from)?;
        Ok(Some(hash))
    }

    /// Execute the oracle-vs-oracle noise-calibration matrix.
    ///
    /// For every selected workload the exact Neovim cell and fixture from the
    /// prepared matrix run for both calibration arms in balanced ABBA order
    /// (A, B, B, A, ...), labeled [`EngineLabel::NoiseA`] and
    /// [`EngineLabel::NoiseB`] and launched directly. The collected samples
    /// and process records are summarized by [`summarize_noise`]; the
    /// calibration verdict — including a noisy-host ceiling failure — is
    /// carried by [`NoiseVerdict`] inside the returned
    /// [`RunSummary::NoiseCalibration`]. Comparison labels are never emitted,
    /// comparison workloads are never manufactured, and comparison judging is
    /// never invoked.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the run identity is absent, the run
    /// directory cannot be created, an engine process fails, or
    /// [`summarize_noise`] rejects the collection.
    fn run_noise_calibration(&self, matrix: &PreparedMatrix) -> Result<RunSummary, RunError> {
        let (run_id, run_dir) = self.require_run_identity()?;
        fs::create_dir_all(&run_dir).map_err(|source| RunError::Io {
            path: run_dir.clone(),
            source,
        })?;
        let started_unix = unix_seconds_now()?;

        // One Neovim cell + prepared fixture per selected workload, in
        // matrix execution order.
        let cells = matrix.neovim_cells()?;

        let contract = self.build_measurement_contract(&cells)?;

        // Snapshots are preserved on every sample for divergence detection.
        let mut collected: Vec<CollectedSample> = Vec::new();
        let mut processes: Vec<ProcessRun> = Vec::new();
        for (cell, fixture) in &cells {
            let process_count = self.config.profile.processes_per_engine(cell.kind());
            // abba_order alternates arm slots A,B,B,A,...; both slots run
            // the same Neovim binary here and differ only in label.
            let order = abba_order(process_count);
            let mut arm_seen = [0usize; 2];
            for family in &order {
                let (engine_label, arm) = match family {
                    EngineFamily::Neovim => (EngineLabel::NoiseA, 0),
                    EngineFamily::Oxvim => (EngineLabel::NoiseB, 1),
                };
                let process_index = arm_seen[arm];
                arm_seen[arm] += 1;
                let output = self.run_one_process(ProcessInvocation {
                    cell,
                    fixture,
                    run_id: &run_id,
                    engine_label,
                    process_index,
                    launcher: ProcessLauncher::Direct,
                })?;
                processes.push(output.process);
                collected.extend(output.samples);
            }
        }

        // Owned values for the report API; the snapshot-paired collection
        // stays alive for divergence detection and stage rows.
        let samples: Vec<Sample> = collected.iter().map(|c| c.sample.clone()).collect();
        let noise = summarize_noise(&run_id, &contract, &samples, &processes)?;
        let divergences = compute_divergences(
            &run_id,
            EngineLabel::NoiseA,
            EngineLabel::NoiseB,
            &collected,
        )?;
        let summary = RunSummary::NoiseCalibration(noise.clone());

        let manifest = build_run_manifest(
            &contract,
            &run_id,
            ManifestMode::NoiseCalibration,
            started_unix,
            None,
            None,
        );

        write_artifact_json(&run_dir.join("manifest.json"), &manifest)?;
        write_artifact_ndjson(&run_dir.join("samples.ndjson"), &samples)?;
        write_artifact_ndjson(&run_dir.join("processes.ndjson"), &processes)?;
        write_artifact_json(&run_dir.join("noise.json"), &noise)?;
        write_artifact_json(&run_dir.join("divergences.json"), &divergences)?;
        // The summary is the final claim: it lands only after every
        // supporting artifact is on disk.
        write_artifact_json(&run_dir.join("summary.json"), &summary)?;
        Ok(summary)
    }

    /// The run identity required by every completed mode.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Io`] when the runner was constructed without a
    /// run identifier or directory (dry-run).
    fn require_run_identity(&self) -> Result<(String, PathBuf), RunError> {
        let run_id = match &self.run_id {
            Some(id) => id.clone(),
            None => {
                return Err(missing_run_identity(&self.config.output_root, "run id"));
            }
        };
        let run_dir = match &self.run_dir {
            Some(dir) => dir.clone(),
            None => {
                return Err(missing_run_identity(
                    &self.config.output_root,
                    "run directory",
                ));
            }
        };
        Ok((run_id, run_dir))
    }

    /// Build the exact measurement contract for the selected Neovim cells.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when an engine binary, a host fact, or a
    /// fixture identity cannot be read.
    fn build_measurement_contract(
        &self,
        cells: &[(&WorkloadCell, &PreparedFixture)],
    ) -> Result<MeasurementContract, RunError> {
        // Kept in lockstep with report::NOISE_SCHEMA_VERSION.
        const CONTRACT_SCHEMA_VERSION: u32 = 1;

        let oracle_binary = Engine::Neovim.command();
        let candidate_binary = Engine::Oxvim.command();
        let oracle = contract_file_identity(&oracle_binary)?;
        let candidate = contract_file_identity(&candidate_binary)?;

        let host = HostFingerprint {
            hostname: hostname()?,
            os: env::consts::OS.to_owned(),
            kernel: kernel_release()?,
            architecture: env::consts::ARCH.to_owned(),
            cpu: cpu_model()?,
            logical_cpus: num_cpus()?,
            memory_kib: memory_total_kib()?,
        };

        // Every session attaches through the isolated default geometry with
        // line-grid and RGB enabled (see PerfSession); derive the UI shape
        // from one real spawn config so it cannot drift from what ran.
        let (first_cell, first_fixture) = cells.first().ok_or_else(|| {
            RunError::Report(ReportError::InvalidContract {
                reason: "no workload cells selected".to_owned(),
            })
        })?;
        let spawn_config = first_cell.spawn_config(
            &self.fixture_root,
            self.config.profile,
            &first_fixture.arguments,
        )?;
        let (width, height) = spawn_config.ui_size.dimensions();
        let ui = UiContract {
            width,
            height,
            rgb: true,
            ext_linegrid: true,
        };

        let mut contract_cells = Vec::with_capacity(cells.len());
        for (cell, fixture) in cells {
            let timeout_ms =
                u64::try_from(self.config.profile.timeout(cell).as_millis()).map_err(|source| {
                    RunError::IntegerConversion {
                        context: "cell timeout_ms",
                        source,
                    }
                })?;
            let wall_limit_ms = u64::try_from(self.config.profile.wall_cap(cell)?.as_millis())
                .map_err(|source| RunError::IntegerConversion {
                    context: "cell wall_limit_ms",
                    source,
                })?;
            contract_cells.push(CellContract {
                workload: cell.id.cell_id(),
                processes_per_engine: self.config.profile.processes_per_engine(cell.kind()),
                samples_per_process: self.config.profile.samples_per_process(cell.id),
                warmup_per_process: self.config.profile.warmup_per_process(cell.id),
                timeout_ms,
                wall_limit_ms,
                // One instrumented DHAT run per scheduled process.
                allocation_runs_per_engine: self.config.profile.processes_per_engine(cell.kind()),
                input_definition: input_definition(cell.id),
                stage_contract: stage_contract(cell.id),
                fixture_hashes: fixture_identities(fixture),
            });
        }

        Ok(MeasurementContract {
            schema_version: CONTRACT_SCHEMA_VERSION,
            profile: self.config.profile,
            release_harness_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            host,
            oracle,
            candidate,
            ui,
            // ABBA blocks begin with arm A.
            order: AlternationOrder::AlternatingFirstArm,
            cells: contract_cells,
        })
    }

    /// Execute the full cross-engine comparison matrix.
    ///
    /// The noise calibration is read, decoded as a typed artifact, and gated
    /// on schema, run identity, exact contract equality, verdict usability,
    /// and its own noise ceilings *before* any engine process spawns; an
    /// incompatible or noisy-host calibration rejects the run without
    /// spending a single process. Direct ABBA collection then runs the exact
    /// Neovim/Oxvim cell pairs, followed by the separate DHAT allocation
    /// pass and the `--startuptime` side pass, and the report module judges
    /// the collected evidence.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the run identity is absent, the calibration
    /// cannot be read or decoded or does not back this run, an engine or
    /// profiler process fails, the startup side pass fails, or the report
    /// rejects the collected evidence.
    fn run_comparison(&self, matrix: &PreparedMatrix) -> Result<RunSummary, RunError> {
        let (run_id, run_dir) = self.require_run_identity()?;
        fs::create_dir_all(&run_dir).map_err(|source| RunError::Io {
            path: run_dir.clone(),
            source,
        })?;
        let started_unix = unix_seconds_now()?;

        let inputs = self.comparison_inputs()?;

        // One contract row per selected workload: the Neovim arm's
        // cell/fixture pair, exactly as the calibration contract was built.
        let cells = matrix.neovim_cells()?;
        let contract = self.build_measurement_contract(&cells)?;

        // The calibration source identity feeds the manifest writer.
        let (calibration, calibration_source) =
            load_noise_calibration(inputs.noise_path, &contract)?;

        // Direct timing evidence first; allocation and startup evidence are
        // separate passes.
        let direct = self.collect_direct_pass(matrix, &run_id)?;
        let divergences = compute_divergences(
            &run_id,
            EngineLabel::Neovim,
            EngineLabel::Oxvim,
            &direct.samples,
        )?;

        // Snapshots have served divergence detection; only the typed report
        // samples continue to the report builders and samples.ndjson.
        let samples: Vec<Sample> = direct
            .samples
            .into_iter()
            .map(|collected| collected.sample)
            .collect();

        let (allocations, valgrind_provenance) =
            self.run_allocation_pass(matrix, inputs.valgrind_path)?;
        // The startup side pass keeps its parsed records for the artifact
        // writers; its strict log validation is load-bearing either way.
        let startuptime = self
            .run_startuptime_side_pass(matrix)?
            .into_artifact(&run_id);
        let startup_stage_runs = startuptime.stage_runs();
        let stages = summarize_stages(
            &run_id,
            &contract,
            &samples,
            &startup_stage_runs,
            &calibration,
        )?;
        let comparison: ComparisonRunSummary = judge_comparison(
            &run_id,
            &contract,
            &samples,
            &direct.processes,
            &allocations,
            &calibration,
        )?;
        let summary = RunSummary::Comparison(comparison);
        let manifest = build_run_manifest(
            &contract,
            &run_id,
            ManifestMode::Comparison,
            started_unix,
            Some(&calibration_source),
            Some(valgrind_provenance),
        );

        ComparisonArtifacts {
            manifest: &manifest,
            samples: &samples,
            processes: &direct.processes,
            allocations: &allocations,
            calibration: &calibration,
            divergences: &divergences,
            stages: &stages,
            startuptime: &startuptime,
            summary: &summary,
        }
        .write_all(&run_dir)?;
        Ok(summary)
    }

    /// Resolve the comparison mode's calibration and profiler inputs,
    /// rejecting a missing valgrind binary before any process spawns.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the runner was not constructed for
    /// comparison mode or the mode carries no valgrind binary.
    fn comparison_inputs(&self) -> Result<ComparisonInputs<'_>, RunError> {
        let ExecutionMode::Comparison {
            noise_path,
            valgrind_path,
        } = &self.config.mode
        else {
            return Err(missing_run_identity(
                &self.config.output_root,
                "comparison mode",
            ));
        };
        // The report judges exact DHAT allocation coverage, so a comparison
        // without a valgrind binary can never produce a valid summary.
        let valgrind_path =
            valgrind_path
                .as_deref()
                .ok_or_else(|| RunError::ProfilerPreflight {
                    path: PathBuf::from("valgrind"),
                    detail: "comparison requires a valgrind binary: the report \
                         demands exact allocation coverage"
                        .to_owned(),
                })?;
        Ok(ComparisonInputs {
            noise_path,
            valgrind_path,
        })
    }

    /// Collect the direct (uninstrumented) ABBA comparison pass: every
    /// selected workload's Neovim/Oxvim cell pair runs once per process slot
    /// in `Nv, Ox, Ox, Nv` blocks, one sample batch per scheduled process.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when a cell pair is incomplete or a process
    /// fails.
    fn collect_direct_pass(
        &self,
        matrix: &PreparedMatrix,
        run_id: &str,
    ) -> Result<DirectCollection, RunError> {
        let mut collection = DirectCollection::default();

        for workload_id in matrix
            .pairs
            .iter()
            .filter(|(_, family)| *family == EngineFamily::Neovim)
            .map(|(workload_id, _)| workload_id)
        {
            let head_cell = matrix.neovim_head_cell(workload_id)?;
            let process_count = self.config.profile.processes_per_engine(head_cell.kind());
            let mut process_indexes = [0usize; 2];
            for family in abba_order(process_count) {
                let (engine_label, counter) = match family {
                    EngineFamily::Neovim => (EngineLabel::Neovim, 0),
                    EngineFamily::Oxvim => (EngineLabel::Oxvim, 1),
                };
                let (cell, fixture) = matrix.abba_cell(workload_id, family)?;
                let process_index = process_indexes[counter];
                process_indexes[counter] += 1;
                let ProcessOutput { process, samples } =
                    self.run_one_process(ProcessInvocation {
                        cell,
                        fixture,
                        run_id,
                        engine_label,
                        process_index,
                        launcher: ProcessLauncher::Direct,
                    })?;
                collection.processes.push(process);
                collection.samples.extend(samples);
            }
        }
        Ok(collection)
    }

    /// Run one scheduled process and collect its samples and process record.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the spawn configuration cannot be resolved,
    /// the process fails, or the workload kind is neither startup nor
    /// steady-state.
    fn run_one_process(
        &self,
        invocation: ProcessInvocation<'_>,
    ) -> Result<ProcessOutput, RunError> {
        let execution =
            ProcessExecution::resolve(invocation, self.config.profile, &self.fixture_root)?;

        let measurements = match execution.cell.kind() {
            Kind::Startup => Self::run_startup_process(&execution)?,
            Kind::SteadyState => Self::run_steady_state_process(&execution)?,
        };

        let samples_recorded = measurements.samples.len();
        let process = ProcessRun {
            run_id: execution.run_id.to_owned(),
            workload: execution.cell.cell_id(),
            engine: execution.engine_label,
            process_index: execution.process_index,
            pid: measurements.pid,
            peak_rss_kib: measurements.peak_rss_kib,
            warmup_discarded: self.config.profile.warmup_per_process(execution.cell.id),
            samples_recorded,
        };

        Ok(ProcessOutput {
            process,
            samples: measurements.samples,
        })
    }

    /// Run one startup-kind process.
    ///
    /// Startup workloads measure a single session: plugin-startup cells
    /// validate the API-info protocol (channel, metadata, exactly api level
    /// 15, plugin sink) before reading peak RSS, and open cells validate the
    /// initial-flush grid. Exactly one sample is produced.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the session cannot spawn, the protocol or
    /// grid validation fails, or the shutdown fails alongside a primary
    /// error.
    fn run_startup_process(
        execution: &ProcessExecution<'_>,
    ) -> Result<ProcessMeasurements, RunError> {
        match execution.cell.id {
            WorkloadId::Startup { plugin_count } => {
                Self::run_startup_plugin_process(execution, plugin_count)
            }
            WorkloadId::Open => Self::run_startup_open_process(execution),
            _ => Err(RunError::InvalidWorkloadKind {
                workload: execution.cell.cell_id(),
                expected: "startup",
            }),
        }
    }

    /// Run one plugin-startup cell: spawn to the first complete initial
    /// flush, validate protocol shape and the plugin sink, and read peak
    /// RSS.
    ///
    /// WHY flush-based: `--embed` defers startup completion — and therefore
    /// `plugin/*.lua` loading — until `ui_attach`, so the spawn-to-flush
    /// window is the only one that actually contains plugin loading; the
    /// api-info-only window closes before any plugin can run.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the session cannot spawn, the protocol or
    /// sink validation fails, or the shutdown fails alongside a primary
    /// error.
    fn run_startup_plugin_process(
        execution: &ProcessExecution<'_>,
        plugin_count: usize,
    ) -> Result<ProcessMeasurements, RunError> {
        let (mut session, elapsed, snapshot) = PerfSession::spawn_to_initial_flush(
            execution.engine,
            execution.case,
            execution.config.clone(),
        )?;

        let measured = (|| {
            if snapshot.rendered_grid.is_empty() {
                return Err(RunError::Session(PerfError::Protocol(
                    "plugin startup produced an empty rendered grid".to_owned(),
                )));
            }
            let (_, api_info) = session
                .request_to_response_staged("nvim_get_api_info", vec![])
                .map_err(RunError::Session)?;
            let api_info = api_info.map_err(|message| PerfError::Request {
                method: "nvim_get_api_info".to_owned(),
                message,
            })?;
            Self::validate_api_info_shape(&api_info)?;
            Self::require_plugin_sink(&mut session, plugin_count)?;
            Ok(session.peak_rss_kib()?)
        })();

        let pid = session.child_pid();
        Self::finish_ready_startup_measurement(
            session,
            pid,
            elapsed,
            measured,
            |startup_delta_us| {
                Ok(startup_sample(
                    execution.cell,
                    execution.run_id,
                    execution.engine_label,
                    execution.process_index,
                    startup_delta_us,
                    hash_grid(&snapshot.rendered_grid),
                    Some(snapshot),
                ))
            },
        )
    }

    /// Run one open cell: spawn to the first complete initial flush,
    /// validate the rendered grid, and read peak RSS.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the session cannot spawn, the grid is
    /// empty, or the shutdown fails alongside a primary error.
    fn run_startup_open_process(
        execution: &ProcessExecution<'_>,
    ) -> Result<ProcessMeasurements, RunError> {
        let (session, elapsed, snapshot) = PerfSession::spawn_to_initial_flush(
            execution.engine,
            execution.case,
            execution.config.clone(),
        )?;

        let measured = (|| {
            if snapshot.rendered_grid.is_empty() {
                return Err(RunError::Session(PerfError::Protocol(
                    "open workload produced an empty rendered grid".to_owned(),
                )));
            }
            Ok(session.peak_rss_kib()?)
        })();

        let pid = session.child_pid();
        Self::finish_ready_startup_measurement(
            session,
            pid,
            elapsed,
            measured,
            |startup_delta_us| {
                Ok(startup_sample(
                    execution.cell,
                    execution.run_id,
                    execution.engine_label,
                    execution.process_index,
                    startup_delta_us,
                    hash_grid(&snapshot.rendered_grid),
                    Some(snapshot),
                ))
            },
        )
    }

    fn finish_ready_startup_measurement(
        session: PerfSession<Ready>,
        pid: u32,
        elapsed: Duration,
        measured: Result<u64, RunError>,
        collected: impl FnOnce(u64) -> Result<CollectedSample, RunError>,
    ) -> Result<ProcessMeasurements, RunError> {
        match measured {
            Ok(peak_rss_kib) => {
                session.shutdown()?;
                let startup_delta_us = duration_to_us(elapsed)?;
                Ok(ProcessMeasurements {
                    samples: vec![collected(startup_delta_us)?],
                    peak_rss_kib,
                    pid,
                })
            }
            Err(e) => Err(shutdown_alongside(e, session.shutdown())),
        }
    }

    /// Validate the `nvim_get_api_info` protocol shape: a two-item array, a
    /// positive channel, dict metadata carrying a dict `version`, exactly
    /// api level 15.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Session`] protocol failures for every mismatch.
    fn validate_api_info_shape(api_info: &Object) -> Result<(), RunError> {
        let channel_and_meta = match api_info {
            Object::Array(items) if items.len() == 2 => items,
            _ => {
                return Err(RunError::Session(PerfError::Protocol(format!(
                    "nvim_get_api_info returned {api_info:?}, expected a two-item array"
                ))));
            }
        };

        match &channel_and_meta[0] {
            Object::Integer(value) if *value > 0 => {}
            _ => {
                return Err(RunError::Session(PerfError::Protocol(format!(
                    "nvim_get_api_info channel is not a positive integer: {:?}",
                    channel_and_meta[0]
                ))));
            }
        }

        let Object::Dict(metadata) = &channel_and_meta[1] else {
            return Err(RunError::Session(PerfError::Protocol(format!(
                "nvim_get_api_info metadata is not a dict: {:?}",
                channel_and_meta[1]
            ))));
        };

        let version_obj = metadata.get(&OxStr::from("version")).ok_or_else(|| {
            RunError::Session(PerfError::Protocol(
                "nvim_get_api_info metadata missing 'version' key".to_owned(),
            ))
        })?;
        let Object::Dict(version_dict) = version_obj else {
            return Err(RunError::Session(PerfError::Protocol(format!(
                "nvim_get_api_info version is not a dict: {version_obj:?}"
            ))));
        };

        match version_dict.get(&OxStr::from("api_level")) {
            Some(Object::Integer(level)) if *level == 15 => {}
            Some(other) => {
                return Err(RunError::Session(PerfError::Protocol(format!(
                    "nvim_get_api_info api_level is not exactly 15: {other:?}"
                ))));
            }
            None => {
                return Err(RunError::Session(PerfError::Protocol(
                    "nvim_get_api_info version missing 'api_level' key".to_owned(),
                )));
            }
        }
        Ok(())
    }

    /// Require the plugin sink to equal the fixture's expected total for
    /// `plugin_count`; call only after the first flush, because `--embed`
    /// loads plugins at startup completion, not at spawn.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] for request failures and protocol mismatches.
    fn require_plugin_sink(
        session: &mut PerfSession<Ready>,
        plugin_count: usize,
    ) -> Result<(), RunError> {
        let (_, sink_result) = session
            .request_to_response_staged(
                "nvim_exec_lua",
                vec![
                    Object::String(OxStr::from("return _G.__perf_sink or 0")),
                    Object::Array(vec![]),
                ],
            )
            .map_err(RunError::Session)?;
        let sink_value = sink_result.map_err(|message| PerfError::Request {
            method: "nvim_exec_lua".to_owned(),
            message,
        })?;
        let Object::Integer(actual_sink) = &sink_value else {
            return Err(RunError::Session(PerfError::Protocol(format!(
                "plugin sink is not an integer: {sink_value:?}"
            ))));
        };
        let expected_sink = fixture::expected_plugin_sink(plugin_count)?;
        if *actual_sink != expected_sink {
            return Err(RunError::Session(PerfError::Protocol(format!(
                "plugin sink mismatch: expected {expected_sink}, got {actual_sink}"
            ))));
        }
        Ok(())
    }

    /// Run one steady-state process.
    ///
    /// The session runs `warmup_per_process` discarded windows, then
    /// `samples_per_process` measured windows; every measured window must
    /// produce a nonempty rendered grid and a response that does not exceed
    /// its wall time.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when the session cannot spawn, a window or
    /// snapshot fails, timing is inconsistent, or the shutdown fails
    /// alongside a primary error.
    fn run_steady_state_process(
        execution: &ProcessExecution<'_>,
    ) -> Result<ProcessMeasurements, RunError> {
        if execution.cell.kind() != Kind::SteadyState {
            return Err(RunError::InvalidWorkloadKind {
                workload: execution.cell.cell_id(),
                expected: "steady-state",
            });
        }

        let spawned =
            PerfSession::spawn(execution.engine, execution.case, execution.config.clone())?;
        let (attached, _) = spawned.attach_ui()?;
        let mut session = attached.finish_setup()?;

        let collected = (|| {
            Self::run_steady_warmups(&mut session, execution.cell, execution.profile)?;
            let samples = Self::collect_steady_samples(
                &mut session,
                execution.cell,
                execution.profile,
                execution.run_id,
                execution.engine_label,
                execution.process_index,
            )?;
            let peak_rss_kib = session.peak_rss_kib()?;
            Ok((samples, peak_rss_kib))
        })();

        let pid = session.child_pid();
        match collected {
            Ok((samples, peak_rss_kib)) => {
                session.shutdown()?;
                Ok(ProcessMeasurements {
                    samples,
                    peak_rss_kib,
                    pid,
                })
            }
            Err(e) => Err(shutdown_alongside(e, session.shutdown())),
        }
    }

    /// Run and discard the warmup windows, draining deferred work after them
    /// so no warmup side effect leaks into a measured window.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when a warmup window fails.
    fn run_steady_warmups(
        session: &mut PerfSession<Ready>,
        cell: &WorkloadCell,
        profile: Profile,
    ) -> Result<(), RunError> {
        for i in 0..profile.warmup_per_process(cell.id) {
            let _ = workload::run_steady_window(session, cell.id, i)?;
        }
        let _ = session.drain_deferred();
        Ok(())
    }

    /// Collect one verified sample per measured steady-state window.
    ///
    /// Each window must produce a nonempty rendered grid; for
    /// flush-accounting workloads the response must not exceed the wall
    /// time.
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] when a window, snapshot, or hash fails or the
    /// timing is inconsistent.
    fn collect_steady_samples(
        session: &mut PerfSession<Ready>,
        cell: &WorkloadCell,
        profile: Profile,
        run_id: &str,
        engine_label: EngineLabel,
        process_index: usize,
    ) -> Result<Vec<CollectedSample>, RunError> {
        let engine = cell.family.to_engine();
        let mut samples = Vec::with_capacity(profile.samples_per_process(cell.id));
        for i in 0..profile.samples_per_process(cell.id) {
            let window = workload::run_steady_window(session, cell.id, i)?;
            let snapshot = session.snapshot()?;
            if snapshot.rendered_grid.is_empty() {
                return Err(RunError::Session(PerfError::Protocol(
                    "steady-state window produced an empty rendered grid".to_owned(),
                )));
            }
            let grid_hash = hash_grid(&snapshot.rendered_grid);

            let wall_us = duration_to_us(window.wall)?;
            let response_us = duration_to_us(window.response)?;

            let (response_opt, flush_opt) = match cell.id {
                WorkloadId::LuaPure | WorkloadId::LuaApi => (Some(response_us), None),
                _ => {
                    let flush_us = wall_us.checked_sub(response_us).ok_or_else(|| {
                        RunError::Session(PerfError::Protocol(format!(
                            "steady-state {}: response_us exceeds wall_us",
                            cell.cell_id()
                        )))
                    })?;
                    (Some(response_us), Some(flush_us))
                }
            };

            samples.push(CollectedSample {
                sample: Sample {
                    run_id: run_id.to_owned(),
                    workload: cell.cell_id(),
                    engine: engine_label,
                    process_index,
                    sample_index: i,
                    wall_us,
                    response_us: response_opt,
                    flush_us: flush_opt,
                    startup_delta_us: None,
                    grid_sha256: grid_hash,
                    raw_source: format!("{engine:?}"),
                },
                snapshot: Some(snapshot),
            });
        }
        Ok(samples)
    }

    // ------------------------------------------------------------------
    // Allocation pass (Valgrind DHAT)
    // ------------------------------------------------------------------

    /// Run the DHAT allocation pass over the prepared matrix.
    ///
    /// Each selected cell/engine/process is run once under
    /// `valgrind --tool=dhat --mode=heap --trace-children=no` using
    /// [`ProcessLauncher::Prefix`]. The same workload config, fixture,
    /// warmups, samples, and correctness lifecycle apply. Timing and RSS
    /// from the profiler-wrapped process are discarded; only the DHAT
    /// totals (decoded from the raw JSON with the actual child PID) and
    /// raw file paths are preserved in each [`AllocationRun`].
    ///
    /// # Errors
    ///
    /// Returns [`RunError::ProfilerPreflight`] if the Valgrind path is not
    /// absolute or `--version` fails, [`RunError::ProfilerExit`] if a
    /// profiler-wrapped process fails, [`RunError::ProfilerRead`] if the
    /// DHAT output cannot be read, or [`RunError::ProfilerDecode`] if the
    /// DHAT document is invalid.
    fn run_allocation_pass(
        &self,
        matrix: &PreparedMatrix,
        valgrind_path: &Path,
    ) -> Result<(Vec<AllocationRun>, ProfilerProvenance), RunError> {
        let valgrind_version = preflight_valgrind(valgrind_path)?;
        let run_id = self
            .run_id
            .as_deref()
            .ok_or_else(|| RunError::ProfilerPreflight {
                path: valgrind_path.to_path_buf(),
                detail: "run_id not set (dry-run mode?)".to_owned(),
            })?;
        let run_dir = self
            .run_dir
            .as_deref()
            .ok_or_else(|| RunError::ProfilerPreflight {
                path: valgrind_path.to_path_buf(),
                detail: "run_dir not set (dry-run mode?)".to_owned(),
            })?;

        let alloc_dir = run_dir.join("allocations");
        fs::create_dir_all(&alloc_dir).map_err(|source| RunError::Io {
            path: alloc_dir.clone(),
            source,
        })?;

        let mut runs = Vec::new();

        for workload_id in matrix
            .pairs
            .iter()
            .filter(|(_, family)| *family == EngineFamily::Neovim)
            .map(|(workload_id, _)| workload_id)
        {
            let head_cell = matrix.neovim_head_cell(workload_id)?;
            let process_count = self.config.profile.processes_per_engine(head_cell.kind());
            let mut process_indexes = [0usize; 2];
            for family in abba_order(process_count) {
                let (engine_label, counter) = match family {
                    EngineFamily::Neovim => (EngineLabel::Neovim, 0),
                    EngineFamily::Oxvim => (EngineLabel::Oxvim, 1),
                };
                let (cell, fixture) = matrix.abba_cell(workload_id, family)?;
                let process_index = process_indexes[counter];
                process_indexes[counter] += 1;

                let engine = engine_name(family);
                let stem = format!("{}_{}_p{}", workload_id.cell_id(), engine, process_index);
                let dhat_path = alloc_dir.join(format!("{stem}.dhat.json"));
                let valgrind_log_path = alloc_dir.join(format!("{stem}.valgrind.log"));

                let launcher = ProcessLauncher::Prefix {
                    executable: valgrind_path.to_path_buf(),
                    args: vec![
                        "--tool=dhat".into(),
                        "--mode=heap".into(),
                        "--trace-children=no".into(),
                        format!("--dhat-out-file={}", dhat_path.display()).into(),
                        format!("--log-file={}", valgrind_log_path.display()).into(),
                    ],
                };

                runs.push(self.collect_allocation_run(InstrumentedRun {
                    invocation: ProcessInvocation {
                        cell,
                        fixture,
                        run_id,
                        engine_label,
                        process_index,
                        launcher,
                    },
                    engine,
                    dhat_path,
                    valgrind_log_path,
                })?);
            }
        }

        let provenance = ProfilerProvenance {
            path: valgrind_path.to_path_buf(),
            version: valgrind_version,
            options: vec![
                "--tool=dhat".to_owned(),
                "--mode=heap".to_owned(),
                "--trace-children=no".to_owned(),
            ],
            output_pattern: "allocations/<workload>_<engine>_p<process>.dhat.json".to_owned(),
        };
        Ok((runs, provenance))
    }

    /// Run one DHAT-instrumented process and decode its allocation totals.
    ///
    /// The profiler-wrapped lifecycle is the same as the direct pass; its
    /// timing and RSS are discarded, and only the decoded DHAT totals (bound
    /// to the actual child PID) and the hashed raw files survive.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::ProfilerExit`] when the wrapped process fails,
    /// [`RunError::ProfilerRead`] when the DHAT output cannot be read, and
    /// [`RunError::ProfilerDecode`] when the DHAT document is invalid.
    fn collect_allocation_run(
        &self,
        instrumented: InstrumentedRun<'_>,
    ) -> Result<AllocationRun, RunError> {
        let InstrumentedRun {
            invocation,
            engine,
            dhat_path,
            valgrind_log_path,
        } = instrumented;
        let workload = invocation.cell.cell_id();
        let run_id = invocation.run_id;
        let engine_label = invocation.engine_label;
        let process_index = invocation.process_index;

        // Run the same process lifecycle; discard timing/RSS.
        let output = self.run_one_process(invocation).map_err(|e| match e {
            RunError::Session(source) => RunError::ProfilerExit {
                workload: workload.clone(),
                engine: engine.clone(),
                source,
            },
            other => other,
        })?;

        let pid = output.process.pid;

        // Read the DHAT output after clean shutdown.
        let dhat_bytes = fs::read(&dhat_path).map_err(|source| RunError::ProfilerRead {
            path: dhat_path.clone(),
            source,
        })?;

        let totals = allocation::decode_dhat(&dhat_bytes, pid).map_err(|source| {
            RunError::ProfilerDecode {
                workload: workload.clone(),
                engine,
                source,
            }
        })?;

        let dhat_sha = fixture::hash_file(&dhat_path).map_err(RunError::from)?;
        let valgrind_log_sha = fixture::hash_file(&valgrind_log_path).map_err(RunError::from)?;

        Ok(AllocationRun {
            run_id: run_id.to_owned(),
            workload,
            engine: engine_label,
            process_index,
            total_blocks: totals.total_blocks,
            total_bytes: totals.total_bytes,
            raw_files: vec![
                ReportFileIdentity {
                    path: dhat_path.display().to_string(),
                    sha256: dhat_sha,
                },
                ReportFileIdentity {
                    path: valgrind_log_path.display().to_string(),
                    sha256: valgrind_log_sha,
                },
            ],
        })
    }

    // ------------------------------------------------------------------
    // Startup-time side pass
    // ------------------------------------------------------------------

    /// Run the `--startuptime` side pass over selected startup/open cells.
    ///
    /// One [`StartuptimeRecord`] is produced per engine/workload/process,
    /// preserving the raw log path and parsed [`startuptime::StartupLog`].
    /// The log is parsed with strict integer microsecond parsing; a
    /// nonempty, consistent ordinal/label sequence is required within each
    /// engine/workload group. Spawn, exit, read, and parse errors are
    /// propagated as [`RunError::StartuptimeSide`].
    ///
    /// When `config.startuptime` is false, returns
    /// [`StartuptimeSidePass::NotCollected`].
    fn run_startuptime_side_pass(
        &self,
        matrix: &PreparedMatrix,
    ) -> Result<StartuptimeSidePass, RunError> {
        if !self.config.startuptime {
            return Ok(StartuptimeSidePass::NotCollected);
        }

        let run_dir = self
            .run_dir
            .as_deref()
            .ok_or_else(|| RunError::StartuptimeSide {
                engine: "*".to_owned(),
                workload: "*".to_owned(),
                detail: "run_dir not set (dry-run mode?)".to_owned(),
            })?;

        let startup_dir = run_dir.join("startuptime");
        fs::create_dir_all(&startup_dir).map_err(|source| RunError::Io {
            path: startup_dir.clone(),
            source,
        })?;

        let mut records = Vec::new();

        for (workload_id, family) in &matrix.pairs {
            // Only startup/open cells are eligible for the side pass.
            if workload_id.kind() != Kind::Startup {
                continue;
            }

            let cell = matrix
                .cells
                .iter()
                .find(|c| c.id == *workload_id && c.family == *family)
                .ok_or_else(|| RunError::MissingCell {
                    workload: workload_id.cell_id(),
                    engine: engine_name(*family),
                })?;
            let fixture = &matrix.fixtures[&cell.fixture];
            let engine = family.to_engine();
            let engine_label = match family {
                EngineFamily::Neovim => EngineLabel::Neovim,
                EngineFamily::Oxvim => EngineLabel::Oxvim,
            };

            let process_count = self.config.profile.processes_per_engine(cell.kind());

            for process_index in 0..process_count {
                let log_name = format!(
                    "{}_{}_p{}.log",
                    workload_id.cell_id(),
                    engine_name(*family),
                    process_index
                );

                // Build the session config for env isolation and fixture args.
                let config =
                    cell.spawn_config(&self.fixture_root, self.config.profile, &fixture.arguments)?;

                records.push(Self::collect_startuptime_log(StartupLogRun {
                    cell,
                    engine,
                    engine_label,
                    process_index,
                    config,
                    startup_dir: &startup_dir,
                    log_name,
                })?);
            }

            // Require consistent ordinal/label sequence within this
            // engine/workload group: all processes must share the same
            // mark label sequence.
            let workload = workload_id.cell_id();
            let group: Vec<&StartuptimeRecord> = records
                .iter()
                .filter(|r| r.workload == workload && r.engine == engine_label)
                .collect();
            require_consistent_mark_sequence(&group, &workload, *family)?;
        }

        Ok(StartuptimeSidePass::Collected(records))
    }

    /// Spawn the editor once with `--startuptime`, then read and strictly
    /// parse its log, requiring a nonempty mark list with nonempty labels.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::StartuptimeSide`] when the editor cannot spawn,
    /// exits nonzero, the log cannot be read or parsed, or a mark carries
    /// no label.
    fn collect_startuptime_log(run: StartupLogRun<'_>) -> Result<StartuptimeRecord, RunError> {
        let StartupLogRun {
            cell,
            engine,
            engine_label,
            process_index,
            config,
            startup_dir,
            log_name,
        } = run;
        let workload = cell.cell_id();
        let engine_display = engine_name(cell.family);
        let side_error = |detail: String| RunError::StartuptimeSide {
            engine: engine_display.clone(),
            workload: workload.clone(),
            detail,
        };

        let log_path = startup_dir.join(&log_name);
        let case = cell.session_case();

        // Build a direct command with --startuptime instead of --embed.
        let editor = engine.command();
        let mut command = Command::new(&editor);
        command
            // The oracle rejects an attached value ("garbage after option
            // argument"): --startuptime takes its path as a separate argv
            // element, unlike the single-token --embed style flags.
            .arg("--startuptime")
            .arg(log_path.display().to_string())
            // Exit mode, uniform for both engines (probed): --headless
            // avoids any UI, and +qa! quits after startup so the process
            // terminates on its own; -es is not uniform (the oracle exits 1
            // on stdin EOF) and the interactive path would block.
            .arg("--headless")
            .arg("+qa!")
            .arg("--clean")
            .arg("-n")
            .arg("-i")
            .arg("NONE");
        if !matches!(case, WorkloadCase::PluginStartup { .. }) {
            command.arg("--noplugin");
        }
        command.args(&config.extra_args);
        command
            .env_clear()
            .env("HOME", &config.home)
            .env("XDG_CONFIG_HOME", &config.xdg_config_home)
            .env("XDG_DATA_HOME", &config.xdg_data_home)
            .env("XDG_STATE_HOME", &config.xdg_state_home)
            .env("XDG_CACHE_HOME", &config.xdg_cache_home)
            .env("XDG_RUNTIME_DIR", &config.xdg_runtime_dir)
            .env("TMPDIR", &config.tmp_dir)
            .current_dir(&config.working_dir);

        // Apply the engine-specific runtime env.
        let (env_key, runtime_dir) = engine_runtime_env(engine);
        command.env(env_key, &runtime_dir);

        let status = command
            .status()
            .map_err(|source| side_error(format!("spawn failed: {source}")))?;
        if !status.success() {
            let code = status.code();
            return Err(side_error(format!("editor exited with status {code:?}")));
        }

        // Read the raw log.
        let log_bytes = fs::read_to_string(&log_path)
            .map_err(|source| side_error(format!("cannot read log: {source}")))?;

        // Parse with strict integer microsecond parsing.
        let log = startuptime::parse(&log_bytes)
            .map_err(|err| side_error(format!("parse failed: {err}")))?;

        // Require nonempty marks.
        if log.marks.is_empty() {
            return Err(side_error("parsed log contains no marks".to_owned()));
        }

        // Validate integer clock_us/delta_us (parser already produces u64).
        for (i, mark) in log.marks.iter().enumerate() {
            if mark.label.is_empty() {
                return Err(side_error(format!("mark {i} has an empty label")));
            }
        }

        Ok(StartuptimeRecord {
            engine: engine_label,
            workload,
            process_index,
            log,
            raw_log_path: log_path,
        })
    }
}

/// Require every process of one engine/workload group to share the same
/// mark label sequence, so cross-process stage comparison stays defined.
///
/// # Errors
///
/// Returns [`RunError::StartuptimeSide`] when two processes disagree.
fn require_consistent_mark_sequence(
    group: &[&StartuptimeRecord],
    workload: &str,
    family: EngineFamily,
) -> Result<(), RunError> {
    let Some(first) = group.first() else {
        return Ok(());
    };
    let first_labels: Vec<&str> = first.log.marks.iter().map(|m| m.label.as_str()).collect();
    for record in group.iter().skip(1) {
        let labels: Vec<&str> = record.log.marks.iter().map(|m| m.label.as_str()).collect();
        if labels != first_labels {
            return Err(RunError::StartuptimeSide {
                engine: engine_name(family),
                workload: workload.to_owned(),
                detail: format!(
                    "inconsistent mark label sequence across \
                     processes (expected {first_labels:?}, got {labels:?})"
                ),
            });
        }
    }
    Ok(())
}

/// The engine display name used in error contexts and artifact stems: the
/// lowercased `Debug` form of the engine family, matching the serialized
/// engine strings.
fn engine_name(family: EngineFamily) -> String {
    format!("{family:?}").to_lowercase()
}

/// Assemble the single startup sample for one startup-kind process: the
/// startup delta is the wall time, no response/flush split exists, and the
/// grid hash covers the rendered grid (or the no-UI sentinel).
fn startup_sample(
    cell: &WorkloadCell,
    run_id: &str,
    engine_label: EngineLabel,
    process_index: usize,
    startup_delta_us: u64,
    grid_hash: String,
    snapshot: Option<UiSnapshot>,
) -> CollectedSample {
    let engine = cell.family.to_engine();
    CollectedSample {
        sample: Sample {
            run_id: run_id.to_owned(),
            workload: cell.cell_id(),
            engine: engine_label,
            process_index,
            sample_index: 0,
            wall_us: startup_delta_us,
            response_us: None,
            flush_us: None,
            startup_delta_us: Some(startup_delta_us),
            grid_sha256: grid_hash,
            raw_source: format!("{engine:?}"),
        },
        snapshot,
    }
}

/// Validate the valgrind binary before any instrumented process spawns:
/// the path must be absolute and `--version` must succeed; its trimmed
/// stdout is the recorded profiler version.
///
/// # Errors
///
/// Returns [`RunError::ProfilerPreflight`] when the path is relative,
/// `--version` cannot be executed, or it exits nonzero.
fn preflight_valgrind(valgrind_path: &Path) -> Result<String, RunError> {
    if !valgrind_path.is_absolute() {
        return Err(RunError::ProfilerPreflight {
            path: valgrind_path.to_path_buf(),
            detail: "valgrind path is not absolute".to_owned(),
        });
    }

    // Preflight: --version must succeed.
    let version_output = Command::new(valgrind_path)
        .arg("--version")
        .output()
        .map_err(|source| RunError::ProfilerPreflight {
            path: valgrind_path.to_path_buf(),
            detail: format!("cannot execute --version: {source}"),
        })?;
    if !version_output.status.success() {
        return Err(RunError::ProfilerPreflight {
            path: valgrind_path.to_path_buf(),
            detail: format!(
                "--version exited with status {:?}",
                version_output.status.code()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .to_owned())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn duration_to_us(d: Duration) -> Result<u64, RunError> {
    d.as_micros()
        .try_into()
        .map_err(|source| RunError::IntegerConversion {
            context: "duration to microseconds",
            source,
        })
}

fn hash_grid(grid: &str) -> String {
    hex_digest(Sha256::digest(grid.as_bytes()))
}

fn file_sha256(path: &Path) -> Result<String, RunError> {
    let content = fs::read(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(hex_digest(Sha256::digest(&content)))
}

fn generate_run_id() -> Result<String, RunError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| RunError::SystemTime {
            context: "run ID generation",
            source,
        })?;
    let nanos = now.as_nanos();
    let digest = Sha256::digest(format!("{nanos}").as_bytes());
    let mut run_id = hex_digest(digest);
    run_id.truncate(16);
    Ok(run_id)
}

fn num_cpus() -> Result<usize, RunError> {
    thread::available_parallelism()
        .map(std::num::NonZero::get)
        .map_err(|source| RunError::CpuCount { source })
}

fn hostname() -> Result<String, RunError> {
    let path = Path::new("/etc/hostname");
    let content = fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let h = content.trim().to_owned();
    if h.is_empty() {
        Err(RunError::Hostname)
    } else {
        Ok(h)
    }
}

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let bytes = digest.as_ref();
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX_CHARS[(b >> 4) as usize] as char);
        s.push(HEX_CHARS[(b & 0xf) as usize] as char);
    }
    s
}

/// The current Unix time in whole seconds.
///
/// # Errors
///
/// Returns [`RunError::SystemTime`] when the clock is before the Unix epoch.
fn unix_seconds_now() -> Result<u64, RunError> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| RunError::SystemTime {
            context: "manifest started time",
            source,
        })?;
    Ok(since_epoch.as_secs())
}

/// Combine a primary measurement failure with the outcome of the shutdown
/// that follows it, so neither failure masks the other.
fn shutdown_alongside(primary: RunError, shutdown: Result<(), PerfError>) -> RunError {
    match shutdown {
        Ok(()) => primary,
        Err(shutdown) => RunError::Lifecycle {
            primary: Box::new(primary),
            shutdown: Box::new(RunError::Session(shutdown)),
        },
    }
}

/// Bind an I/O error to the artifact path it occurred on.
fn json_write_error(path: &Path) -> impl Fn(std::io::Error) -> RunError + '_ {
    move |source| RunError::JsonWrite {
        path: path.to_path_buf(),
        source,
    }
}

/// Encode `value` as JSON and write it to `path`, newline-terminated.
///
/// # Errors
///
/// Returns [`RunError::JsonEncode`] when serialization fails and
/// [`RunError::JsonWrite`] when the file cannot be created, written, or
/// flushed.
fn write_artifact_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RunError> {
    let bytes = sonic_rs::to_vec(value).map_err(|source| RunError::JsonEncode {
        path: path.to_path_buf(),
        source,
    })?;
    let file = File::create(path).map_err(json_write_error(path))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(&bytes).map_err(json_write_error(path))?;
    writer.write_all(b"\n").map_err(json_write_error(path))?;
    writer.flush().map_err(json_write_error(path))?;
    Ok(())
}

/// Encode each record as one JSON line (NDJSON) and write them to `path`.
///
/// Every record is immediately followed by a newline; the stream is flushed
/// before the call returns.
///
/// # Errors
///
/// Returns [`RunError::JsonEncode`] when serialization fails and
/// [`RunError::JsonWrite`] when the file cannot be created, written, or
/// flushed.
fn write_artifact_ndjson<T: Serialize>(path: &Path, records: &[T]) -> Result<(), RunError> {
    let file = File::create(path).map_err(json_write_error(path))?;
    let mut writer = BufWriter::new(file);
    for record in records {
        let bytes = sonic_rs::to_vec(record).map_err(|source| RunError::JsonEncode {
            path: path.to_path_buf(),
            source,
        })?;
        writer.write_all(&bytes).map_err(json_write_error(path))?;
        writer.write_all(b"\n").map_err(json_write_error(path))?;
    }
    writer.flush().map_err(json_write_error(path))?;
    Ok(())
}

/// Assemble the typed manifest for one completed run.
///
/// The manifest states the measurement channels explicitly: latency and peak
/// RSS come from direct sessions; the allocation channel is DHAT exactly when
/// a resolved profiler is supplied. A calibration run records no candidate or
/// profiler source.
fn build_run_manifest(
    contract: &MeasurementContract,
    run_id: &str,
    mode: ManifestMode,
    started_unix: u64,
    calibration: Option<&CalibrationSource>,
    valgrind: Option<ProfilerProvenance>,
) -> RunManifest {
    RunManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        run_id: run_id.to_owned(),
        mode,
        started_unix,
        contract: contract.clone(),
        engines: ManifestEngines {
            oracle: contract.oracle.clone(),
            candidate: contract.candidate.clone(),
        },
        channels: ManifestChannels {
            latency: MeasurementChannel::Direct,
            peak_rss: MeasurementChannel::Direct,
            allocations: valgrind.as_ref().map(|profiler| MeasurementChannel::Dhat {
                profiler: profiler.clone(),
            }),
        },
        calibration: calibration.cloned(),
        valgrind,
    }
}

/// Collect one workload's `(process, sample) -> entry` mapping from a
/// `(workload, process, sample)` index, carrying the entry reference
/// lifetime through the borrow of the index's stored values.
fn pair_samples<'a>(
    index: &HashMap<(&'a str, usize, usize), &'a CollectedSample>,
    workload: &str,
) -> BTreeMap<(usize, usize), &'a CollectedSample> {
    index
        .iter()
        .filter(|((name, ..), _)| *name == workload)
        .map(|((_, process, sample), entry)| ((*process, *sample), *entry))
        .collect()
}

/// Pair left/right samples exactly by (workload, process, sample) and record
/// every grid-hash mismatch with both actual snapshots.
///
/// The mismatch rule is exactly the report's: `Sample::grid_sha256`. The
/// pairing is exact — unequal key sets and duplicate samples are typed
/// errors, never silent drops.
///
/// # Errors
///
/// Returns [`RunError::Report`] when the two sides do not cover identical
/// keys, a sample is duplicated, or the walk overflows.
fn compute_divergences(
    run_id: &str,
    left_engine: EngineLabel,
    right_engine: EngineLabel,
    collected: &[CollectedSample],
) -> Result<DivergencesArtifact, RunError> {
    let mut workloads: BTreeSet<&str> = BTreeSet::new();
    let mut left_index: HashMap<(&str, usize, usize), &CollectedSample> = HashMap::new();
    let mut right_index: HashMap<(&str, usize, usize), &CollectedSample> = HashMap::new();
    for entry in collected {
        let sample = &entry.sample;
        workloads.insert(sample.workload.as_str());
        let key = (
            sample.workload.as_str(),
            sample.process_index,
            sample.sample_index,
        );
        let index = if sample.engine == left_engine {
            &mut left_index
        } else if sample.engine == right_engine {
            &mut right_index
        } else {
            continue;
        };
        if index.insert(key, entry).is_some() {
            return Err(RunError::Report(ReportError::InvalidContract {
                reason: format!(
                    "duplicate {engine:?} sample for {workload}/{}/{}",
                    key.1,
                    key.2,
                    engine = sample.engine,
                    workload = key.0,
                ),
            }));
        }
    }

    let mut records = Vec::new();
    let mut compared = 0usize;
    let mut first_mismatch_ordinal = None;
    for workload in workloads {
        let left = pair_samples(&left_index, workload);
        let right = pair_samples(&right_index, workload);
        let coverage = |got: usize| {
            RunError::Report(ReportError::MissingCoverage {
                artifact: "divergence-pairing".to_owned(),
                workload: workload.to_owned(),
                engine: right_engine,
                got,
                want: left.len(),
            })
        };
        if left.len() != right.len() {
            return Err(coverage(right.len()));
        }
        for (key, left_entry) in &left {
            let right_entry = right.get(key).ok_or_else(|| coverage(right.len()))?;
            let ordinal = compared;
            compared = compared.checked_add(1).ok_or_else(|| {
                RunError::Report(ReportError::ArithmeticOverflow {
                    field: "divergence walk".to_owned(),
                })
            })?;
            if left_entry.sample.grid_sha256 != right_entry.sample.grid_sha256 {
                if first_mismatch_ordinal.is_none() {
                    first_mismatch_ordinal = Some(ordinal);
                }
                records.push(DivergenceRecord {
                    run_id: run_id.to_owned(),
                    workload: workload.to_owned(),
                    process_index: key.0,
                    sample_index: key.1,
                    left_engine,
                    right_engine,
                    left_grid_sha256: left_entry.sample.grid_sha256.clone(),
                    right_grid_sha256: right_entry.sample.grid_sha256.clone(),
                    left_snapshot: left_entry.snapshot.as_ref().map(SnapshotRecord::capture),
                    right_snapshot: right_entry.snapshot.as_ref().map(SnapshotRecord::capture),
                });
            }
        }
    }

    Ok(DivergencesArtifact {
        schema_version: DIVERGENCES_SCHEMA_VERSION,
        run_id: run_id.to_owned(),
        left_engine,
        right_engine,
        compared,
        mismatched: records.len(),
        first_mismatch_ordinal,
        records,
    })
}
/// Load and gate the noise calibration for a comparison run.
///
/// Reads the artifact bytes, decodes a typed [`NoiseRunSummary`], and rejects
/// any calibration that cannot back a run under `contract` — wrong schema,
/// empty run identity, contract mismatch, or a failed noise ceiling verdict.
/// The source identity (path, SHA-256, run id) is returned for the manifest
/// writer.
///
/// # Errors
///
/// Returns [`RunError::Io`] when the artifact or its hash cannot be read,
/// [`RunError::NoiseDecode`] when the JSON does not decode, and
/// [`RunError::NoiseIncompatible`] when the calibration cannot back a run.
fn load_noise_calibration(
    path: &Path,
    contract: &MeasurementContract,
) -> Result<(NoiseRunSummary, CalibrationSource), RunError> {
    // Kept in lockstep with report::NOISE_SCHEMA_VERSION.
    const NOISE_SCHEMA_VERSION: u32 = 1;

    let bytes = fs::read(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let calibration: NoiseRunSummary =
        sonic_rs::from_slice(&bytes).map_err(|source| RunError::NoiseDecode {
            path: path.to_path_buf(),
            source,
        })?;

    let source = CalibrationSource {
        path: path.to_path_buf(),
        sha256: fixture::hash_file(path).map_err(RunError::from)?,
        run_id: calibration.run_id.clone(),
    };
    debug_assert_eq!(source.sha256.len(), 64, "hex digest length drifted");

    let reject = |reason: String| RunError::NoiseIncompatible {
        path: source.path.clone(),
        reason,
    };
    if calibration.schema_version != NOISE_SCHEMA_VERSION {
        return Err(reject(format!(
            "noise schema version {} is not {NOISE_SCHEMA_VERSION}",
            calibration.schema_version
        )));
    }
    if source.run_id.is_empty() {
        return Err(reject("calibration run id is empty".to_owned()));
    }
    if calibration.contract != *contract {
        return Err(reject(
            "measurement contract mismatch: the calibration was produced \
             under a different contract"
                .to_owned(),
        ));
    }
    if let NoiseVerdict::Fail(rejections) = &calibration.verdict {
        return Err(reject(format!(
            "calibration ceiling verdict failed: {rejections:?}"
        )));
    }

    Ok((calibration, source))
}

/// Return the engine-specific runtime environment variable name and path.
///
/// Mirrors [`Engine::runtime_env`] from the session module, which is private.
/// The runner needs this for the `--startuptime` side pass, which spawns the
/// editor directly rather than through [`PerfSession`].
fn engine_runtime_env(engine: Engine) -> (&'static str, PathBuf) {
    match engine {
        Engine::Neovim => (
            "VIMRUNTIME",
            crate::root().join(".references/neovim/runtime"),
        ),
        Engine::Oxvim => ("OXVIM_RUNTIME", crate::root().join("runtime")),
    }
}

/// The report contract identity of one engine binary.
///
/// # Errors
///
/// Returns [`RunError::Io`] when the binary cannot be read or hashed.
fn contract_file_identity(path: &Path) -> Result<ReportFileIdentity, RunError> {
    Ok(ReportFileIdentity {
        path: path.display().to_string(),
        sha256: file_sha256(path)?,
    })
}

/// The content-only fixture hashes recorded in one cell's contract.
///
/// `FixtureArguments::None` carries no fixture file; its identity pins the
/// empty content so every cell still carries a deterministic hash. Paths are
/// provenance and never enter the contract: two calibrations over identical
/// fixture bytes are interchangeable wherever the bytes live.
fn fixture_identities(fixture: &PreparedFixture) -> Vec<String> {
    match &fixture.arguments {
        FixtureArguments::None => vec![hex_digest(Sha256::digest(b""))],
        FixtureArguments::LargeBuffer { .. } | FixtureArguments::PluginTree { .. } => {
            match &fixture.sha256 {
                Some(sha256) => vec![sha256.clone()],
                // prepare_matrix hashed every file-backed fixture; an empty
                // list fails contract validation loudly instead of panicking.
                None => Vec::new(),
            }
        }
    }
}

/// The deterministic input description recorded in one cell's contract.
fn input_definition(id: WorkloadId) -> String {
    match id {
        WorkloadId::Startup { plugin_count } => format!(
            "nvim startup loading {plugin_count} plugin/*.lua scripts from \
             the generated plugin tree"
        ),
        WorkloadId::Input => "nvim_input \"ix<Esc>x\" to trailing flush".to_owned(),
        WorkloadId::LuaPure => "nvim_exec_lua pure LuaJIT computation".to_owned(),
        WorkloadId::LuaApi => "nvim_exec_lua calling vim.api".to_owned(),
        WorkloadId::Open => "large-buffer open to first complete initial flush".to_owned(),
        WorkloadId::Edit => "large-buffer midpoint line replacement to trailing flush".to_owned(),
        WorkloadId::Scroll => "large-buffer five-line scroll to trailing flush".to_owned(),
    }
}

/// The stage fields every sample of a cell must carry, matching how
/// `run_startup_process` and `run_steady_state_process` stamp samples.
fn stage_contract(id: WorkloadId) -> StageContract {
    match id {
        WorkloadId::Startup { .. } | WorkloadId::Open => StageContract::Startup,
        WorkloadId::LuaPure | WorkloadId::LuaApi => StageContract::SteadyState {
            response: true,
            flush: false,
        },
        WorkloadId::Input | WorkloadId::Edit | WorkloadId::Scroll => StageContract::SteadyState {
            response: true,
            flush: true,
        },
    }
}

/// A structured "expected host fact absent" I/O error.
fn missing_host_fact(path: &Path, field: &str) -> RunError {
    RunError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, format!("{field} not found")),
    }
}

/// A structured "run identity absent for this mode" I/O error.
fn missing_run_identity(output_root: &Path, what: &str) -> RunError {
    RunError::Io {
        path: output_root.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{what} was not created for this mode"),
        ),
    }
}

/// The kernel release from `/proc/sys/kernel/osrelease`.
///
/// # Errors
///
/// Returns [`RunError::Io`] when the file cannot be read or the release is
/// absent.
fn kernel_release() -> Result<String, RunError> {
    let path = Path::new("/proc/sys/kernel/osrelease");
    let content = fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let release = content.trim().to_owned();
    if release.is_empty() {
        return Err(missing_host_fact(path, "kernel release"));
    }
    Ok(release)
}

/// The first `model name` entry from `/proc/cpuinfo`.
///
/// # Errors
///
/// Returns [`RunError::Io`] when the file cannot be read or no model name
/// is present.
fn cpu_model() -> Result<String, RunError> {
    let path = Path::new("/proc/cpuinfo");
    let content = fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    for line in content.lines() {
        if let Some(model) = line.strip_prefix("model name")
            && let Some(value) = model.trim_start().strip_prefix(':')
        {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_owned());
            }
        }
    }
    Err(missing_host_fact(path, "cpu model name"))
}

/// Total physical memory in KiB from `MemTotal` in `/proc/meminfo`.
///
/// # Errors
///
/// Returns [`RunError::Io`] when the file cannot be read or `MemTotal` is
/// absent or malformed.
fn memory_total_kib() -> Result<u64, RunError> {
    let path = Path::new("/proc/meminfo");
    let content = fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    for line in content.lines() {
        // `MemTotal:       791180808 kB` — drop the separator before the
        // value, mirroring `cpu_model`, or the first field parses as ":".
        let Some(entry) = line
            .strip_prefix("MemTotal")
            .and_then(|rest| rest.trim_start().strip_prefix(':'))
        else {
            continue;
        };
        let mut fields = entry.split_whitespace();
        let Some(value) = fields.next().and_then(|raw| raw.parse::<u64>().ok()) else {
            continue;
        };
        if matches!(fields.next(), Some("kB") | None) {
            return Ok(value);
        }
    }
    Err(missing_host_fact(path, "MemTotal"))
}
