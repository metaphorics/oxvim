//! External comparative-performance harness substrate.
//!
//! This module measures Oxvim against the pinned Neovim oracle from *outside*
//! both binaries: it spawns them over `--embed`, timestamps decoded RPC
//! messages in a dedicated reader thread, applies redraw batches to the
//! existing [`ox_tui::TuiState`] screen model, and reads Linux `VmHWM` for
//! child peak RSS. It never calls into Oxvim internals and never activates
//! Criterion or `--startuptime` inside a timed session (startup logs come
//! from a separate side pass); see `.outline/sdd/plugin-probe.md` (isolation
//! discipline only — its historical defect list is not an oracle) and issue
//! #11 for the normative paired-session contract.
//!
//! # Measurement channels
//!
//! Latency and peak RSS are direct measurements from uninstrumented sessions:
//! one timed RPC window and one `VmHWM` read per process. Allocation totals
//! are a separate pass: comparison mode re-runs every selected process under
//! `valgrind --tool=dhat` through [`ProcessLauncher::Prefix`], discards the
//! instrumented process's timing and RSS, and keeps only whole-process DHAT
//! heap totals decoded by [`decode_dhat`]. Noise calibration runs no DHAT
//! pass. The run manifest records which channels each mode populated.
//!
//! # Modes and artifacts
//!
//! [`ExecutionMode`] selects the run shape:
//!
//! - [`ExecutionMode::DryRun`] expands the workload matrix deterministically
//!   and returns a [`DryRunPlan`]: no engines spawn, no fixtures are
//!   materialised, no artifacts are written, and no run ID or verdict exists.
//! - [`ExecutionMode::NoiseCalibration`] runs the oracle-vs-oracle matrix
//!   (Neovim on both sides) and produces a [`NoiseRunSummary`] tagged
//!   [`RunSummary::NoiseCalibration`].
//! - [`ExecutionMode::Comparison`] is the default measured mode. It loads a
//!   prior calibration artifact and an absolute `valgrind` path (the run is
//!   rejected before any process spawns without them), always runs both
//!   engines in direct ABBA collection, always performs the separate DHAT
//!   allocation pass, and produces a [`RunSummary::Comparison`] with a [`Verdict`].
//!
//! A completed run writes typed artifacts under `target/perf/<run_id>/`:
//!
//! | File | Contents |
//! |------|----------|
//! | `manifest.json` | mode, contract, engine identities, measurement channels, calibration/profiler provenance |
//! | `samples.ndjson` | one [`Sample`] per line, in production order |
//! | `processes.ndjson` | one [`ProcessRun`] per line |
//! | `allocations.ndjson` | one [`AllocationRun`] per line (comparison only) |
//! | `noise.json` | [`NoiseRunSummary`] (calibration mode; comparison copies its validated calibration) |
//! | `summary.json` | [`RunSummary`] tagged by mode |
//! | `divergences.json` | mismatch records preserving both actual UI snapshots |
//! | `stages.json` | aggregated startup and steady-state stage artifact from [`report::summarize_stages`] (comparison) |
//! | `startuptime.json` | parsed startup logs or explicit not-collected (comparison) |
//! | `startuptime/`, `allocations/` | raw engine logs and DHAT outputs, referenced by hash |
//!
//! # Module inventory
//!
//! - [`session`]: owned editor processes via [`ProcessLauncher`], fresh
//!   `HOME`/`XDG_*`/`TMPDIR` isolation, timestamped RPC decode, the setup
//!   fence, the `Spawned`/`Attached`/`Ready` typestate ladder, snapshot
//!   capture, and `/proc/<pid>/status` `VmHWM` peak-RSS reading.
//! - [`fixture`]: idempotent content-hashed `large.txt` and plugin-tree
//!   generation below `target/perf`.
//! - [`workload`]: the seven workload type names ([`workload::WorkloadId`]),
//!   profiles ([`workload::Profile`]), per-cell
//!   [`workload::FixtureRequirement`]s and resolved
//!   [`workload::FixtureArguments`], the [`workload::matrix`], request and
//!   alternation builders, steady-state window driving, and ABBA ordering.
//! - [`stats`]: nearest-rank percentiles and [`stats::Percentiles`]
//!   summarization.
//! - [`startuptime`]: strict integer-microsecond `--startuptime` parser for
//!   both engine headers ([`startuptime::StartupLog`], [`startuptime::parse`]).
//! - [`allocation`]: typed decoder for Valgrind DHAT heap totals
//!   ([`decode_dhat`], [`allocation::DhatTotals`]).
//! - [`report`]: measurement/stage contracts, noise calibration and
//!   allocation summaries, verdict rules and thresholds, and the
//!   [`report::materiality_open`], [`report::summarize_noise`],
//!   [`report::judge_comparison`], and [`report::summarize_stages`]
//!   builders.
//! - [`runner`]: [`ExecutionMode`] dispatch, profiles and filters, direct
//!   ABBA collection, the separate DHAT allocation pass, the startup side
//!   pass, dry-run planning, and artifact orchestration ([`runner::Runner`]).

pub mod allocation;
pub mod fixture;
pub mod report;
pub mod runner;
pub mod session;
pub mod startuptime;
pub mod stats;
pub mod workload;

pub use allocation::{DhatError, DhatTotals, decode_dhat};
pub use report::{
    AllocationRun, AllocationSummary, AlternationOrder, CellContract, ComparisonRunSummary,
    EngineLabel, ExperimentOpening, FileIdentity, HostFingerprint, LatencyKind,
    MeasurementContract, NoiseRunSummary, NoiseVerdict, ProcessRun, Rejection, ReportError,
    RunSummary, Sample, StageArtifactRow, StageContract, StageProvenance, StagesArtifact,
    StartupStageMark, StartupStageRun, StartupStages, Thresholds, Verdict, WorkloadNoise,
    WorkloadSummary, judge_comparison, materiality_open, summarize_noise, summarize_stages,
};
pub use runner::{DryRunCell, DryRunPlan, ExecutionMode, RunConfig, RunError, RunOutcome};
pub use session::ProcessLauncher;
pub use startuptime::{Mark, ParseError, StartupLog, parse};
pub use stats::{Percentiles, StatsError, percentile, summarize};
pub use workload::{
    FixtureArguments, FixtureRequirement, Profile, WorkloadCell, WorkloadId, abba_block,
    abba_order, edit_alternation, matrix, request_params, resolve_fixture_arguments,
    run_steady_window, scroll_alternation,
};
