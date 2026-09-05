//! Typed performance artifacts, calibration summaries, and comparison verdicts.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::stats::{Percentiles, StatsError, summarize};
use super::workload::Profile;

const NOISE_SCHEMA_VERSION: u32 = 1;
const STAGES_SCHEMA_VERSION: u32 = 1;
const MATERIALITY_FLOOR_US: u64 = 150;
const STAGES_NOTES: &str = "Stage percentiles are per-stage marginals: response, flush, and \
startup-stage values are not additive components of wall time and are never published as \
cross-engine ratios; wall time is the cross-engine metric.";

/// Closed labels for measured engines and calibration arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineLabel {
    Neovim,
    Oxvim,
    NoiseA,
    NoiseB,
}

impl EngineLabel {
    fn is_comparison(self) -> bool {
        matches!(self, Self::Neovim | Self::Oxvim)
    }
}

/// One timed measurement window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub run_id: String,
    pub workload: String,
    pub engine: EngineLabel,
    pub process_index: usize,
    pub sample_index: usize,
    pub wall_us: u64,
    pub response_us: Option<u64>,
    pub flush_us: Option<u64>,
    pub startup_delta_us: Option<u64>,
    pub grid_sha256: String,
    pub raw_source: String,
}

/// Measurements collected from one completed child process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRun {
    pub run_id: String,
    pub workload: String,
    pub engine: EngineLabel,
    pub process_index: usize,
    pub pid: u32,
    pub peak_rss_kib: u64,
    pub warmup_discarded: usize,
    pub samples_recorded: usize,
}

/// Identity of a file whose bytes participate in the measurement contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub path: String,
    pub sha256: String,
}

/// Host facts which must match between calibration and comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFingerprint {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub architecture: String,
    pub cpu: String,
    pub logical_cpus: usize,
    pub memory_kib: u64,
}

/// UI shape fixed for all selected cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContract {
    pub width: u16,
    pub height: u16,
    pub rgb: bool,
    pub ext_linegrid: bool,
}

/// Alternation applied to the two arms represented by each run mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlternationOrder {
    AlternatingFirstArm,
    AlternatingSecondArm,
}

/// Stage fields required for every sample in a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum StageContract {
    Startup,
    SteadyState { response: bool, flush: bool },
}

/// Exact process, sample, timeout, input, fixture, and stage contract for one cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellContract {
    pub workload: String,
    pub processes_per_engine: usize,
    pub samples_per_process: usize,
    pub warmup_per_process: usize,
    pub timeout_ms: u64,
    pub wall_limit_ms: u64,
    pub allocation_runs_per_engine: usize,
    pub input_definition: String,
    pub stage_contract: StageContract,
    pub fixture_hashes: Vec<String>,
}

/// Complete comparability contract shared by calibration and comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementContract {
    pub schema_version: u32,
    pub profile: Profile,
    pub release_harness_profile: String,
    pub host: HostFingerprint,
    pub oracle: FileIdentity,
    pub candidate: FileIdentity,
    pub ui: UiContract,
    pub order: AlternationOrder,
    pub cells: Vec<CellContract>,
}

/// Fixed and noise-adaptive decision thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub noise_lat_ceiling: f64,
    pub noise_rss_ceiling: f64,
    pub lat_warn_floor: f64,
    pub lat_fail: f64,
    pub max_fail: f64,
    pub rss_warn: f64,
    pub rss_fail: f64,
    pub absolute_mode_below_us: u64,
    pub absolute_warn_us: u64,
    pub absolute_fail_us: u64,
}

impl Thresholds {
    /// Construct the ticket #18 thresholds after validating every operand.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError`] if any fixed threshold is non-finite, negative, or internally
    /// inconsistent.
    pub fn ticket_18() -> Result<Self, ReportError> {
        let thresholds = Self {
            noise_lat_ceiling: 0.10,
            noise_rss_ceiling: 0.05,
            lat_warn_floor: 1.15,
            lat_fail: 2.00,
            max_fail: 3.00,
            rss_warn: 1.25,
            rss_fail: 1.50,
            absolute_mode_below_us: 50,
            absolute_warn_us: 150,
            absolute_fail_us: 500,
        };
        thresholds.validate()?;
        Ok(thresholds)
    }

    fn validate(self) -> Result<(), ReportError> {
        for (field, value) in [
            ("noise_lat_ceiling", self.noise_lat_ceiling),
            ("noise_rss_ceiling", self.noise_rss_ceiling),
            ("lat_warn_floor", self.lat_warn_floor),
            ("lat_fail", self.lat_fail),
            ("max_fail", self.max_fail),
            ("rss_warn", self.rss_warn),
            ("rss_fail", self.rss_fail),
        ] {
            valid_nonnegative(field, value)?;
        }
        if self.lat_warn_floor < 1.0
            || self.lat_fail < self.lat_warn_floor
            || self.max_fail < self.lat_fail
            || self.rss_warn < 1.0
            || self.rss_fail < self.rss_warn
            || self.absolute_mode_below_us == 0
            || self.absolute_warn_us == 0
            || self.absolute_fail_us < self.absolute_warn_us
        {
            return Err(ReportError::InvalidThresholds);
        }
        Ok(())
    }

    fn adaptive_warn(self, noise_lat: f64) -> Result<f64, ReportError> {
        valid_nonnegative("noise_lat", noise_lat)?;
        Ok((1.0 + 3.0 * noise_lat).max(self.lat_warn_floor))
    }
}

/// Why report construction could not produce a comparable completed artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum ReportError {
    EmptyField {
        field: String,
    },
    InvalidContract {
        reason: String,
    },
    InvalidThresholds,
    InvalidNumber {
        field: String,
    },
    WrongEngine {
        workload: String,
        engine: EngineLabel,
    },
    WrongRunId {
        artifact: String,
        expected: String,
        actual: String,
    },
    UnknownWorkload {
        workload: String,
    },
    DuplicateRecord {
        artifact: String,
        workload: String,
        engine: EngineLabel,
        process_index: usize,
        sample_index: Option<usize>,
    },
    MissingCoverage {
        artifact: String,
        workload: String,
        engine: EngineLabel,
        got: usize,
        want: usize,
    },
    InvalidProcessRecord {
        workload: String,
        engine: EngineLabel,
        process_index: usize,
        reason: String,
    },
    InvalidSampleRecord {
        workload: String,
        engine: EngineLabel,
        process_index: usize,
        sample_index: usize,
        reason: String,
    },
    InvalidAllocationRecord {
        workload: String,
        engine: EngineLabel,
        process_index: usize,
        reason: String,
    },
    Percentiles {
        workload: String,
        metric: String,
        reason: String,
    },
    ZeroDenominator {
        workload: String,
        metric: String,
    },
    IncompatibleCalibration {
        reason: String,
    },
    ArithmeticOverflow {
        field: String,
    },
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ReportError {}

/// The latency rule that rejected a workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyKind {
    P95,
    Max,
    Absolute,
}

/// One performance rejection. Structural failures are [`ReportError`] instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rejection", rename_all = "snake_case")]
pub enum Rejection {
    NoisyHost {
        workload: String,
        metric: String,
        measured: f64,
        ceiling: f64,
    },
    Incomparable {
        workload: String,
        mismatched: usize,
        first_mismatch: usize,
    },
    Latency {
        workload: String,
        ratio: f64,
        threshold: f64,
        kind: LatencyKind,
    },
    Memory {
        workload: String,
        ratio: f64,
        threshold: f64,
    },
}

/// Overall or per-workload performance verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "details", rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Warn(Vec<String>),
    Fail(Vec<Rejection>),
}

/// Pass/fail ceiling decision for a completed noise calibration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "details", rename_all = "snake_case")]
pub enum NoiseVerdict {
    Pass,
    Fail(Vec<Rejection>),
}

/// Noise measured for one workload from exact A/B distributions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadNoise {
    pub run_id: String,
    pub workload: String,
    pub latency_a: Percentiles,
    pub latency_b: Percentiles,
    pub rss_a: Percentiles,
    pub rss_b: Percentiles,
    pub noise_lat: f64,
    pub noise_rss: f64,
    pub verdict: NoiseVerdict,
}

/// Completed noise-calibration summary and reusable comparison input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoiseRunSummary {
    pub schema_version: u32,
    pub run_id: String,
    pub profile: Profile,
    pub contract: MeasurementContract,
    pub thresholds: Thresholds,
    pub workloads: Vec<WorkloadNoise>,
    pub verdict: NoiseVerdict,
}

/// One whole-process Valgrind DHAT allocation record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationRun {
    pub run_id: String,
    pub workload: String,
    pub engine: EngineLabel,
    pub process_index: usize,
    pub total_blocks: u64,
    pub total_bytes: u64,
    pub raw_files: Vec<FileIdentity>,
}

/// Whole-process allocation distributions for both comparison engines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationSummary {
    pub run_id: String,
    pub workload: String,
    pub neovim_blocks: Percentiles,
    pub oxvim_blocks: Percentiles,
    pub neovim_bytes: Percentiles,
    pub oxvim_bytes: Percentiles,
    pub raw_runs: Vec<AllocationRun>,
}

/// Machine-readable decision for opening an optimization experiment.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExperimentOpening {
    pub stage_p95_us: u64,
    pub wall_p95_us: u64,
    pub share: f64,
    pub noise_lat: f64,
    pub minimum_share: f64,
    pub minimum_candidate_gain: f64,
    pub material: bool,
}

/// Stage sample copied into the stage artifact for exact accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageProvenance {
    pub process_index: usize,
    pub sample_index: usize,
    pub wall_us: u64,
    pub response_us: Option<u64>,
    pub flush_us: Option<u64>,
    pub startup_delta_us: Option<u64>,
    pub raw_source: String,
}

/// Typed steady-state stage row; response and flush are never cross-engine ratios.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum StageArtifactRow {
    SteadyState {
        run_id: String,
        workload: String,
        engine: EngineLabel,
        wall: Percentiles,
        response: Option<Percentiles>,
        flush: Option<Percentiles>,
        response_materiality: Option<ExperimentOpening>,
        flush_materiality: Option<ExperimentOpening>,
        raw_samples: Vec<StageProvenance>,
    },
}

/// One startup stage mark copied from a parsed startup log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupStageMark {
    pub ordinal: usize,
    pub label: String,
    pub delta_us: u64,
    pub clock_us: u64,
}

/// One parsed startup log reduced to typed marks for stage aggregation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupStageRun {
    pub run_id: String,
    pub workload: String,
    pub engine: EngineLabel,
    pub process_index: usize,
    pub raw_source: String,
    pub marks: Vec<StartupStageMark>,
}

/// Startup stages aggregated for one workload, engine, and mark ordinal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartupStageRow {
    pub run_id: String,
    pub workload: String,
    pub engine: EngineLabel,
    pub ordinal: usize,
    pub label: String,
    pub delta: Percentiles,
    pub whole_startup: Percentiles,
    pub materiality: ExperimentOpening,
}

/// Startup rows in the stage artifact: aggregated, or explicitly absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StartupStages {
    NotCollected,
    Collected { rows: Vec<StartupStageRow> },
}

/// Complete stage artifact for one comparison run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagesArtifact {
    pub schema_version: u32,
    pub run_id: String,
    pub startup: StartupStages,
    pub steady_state: Vec<StageArtifactRow>,
    pub notes: String,
}

/// One workload's complete comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadSummary {
    pub run_id: String,
    pub workload: String,
    pub neovim: Percentiles,
    pub oxvim: Percentiles,
    pub neovim_rss: Percentiles,
    pub oxvim_rss: Percentiles,
    pub allocations: AllocationSummary,
    pub stages: Vec<StageArtifactRow>,
    pub lat_ratio: f64,
    pub lat_max_ratio: f64,
    pub rss_ratio: f64,
    pub noise_lat: f64,
    pub noise_rss: f64,
    pub threshold_warn: f64,
    pub mismatched_samples: usize,
    pub verdict: Verdict,
}

/// Completed comparison summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonRunSummary {
    pub run_id: String,
    pub profile: Profile,
    pub contract: MeasurementContract,
    pub calibration_run_id: String,
    pub verdict: Verdict,
    pub workloads: Vec<WorkloadSummary>,
}

/// One completed measured run, tagged by its mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "summary", rename_all = "snake_case")]
pub enum RunSummary {
    NoiseCalibration(NoiseRunSummary),
    Comparison(ComparisonRunSummary),
}

/// Evaluate the experiment-opening rule.
///
/// # Errors
///
/// Returns [`ReportError`] when `noise_lat` is invalid, `wall_p95_us` is zero, or the computed
/// stage share is not finite.
pub fn materiality_open(
    stage_p95_us: u64,
    wall_p95_us: u64,
    noise_lat: f64,
) -> Result<ExperimentOpening, ReportError> {
    valid_nonnegative("noise_lat", noise_lat)?;
    if wall_p95_us == 0 {
        return Err(ReportError::ZeroDenominator {
            workload: "materiality".to_owned(),
            metric: "wall_p95_us".to_owned(),
        });
    }
    let share = decimal_f64(stage_p95_us) / decimal_f64(wall_p95_us);
    if !share.is_finite() {
        return Err(ReportError::InvalidNumber {
            field: "share".to_owned(),
        });
    }
    let minimum_share = (3.0 * noise_lat).max(0.10);
    let minimum_candidate_gain = (3.0 * noise_lat).max(0.15);
    Ok(ExperimentOpening {
        stage_p95_us,
        wall_p95_us,
        share,
        noise_lat,
        minimum_share,
        minimum_candidate_gain,
        material: stage_p95_us >= MATERIALITY_FLOOR_US && share >= minimum_share,
    })
}

/// Summarize exact NoiseA/NoiseB coverage into a typed calibration result.
///
/// # Errors
///
/// Returns [`ReportError`] when the run, contract, coverage, measurements, or derived noise
/// statistics are invalid.
pub fn summarize_noise(
    run_id: &str,
    contract: &MeasurementContract,
    samples: &[Sample],
    processes: &[ProcessRun],
) -> Result<NoiseRunSummary, ReportError> {
    validate_run_id(run_id)?;
    validate_contract(contract)?;
    let thresholds = Thresholds::ticket_18()?;
    validate_coverage(
        run_id,
        contract,
        samples,
        processes,
        [EngineLabel::NoiseA, EngineLabel::NoiseB],
    )?;

    let mut workloads = Vec::with_capacity(contract.cells.len());
    let mut all_failures = Vec::new();
    for cell in &contract.cells {
        let latency_a = sample_percentiles(
            samples,
            &cell.workload,
            EngineLabel::NoiseA,
            |sample| sample.wall_us,
            "latency_a",
        )?;
        let latency_b = sample_percentiles(
            samples,
            &cell.workload,
            EngineLabel::NoiseB,
            |sample| sample.wall_us,
            "latency_b",
        )?;
        let rss_a = process_percentiles(processes, &cell.workload, EngineLabel::NoiseA)?;
        let rss_b = process_percentiles(processes, &cell.workload, EngineLabel::NoiseB)?;
        let noise_lat =
            symmetric_spread(latency_a.p95, latency_b.p95, &cell.workload, "noise_lat")?;
        let noise_rss = symmetric_spread(rss_a.p95, rss_b.p95, &cell.workload, "noise_rss")?;
        let mut failures = Vec::new();
        if noise_lat > thresholds.noise_lat_ceiling {
            failures.push(Rejection::NoisyHost {
                workload: cell.workload.clone(),
                metric: "latency".to_owned(),
                measured: noise_lat,
                ceiling: thresholds.noise_lat_ceiling,
            });
        }
        if noise_rss > thresholds.noise_rss_ceiling {
            failures.push(Rejection::NoisyHost {
                workload: cell.workload.clone(),
                metric: "rss".to_owned(),
                measured: noise_rss,
                ceiling: thresholds.noise_rss_ceiling,
            });
        }
        let verdict = if failures.is_empty() {
            NoiseVerdict::Pass
        } else {
            NoiseVerdict::Fail(failures.clone())
        };
        all_failures.extend(failures);
        workloads.push(WorkloadNoise {
            run_id: run_id.to_owned(),
            workload: cell.workload.clone(),
            latency_a,
            latency_b,
            rss_a,
            rss_b,
            noise_lat,
            noise_rss,
            verdict,
        });
    }
    let verdict = if all_failures.is_empty() {
        NoiseVerdict::Pass
    } else {
        NoiseVerdict::Fail(all_failures)
    };
    Ok(NoiseRunSummary {
        schema_version: NOISE_SCHEMA_VERSION,
        run_id: run_id.to_owned(),
        profile: contract.profile,
        contract: contract.clone(),
        thresholds,
        workloads,
        verdict,
    })
}

/// Judge a comparison only after exact contract, coverage, allocation, and calibration validation.
///
/// # Errors
///
/// Returns [`ReportError`] when the run artifacts, contract, calibration, derived statistics, or
/// verdict inputs are invalid or incompatible.
pub fn judge_comparison(
    run_id: &str,
    contract: &MeasurementContract,
    samples: &[Sample],
    processes: &[ProcessRun],
    allocations: &[AllocationRun],
    calibration: &NoiseRunSummary,
) -> Result<ComparisonRunSummary, ReportError> {
    validate_run_id(run_id)?;
    validate_contract(contract)?;
    validate_calibration(contract, calibration)?;
    validate_coverage(
        run_id,
        contract,
        samples,
        processes,
        [EngineLabel::Neovim, EngineLabel::Oxvim],
    )?;
    validate_allocations(run_id, contract, allocations)?;

    if !matches!(calibration.verdict, NoiseVerdict::Pass) {
        return incompatible("calibration ceiling verdict must pass");
    }

    let mut summaries = Vec::with_capacity(contract.cells.len());
    let mut all_failures = Vec::new();
    let mut all_warnings = Vec::new();
    for cell in &contract.cells {
        let noise = calibration_noise(calibration, &cell.workload)?;
        let summary = summarize_comparison_workload(
            run_id,
            cell,
            samples,
            processes,
            allocations,
            noise,
            calibration.thresholds,
        )?;
        match &summary.verdict {
            Verdict::Pass => {}
            Verdict::Warn(warnings) => all_warnings.extend(warnings.iter().cloned()),
            Verdict::Fail(failures) => all_failures.extend(failures.iter().cloned()),
        }
        summaries.push(summary);
    }
    let verdict = if all_failures.is_empty() {
        if all_warnings.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Warn(all_warnings)
        }
    } else {
        Verdict::Fail(all_failures)
    };
    Ok(ComparisonRunSummary {
        run_id: run_id.to_owned(),
        profile: contract.profile,
        contract: contract.clone(),
        calibration_run_id: calibration.run_id.clone(),
        verdict,
        workloads: summaries,
    })
}

fn summarize_comparison_workload(
    run_id: &str,
    cell: &CellContract,
    samples: &[Sample],
    processes: &[ProcessRun],
    allocations: &[AllocationRun],
    noise: &WorkloadNoise,
    thresholds: Thresholds,
) -> Result<WorkloadSummary, ReportError> {
    let neovim = sample_percentiles(
        samples,
        &cell.workload,
        EngineLabel::Neovim,
        |sample| sample.wall_us,
        "wall",
    )?;
    let oxvim = sample_percentiles(
        samples,
        &cell.workload,
        EngineLabel::Oxvim,
        |sample| sample.wall_us,
        "wall",
    )?;
    let neovim_rss = process_percentiles(processes, &cell.workload, EngineLabel::Neovim)?;
    let oxvim_rss = process_percentiles(processes, &cell.workload, EngineLabel::Oxvim)?;
    let lat_ratio = ratio(oxvim.p95, neovim.p95, &cell.workload, "latency_p95")?;
    let lat_max_ratio = ratio(oxvim.max, neovim.max, &cell.workload, "latency_max")?;
    let rss_ratio = ratio(oxvim_rss.p95, neovim_rss.p95, &cell.workload, "rss_p95")?;
    let (mismatched_samples, first_mismatch) = snapshot_mismatches(samples, &cell.workload)?;
    let verdict = judge_workload(&WorkloadVerdictInput {
        workload: &cell.workload,
        mismatches: mismatched_samples,
        first_mismatch,
        lat_ratio,
        lat_max_ratio,
        rss_ratio,
        neovim_p95: neovim.p95,
        noise_lat: noise.noise_lat,
        thresholds,
    })?;
    let allocation_summary = summarize_allocations(run_id, &cell.workload, allocations)?;
    let stages = summarize_steady_stages(run_id, cell, samples, noise.noise_lat)?;
    Ok(WorkloadSummary {
        run_id: run_id.to_owned(),
        workload: cell.workload.clone(),
        neovim,
        oxvim,
        neovim_rss,
        oxvim_rss,
        allocations: allocation_summary,
        stages,
        lat_ratio,
        lat_max_ratio,
        rss_ratio,
        noise_lat: noise.noise_lat,
        noise_rss: noise.noise_rss,
        threshold_warn: thresholds.adaptive_warn(noise.noise_lat)?,
        mismatched_samples,
        verdict,
    })
}

struct WorkloadVerdictInput<'a> {
    workload: &'a str,
    mismatches: usize,
    first_mismatch: usize,
    lat_ratio: f64,
    lat_max_ratio: f64,
    rss_ratio: f64,
    neovim_p95: u64,
    noise_lat: f64,
    thresholds: Thresholds,
}

fn judge_workload(input: &WorkloadVerdictInput<'_>) -> Result<Verdict, ReportError> {
    let WorkloadVerdictInput {
        workload,
        mismatches,
        first_mismatch,
        lat_ratio,
        lat_max_ratio,
        rss_ratio,
        neovim_p95,
        noise_lat,
        thresholds,
    } = *input;
    for (field, value) in [
        ("lat_ratio", lat_ratio),
        ("lat_max_ratio", lat_max_ratio),
        ("rss_ratio", rss_ratio),
        ("noise_lat", noise_lat),
    ] {
        valid_nonnegative(field, value)?;
    }
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    if mismatches > 0 {
        failures.push(Rejection::Incomparable {
            workload: workload.to_owned(),
            mismatched: mismatches,
            first_mismatch,
        });
    }
    if neovim_p95 < thresholds.absolute_mode_below_us {
        let delta = (lat_ratio - 1.0) * decimal_f64(neovim_p95);
        if delta > decimal_f64(thresholds.absolute_fail_us) {
            failures.push(Rejection::Latency {
                workload: workload.to_owned(),
                ratio: delta,
                threshold: decimal_f64(thresholds.absolute_fail_us),
                kind: LatencyKind::Absolute,
            });
        } else if delta > decimal_f64(thresholds.absolute_warn_us) {
            warnings.push(format!("{workload}: p95 latency rose by {delta:.3} us"));
        }
    } else if lat_ratio > thresholds.lat_fail {
        failures.push(Rejection::Latency {
            workload: workload.to_owned(),
            ratio: lat_ratio,
            threshold: thresholds.lat_fail,
            kind: LatencyKind::P95,
        });
    } else {
        let adaptive_warn = thresholds.adaptive_warn(noise_lat)?;
        if lat_ratio > adaptive_warn {
            warnings.push(format!(
                "{workload}: p95 latency ratio {lat_ratio:.3} exceeds {adaptive_warn:.3}"
            ));
        }
    }
    if lat_max_ratio > thresholds.max_fail {
        failures.push(Rejection::Latency {
            workload: workload.to_owned(),
            ratio: lat_max_ratio,
            threshold: thresholds.max_fail,
            kind: LatencyKind::Max,
        });
    }
    if rss_ratio > thresholds.rss_fail {
        failures.push(Rejection::Memory {
            workload: workload.to_owned(),
            ratio: rss_ratio,
            threshold: thresholds.rss_fail,
        });
    } else if rss_ratio > thresholds.rss_warn {
        warnings.push(format!(
            "{workload}: RSS ratio {rss_ratio:.3} exceeds {:.3}",
            thresholds.rss_warn
        ));
    }
    Ok(if failures.is_empty() {
        if warnings.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Warn(warnings)
        }
    } else {
        Verdict::Fail(failures)
    })
}

fn validate_contract(contract: &MeasurementContract) -> Result<(), ReportError> {
    if contract.schema_version == 0 {
        return invalid_contract("schema_version must be nonzero");
    }
    for (field, value) in [
        (
            "release_harness_profile",
            contract.release_harness_profile.as_str(),
        ),
        ("host.hostname", contract.host.hostname.as_str()),
        ("host.os", contract.host.os.as_str()),
        ("host.kernel", contract.host.kernel.as_str()),
        ("host.architecture", contract.host.architecture.as_str()),
        ("host.cpu", contract.host.cpu.as_str()),
    ] {
        nonempty(field, value)?;
    }
    if contract.host.logical_cpus == 0
        || contract.host.memory_kib == 0
        || contract.ui.width == 0
        || contract.ui.height == 0
    {
        return invalid_contract("host capacity and UI dimensions must be nonzero");
    }
    validate_file("oracle", &contract.oracle)?;
    validate_file("candidate", &contract.candidate)?;
    if contract.cells.is_empty() {
        return invalid_contract("at least one cell is required");
    }
    let mut names = BTreeSet::new();
    for cell in &contract.cells {
        nonempty("cell.workload", &cell.workload)?;
        nonempty("cell.input_definition", &cell.input_definition)?;
        if !names.insert(cell.workload.clone()) {
            return invalid_contract("cell workloads must be unique");
        }
        if cell.processes_per_engine == 0
            || cell.samples_per_process == 0
            || cell.timeout_ms == 0
            || cell.wall_limit_ms == 0
            || cell.allocation_runs_per_engine == 0
        {
            return invalid_contract("cell counts and time limits must be nonzero");
        }
        if cell.fixture_hashes.is_empty() {
            return invalid_contract("every cell requires fixture hashes");
        }
        for fixture_hash in &cell.fixture_hashes {
            nonempty("fixture.sha256", fixture_hash)?;
        }
    }
    Ok(())
}

fn validate_calibration(
    contract: &MeasurementContract,
    calibration: &NoiseRunSummary,
) -> Result<(), ReportError> {
    validate_run_id(&calibration.run_id)?;
    if calibration.schema_version != NOISE_SCHEMA_VERSION {
        return incompatible("noise schema version mismatch");
    }
    if calibration.profile != contract.profile {
        return incompatible("noise profile mismatch");
    }
    if calibration.contract != *contract {
        return incompatible("measurement contract mismatch");
    }
    calibration.thresholds.validate()?;
    if calibration.workloads.len() != contract.cells.len() {
        return incompatible("noise workload count mismatch");
    }
    let expected: BTreeSet<&str> = contract
        .cells
        .iter()
        .map(|cell| cell.workload.as_str())
        .collect();
    let actual: BTreeSet<&str> = calibration
        .workloads
        .iter()
        .map(|entry| entry.workload.as_str())
        .collect();
    if expected != actual || actual.len() != calibration.workloads.len() {
        return incompatible("noise workload identity mismatch");
    }
    for noise in &calibration.workloads {
        if noise.run_id != calibration.run_id {
            return incompatible("noise workload run_id mismatch");
        }
        valid_nonnegative("noise_lat", noise.noise_lat)?;
        valid_nonnegative("noise_rss", noise.noise_rss)?;
        let recomputed_lat = symmetric_spread(
            noise.latency_a.p95,
            noise.latency_b.p95,
            &noise.workload,
            "noise_lat",
        )?;
        let recomputed_rss = symmetric_spread(
            noise.rss_a.p95,
            noise.rss_b.p95,
            &noise.workload,
            "noise_rss",
        )?;
        if recomputed_lat.to_bits() != noise.noise_lat.to_bits()
            || recomputed_rss.to_bits() != noise.noise_rss.to_bits()
        {
            return incompatible("stored noise operands do not reproduce stored ratios");
        }
    }
    Ok(())
}

fn validate_coverage(
    run_id: &str,
    contract: &MeasurementContract,
    samples: &[Sample],
    processes: &[ProcessRun],
    engines: [EngineLabel; 2],
) -> Result<(), ReportError> {
    let cells: BTreeMap<&str, &CellContract> = contract
        .cells
        .iter()
        .map(|cell| (cell.workload.as_str(), cell))
        .collect();
    validate_process_coverage(run_id, &cells, processes, engines)?;
    validate_sample_coverage(run_id, &cells, samples, engines)?;
    for cell in &contract.cells {
        let wanted_samples = cell
            .processes_per_engine
            .checked_mul(cell.samples_per_process)
            .ok_or_else(|| ReportError::ArithmeticOverflow {
                field: "wanted_samples".to_owned(),
            })?;
        for engine in engines {
            let process_count = processes
                .iter()
                .filter(|record| record.workload == cell.workload && record.engine == engine)
                .count();
            exact_count(
                "process",
                &cell.workload,
                engine,
                process_count,
                cell.processes_per_engine,
            )?;
            let sample_count = samples
                .iter()
                .filter(|record| record.workload == cell.workload && record.engine == engine)
                .count();
            exact_count(
                "sample",
                &cell.workload,
                engine,
                sample_count,
                wanted_samples,
            )?;
        }
    }
    Ok(())
}

fn validate_process_coverage(
    run_id: &str,
    cells: &BTreeMap<&str, &CellContract>,
    processes: &[ProcessRun],
    engines: [EngineLabel; 2],
) -> Result<(), ReportError> {
    let mut process_keys = BTreeSet::new();
    for process in processes {
        validate_artifact_run_id("process", run_id, &process.run_id)?;
        if !engines.contains(&process.engine) {
            return Err(ReportError::WrongEngine {
                workload: process.workload.clone(),
                engine: process.engine,
            });
        }
        let cell =
            cells
                .get(process.workload.as_str())
                .ok_or_else(|| ReportError::UnknownWorkload {
                    workload: process.workload.clone(),
                })?;
        if process.process_index >= cell.processes_per_engine
            || process.samples_recorded != cell.samples_per_process
            || process.warmup_discarded != cell.warmup_per_process
            || process.peak_rss_kib == 0
        {
            return Err(ReportError::InvalidProcessRecord {
                workload: process.workload.clone(),
                engine: process.engine,
                process_index: process.process_index,
                reason: "index, counts, warmup, or RSS does not match contract".to_owned(),
            });
        }
        if !process_keys.insert((
            process.workload.as_str(),
            process.engine,
            process.process_index,
        )) {
            return Err(ReportError::DuplicateRecord {
                artifact: "process".to_owned(),
                workload: process.workload.clone(),
                engine: process.engine,
                process_index: process.process_index,
                sample_index: None,
            });
        }
    }
    Ok(())
}

fn validate_sample_coverage(
    run_id: &str,
    cells: &BTreeMap<&str, &CellContract>,
    samples: &[Sample],
    engines: [EngineLabel; 2],
) -> Result<(), ReportError> {
    let mut sample_keys = BTreeSet::new();
    for sample in samples {
        validate_artifact_run_id("sample", run_id, &sample.run_id)?;
        if !engines.contains(&sample.engine) {
            return Err(ReportError::WrongEngine {
                workload: sample.workload.clone(),
                engine: sample.engine,
            });
        }
        let cell =
            cells
                .get(sample.workload.as_str())
                .ok_or_else(|| ReportError::UnknownWorkload {
                    workload: sample.workload.clone(),
                })?;
        validate_sample(sample, cell)?;
        if !sample_keys.insert((
            sample.workload.as_str(),
            sample.engine,
            sample.process_index,
            sample.sample_index,
        )) {
            return Err(ReportError::DuplicateRecord {
                artifact: "sample".to_owned(),
                workload: sample.workload.clone(),
                engine: sample.engine,
                process_index: sample.process_index,
                sample_index: Some(sample.sample_index),
            });
        }
    }
    Ok(())
}

fn validate_sample(sample: &Sample, cell: &CellContract) -> Result<(), ReportError> {
    if sample.process_index >= cell.processes_per_engine
        || sample.sample_index >= cell.samples_per_process
        || sample.wall_us == 0
        || sample.raw_source.trim().is_empty()
        || sample.grid_sha256.trim().is_empty()
    {
        return invalid_sample(
            sample,
            "index, wall time, raw source, or grid hash is invalid",
        );
    }
    match cell.stage_contract {
        StageContract::Startup => {
            if sample.startup_delta_us.is_none()
                || sample.response_us.is_some()
                || sample.flush_us.is_some()
            {
                return invalid_sample(sample, "startup requires only startup_delta_us");
            }
        }
        StageContract::SteadyState { response, flush } => {
            if sample.startup_delta_us.is_some()
                || response != sample.response_us.is_some()
                || flush != sample.flush_us.is_some()
            {
                return invalid_sample(
                    sample,
                    "steady-state stage presence does not match contract",
                );
            }
            if response
                && flush
                && let (Some(response_us), Some(flush_us)) = (sample.response_us, sample.flush_us)
            {
                let sum = response_us.checked_add(flush_us).ok_or_else(|| {
                    ReportError::ArithmeticOverflow {
                        field: "response_us + flush_us".to_owned(),
                    }
                })?;
                if sum != sample.wall_us {
                    return invalid_sample(
                        sample,
                        "response_us + flush_us must equal wall_us exactly",
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_allocations(
    run_id: &str,
    contract: &MeasurementContract,
    allocations: &[AllocationRun],
) -> Result<(), ReportError> {
    let cells: BTreeMap<&str, &CellContract> = contract
        .cells
        .iter()
        .map(|cell| (cell.workload.as_str(), cell))
        .collect();
    let mut keys = BTreeSet::new();
    for run in allocations {
        validate_artifact_run_id("allocation", run_id, &run.run_id)?;
        if !run.engine.is_comparison() {
            return Err(ReportError::WrongEngine {
                workload: run.workload.clone(),
                engine: run.engine,
            });
        }
        let cell =
            cells
                .get(run.workload.as_str())
                .ok_or_else(|| ReportError::UnknownWorkload {
                    workload: run.workload.clone(),
                })?;
        if run.process_index >= cell.allocation_runs_per_engine || run.raw_files.is_empty() {
            return Err(ReportError::InvalidAllocationRecord {
                workload: run.workload.clone(),
                engine: run.engine,
                process_index: run.process_index,
                reason: "index or raw file coverage is invalid".to_owned(),
            });
        }
        for raw_file in &run.raw_files {
            validate_file("allocation.raw_file", raw_file)?;
        }
        if !keys.insert((run.workload.as_str(), run.engine, run.process_index)) {
            return Err(ReportError::DuplicateRecord {
                artifact: "allocation".to_owned(),
                workload: run.workload.clone(),
                engine: run.engine,
                process_index: run.process_index,
                sample_index: None,
            });
        }
    }
    for cell in &contract.cells {
        for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
            let count = allocations
                .iter()
                .filter(|run| run.workload == cell.workload && run.engine == engine)
                .count();
            exact_count(
                "allocation",
                &cell.workload,
                engine,
                count,
                cell.allocation_runs_per_engine,
            )?;
        }
    }
    Ok(())
}

fn summarize_allocations(
    run_id: &str,
    workload: &str,
    allocations: &[AllocationRun],
) -> Result<AllocationSummary, ReportError> {
    let raw_runs: Vec<AllocationRun> = allocations
        .iter()
        .filter(|run| run.workload == workload)
        .cloned()
        .collect();
    let neovim_blocks = allocation_percentiles(
        &raw_runs,
        workload,
        EngineLabel::Neovim,
        |run| run.total_blocks,
        "blocks",
    )?;
    let oxvim_blocks = allocation_percentiles(
        &raw_runs,
        workload,
        EngineLabel::Oxvim,
        |run| run.total_blocks,
        "blocks",
    )?;
    let neovim_bytes = allocation_percentiles(
        &raw_runs,
        workload,
        EngineLabel::Neovim,
        |run| run.total_bytes,
        "bytes",
    )?;
    let oxvim_bytes = allocation_percentiles(
        &raw_runs,
        workload,
        EngineLabel::Oxvim,
        |run| run.total_bytes,
        "bytes",
    )?;
    Ok(AllocationSummary {
        run_id: run_id.to_owned(),
        workload: workload.to_owned(),
        neovim_blocks,
        oxvim_blocks,
        neovim_bytes,
        oxvim_bytes,
        raw_runs,
    })
}

/// Build the complete stage artifact: aggregated startup rows (or an explicit
/// `not_collected` state) plus every steady-state row.
///
/// # Errors
///
/// Returns [`ReportError`] when the run, contract, calibration, sample stages, or startup-stage
/// records are invalid or incompatible.
pub fn summarize_stages(
    run_id: &str,
    contract: &MeasurementContract,
    samples: &[Sample],
    startup: &[StartupStageRun],
    calibration: &NoiseRunSummary,
) -> Result<StagesArtifact, ReportError> {
    validate_run_id(run_id)?;
    validate_contract(contract)?;
    validate_calibration(contract, calibration)?;
    let mut steady_state = Vec::new();
    for cell in &contract.cells {
        let noise = calibration_noise(calibration, &cell.workload)?;
        steady_state.extend(summarize_steady_stages(
            run_id,
            cell,
            samples,
            noise.noise_lat,
        )?);
    }
    let startup_stages = summarize_startup_stages(run_id, contract, startup, calibration)?;
    Ok(StagesArtifact {
        schema_version: STAGES_SCHEMA_VERSION,
        run_id: run_id.to_owned(),
        startup: startup_stages,
        steady_state,
        notes: STAGES_NOTES.to_owned(),
    })
}

/// Summarize the steady-state stage rows for one workload.
fn summarize_steady_stages(
    run_id: &str,
    cell: &CellContract,
    samples: &[Sample],
    noise_lat: f64,
) -> Result<Vec<StageArtifactRow>, ReportError> {
    let mut rows = Vec::with_capacity(2);
    for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
        let selected: Vec<&Sample> = samples
            .iter()
            .filter(|sample| sample.workload == cell.workload && sample.engine == engine)
            .collect();
        let raw_samples = selected
            .iter()
            .map(|sample| StageProvenance {
                process_index: sample.process_index,
                sample_index: sample.sample_index,
                wall_us: sample.wall_us,
                response_us: sample.response_us,
                flush_us: sample.flush_us,
                startup_delta_us: sample.startup_delta_us,
                raw_source: sample.raw_source.clone(),
            })
            .collect();
        let wall = selected_percentiles(&selected, &cell.workload, "wall", |sample| {
            Some(sample.wall_us)
        })?;
        if let StageContract::SteadyState { response, flush } = cell.stage_contract {
            let response_summary = if response {
                Some(selected_percentiles(
                    &selected,
                    &cell.workload,
                    "response",
                    |sample| sample.response_us,
                )?)
            } else {
                None
            };
            let flush_summary = if flush {
                Some(selected_percentiles(
                    &selected,
                    &cell.workload,
                    "flush",
                    |sample| sample.flush_us,
                )?)
            } else {
                None
            };
            let response_materiality = response_summary
                .map(|summary| materiality_open(summary.p95, wall.p95, noise_lat))
                .transpose()?;
            let flush_materiality = flush_summary
                .map(|summary| materiality_open(summary.p95, wall.p95, noise_lat))
                .transpose()?;
            rows.push(StageArtifactRow::SteadyState {
                run_id: run_id.to_owned(),
                workload: cell.workload.clone(),
                engine,
                wall,
                response: response_summary,
                flush: flush_summary,
                response_materiality,
                flush_materiality,
                raw_samples,
            });
        }
    }
    Ok(rows)
}

/// Aggregate startup logs into per-ordinal stage rows, or report `not_collected`.
fn summarize_startup_stages(
    run_id: &str,
    contract: &MeasurementContract,
    startup: &[StartupStageRun],
    calibration: &NoiseRunSummary,
) -> Result<StartupStages, ReportError> {
    if startup.is_empty() {
        return Ok(StartupStages::NotCollected);
    }
    let mut grouped: BTreeMap<(String, EngineLabel), Vec<&StartupStageRun>> = BTreeMap::new();
    let mut seen: BTreeSet<(String, EngineLabel, usize)> = BTreeSet::new();
    for run in startup {
        validate_startup_stage_run(run, run_id, contract)?;
        if !seen.insert((run.workload.clone(), run.engine, run.process_index)) {
            return Err(ReportError::DuplicateRecord {
                artifact: "startup_stage".to_owned(),
                workload: run.workload.clone(),
                engine: run.engine,
                process_index: run.process_index,
                sample_index: None,
            });
        }
        grouped
            .entry((run.workload.clone(), run.engine))
            .or_default()
            .push(run);
    }
    require_startup_stage_coverage(contract, &grouped)?;
    let mut rows = Vec::new();
    for cell in &contract.cells {
        if !matches!(cell.stage_contract, StageContract::Startup) {
            continue;
        }
        let noise = calibration_noise(calibration, &cell.workload)?;
        for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
            let runs = grouped
                .get(&(cell.workload.clone(), engine))
                .ok_or_else(|| ReportError::MissingCoverage {
                    artifact: "startup_stages".to_owned(),
                    workload: cell.workload.clone(),
                    engine,
                    got: 0,
                    want: cell.processes_per_engine,
                })?;
            let first = runs.first().ok_or_else(|| ReportError::MissingCoverage {
                artifact: "startup_stages".to_owned(),
                workload: cell.workload.clone(),
                engine,
                got: 0,
                want: cell.processes_per_engine,
            })?;
            for run in runs.iter().skip(1) {
                let same_sequence = run.marks.len() == first.marks.len()
                    && run
                        .marks
                        .iter()
                        .zip(first.marks.iter())
                        .all(|(mark, expected)| {
                            mark.ordinal == expected.ordinal && mark.label == expected.label
                        });
                if !same_sequence {
                    return invalid_contract("startup mark sequences must match across processes");
                }
            }
            for (ordinal, expected) in first.marks.iter().enumerate() {
                let mut deltas = Vec::with_capacity(runs.len());
                let mut whole = Vec::with_capacity(runs.len());
                for run in runs {
                    let mark =
                        run.marks
                            .get(ordinal)
                            .ok_or_else(|| ReportError::InvalidContract {
                                reason: "startup mark ordinal missing".to_owned(),
                            })?;
                    deltas.push(mark.delta_us);
                    let last = run
                        .marks
                        .last()
                        .ok_or_else(|| ReportError::InvalidContract {
                            reason: "startup mark total missing".to_owned(),
                        })?;
                    whole.push(last.clock_us);
                }
                let delta = summarize_values(&mut deltas, &cell.workload, "startup_delta")?;
                let whole_startup = summarize_values(&mut whole, &cell.workload, "whole_startup")?;
                let materiality = materiality_open(delta.p95, whole_startup.p95, noise.noise_lat)?;
                rows.push(StartupStageRow {
                    run_id: run_id.to_owned(),
                    workload: cell.workload.clone(),
                    engine,
                    ordinal,
                    label: expected.label.clone(),
                    delta,
                    whole_startup,
                    materiality,
                });
            }
        }
    }
    Ok(StartupStages::Collected { rows })
}

/// Find the calibration noise record for one workload.
fn calibration_noise<'a>(
    calibration: &'a NoiseRunSummary,
    workload: &str,
) -> Result<&'a WorkloadNoise, ReportError> {
    calibration
        .workloads
        .iter()
        .find(|noise| noise.workload == workload)
        .ok_or_else(|| ReportError::IncompatibleCalibration {
            reason: format!("missing workload {workload}"),
        })
}

/// Validate one startup stage record against the current run and contract.
fn validate_startup_stage_run(
    run: &StartupStageRun,
    run_id: &str,
    contract: &MeasurementContract,
) -> Result<(), ReportError> {
    validate_artifact_run_id("startup_stage", run_id, &run.run_id)?;
    nonempty("startup_stage.raw_source", &run.raw_source)?;
    let cell = contract
        .cells
        .iter()
        .find(|cell| cell.workload == run.workload)
        .ok_or_else(|| ReportError::UnknownWorkload {
            workload: run.workload.clone(),
        })?;
    if !matches!(cell.stage_contract, StageContract::Startup) {
        return invalid_contract("startup stage record targets a steady-state cell");
    }
    if !matches!(run.engine, EngineLabel::Neovim | EngineLabel::Oxvim) {
        return Err(ReportError::WrongEngine {
            workload: run.workload.clone(),
            engine: run.engine,
        });
    }
    if run.marks.is_empty() {
        return invalid_contract("startup stage record requires at least one mark");
    }
    let mut previous_clock: Option<u64> = None;
    for (index, mark) in run.marks.iter().enumerate() {
        if mark.ordinal != index {
            return invalid_contract("startup marks must be ordered from ordinal zero");
        }
        nonempty("startup_stage.mark.label", &mark.label)?;
        if mark.clock_us < mark.delta_us {
            return invalid_contract("startup mark delta exceeds its cumulative clock");
        }
        if let Some(previous) = previous_clock
            && mark.clock_us < previous
        {
            return invalid_contract("startup clocks must be nondecreasing");
        }
        previous_clock = Some(mark.clock_us);
    }
    Ok(())
}

/// Require every startup-shaped cell to have the exact per-engine process coverage.
fn require_startup_stage_coverage(
    contract: &MeasurementContract,
    grouped: &BTreeMap<(String, EngineLabel), Vec<&StartupStageRun>>,
) -> Result<(), ReportError> {
    for cell in &contract.cells {
        if !matches!(cell.stage_contract, StageContract::Startup) {
            continue;
        }
        for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
            let got = match grouped.get(&(cell.workload.clone(), engine)) {
                Some(runs) => runs.len(),
                None => 0,
            };
            if got != cell.processes_per_engine {
                return Err(ReportError::MissingCoverage {
                    artifact: "startup_stages".to_owned(),
                    workload: cell.workload.clone(),
                    engine,
                    got,
                    want: cell.processes_per_engine,
                });
            }
        }
    }
    Ok(())
}

fn snapshot_mismatches(samples: &[Sample], workload: &str) -> Result<(usize, usize), ReportError> {
    let neovim: BTreeMap<(usize, usize), &str> = samples
        .iter()
        .filter(|sample| sample.workload == workload && sample.engine == EngineLabel::Neovim)
        .map(|sample| {
            (
                (sample.process_index, sample.sample_index),
                sample.grid_sha256.as_str(),
            )
        })
        .collect();
    let oxvim: BTreeMap<(usize, usize), &str> = samples
        .iter()
        .filter(|sample| sample.workload == workload && sample.engine == EngineLabel::Oxvim)
        .map(|sample| {
            (
                (sample.process_index, sample.sample_index),
                sample.grid_sha256.as_str(),
            )
        })
        .collect();
    if neovim.len() != oxvim.len() {
        return Err(ReportError::MissingCoverage {
            artifact: "snapshot".to_owned(),
            workload: workload.to_owned(),
            engine: EngineLabel::Oxvim,
            got: oxvim.len(),
            want: neovim.len(),
        });
    }
    let mut count = 0usize;
    let mut first = 0usize;
    for (ordinal, (key, left)) in neovim.iter().enumerate() {
        let right = oxvim.get(key).ok_or_else(|| ReportError::MissingCoverage {
            artifact: "snapshot".to_owned(),
            workload: workload.to_owned(),
            engine: EngineLabel::Oxvim,
            got: oxvim.len(),
            want: neovim.len(),
        })?;
        if left != right {
            count = count
                .checked_add(1)
                .ok_or_else(|| ReportError::ArithmeticOverflow {
                    field: "mismatch_count".to_owned(),
                })?;
            if count == 1 {
                first = ordinal;
            }
        }
    }
    Ok((count, first))
}

fn sample_percentiles<F>(
    samples: &[Sample],
    workload: &str,
    engine: EngineLabel,
    select: F,
    metric: &str,
) -> Result<Percentiles, ReportError>
where
    F: Fn(&Sample) -> u64,
{
    let mut values: Vec<u64> = samples
        .iter()
        .filter(|sample| sample.workload == workload && sample.engine == engine)
        .map(select)
        .collect();
    summarize_values(&mut values, workload, metric)
}

fn selected_percentiles<F>(
    samples: &[&Sample],
    workload: &str,
    metric: &str,
    select: F,
) -> Result<Percentiles, ReportError>
where
    F: Fn(&Sample) -> Option<u64>,
{
    let mut values = Vec::with_capacity(samples.len());
    for sample in samples {
        values.push(
            select(sample).ok_or_else(|| ReportError::InvalidSampleRecord {
                workload: workload.to_owned(),
                engine: sample.engine,
                process_index: sample.process_index,
                sample_index: sample.sample_index,
                reason: format!("missing {metric}"),
            })?,
        );
    }
    summarize_values(&mut values, workload, metric)
}

fn process_percentiles(
    processes: &[ProcessRun],
    workload: &str,
    engine: EngineLabel,
) -> Result<Percentiles, ReportError> {
    let mut values: Vec<u64> = processes
        .iter()
        .filter(|process| process.workload == workload && process.engine == engine)
        .map(|process| process.peak_rss_kib)
        .collect();
    summarize_values(&mut values, workload, "peak_rss_kib")
}

fn allocation_percentiles<F>(
    runs: &[AllocationRun],
    workload: &str,
    engine: EngineLabel,
    select: F,
    metric: &str,
) -> Result<Percentiles, ReportError>
where
    F: Fn(&AllocationRun) -> u64,
{
    let mut values: Vec<u64> = runs
        .iter()
        .filter(|run| run.engine == engine)
        .map(select)
        .collect();
    summarize_values(&mut values, workload, metric)
}

fn summarize_values(
    values: &mut [u64],
    workload: &str,
    metric: &str,
) -> Result<Percentiles, ReportError> {
    summarize(values).map_err(|error| stats_error(workload, metric, &error))
}

fn stats_error(workload: &str, metric: &str, error: &StatsError) -> ReportError {
    ReportError::Percentiles {
        workload: workload.to_owned(),
        metric: metric.to_owned(),
        reason: error.to_string(),
    }
}

fn symmetric_spread(a: u64, b: u64, workload: &str, metric: &str) -> Result<f64, ReportError> {
    let denominator = a.min(b);
    if denominator == 0 {
        return Err(ReportError::ZeroDenominator {
            workload: workload.to_owned(),
            metric: metric.to_owned(),
        });
    }
    let result = decimal_f64(a.abs_diff(b)) / decimal_f64(denominator);
    valid_nonnegative(metric, result)?;
    Ok(result)
}

fn ratio(
    numerator: u64,
    denominator: u64,
    workload: &str,
    metric: &str,
) -> Result<f64, ReportError> {
    if denominator == 0 {
        return Err(ReportError::ZeroDenominator {
            workload: workload.to_owned(),
            metric: metric.to_owned(),
        });
    }
    let result = decimal_f64(numerator) / decimal_f64(denominator);
    valid_nonnegative(metric, result)?;
    Ok(result)
}

fn decimal_f64(value: u64) -> f64 {
    let [high_3, high_2, high_1, high_0, low_3, low_2, low_1, low_0] = value.to_be_bytes();
    let high = f64::from(u32::from_be_bytes([high_3, high_2, high_1, high_0]));
    let low = f64::from(u32::from_be_bytes([low_3, low_2, low_1, low_0]));
    high.mul_add(4_294_967_296.0, low)
}

fn exact_count(
    artifact: &str,
    workload: &str,
    engine: EngineLabel,
    got: usize,
    want: usize,
) -> Result<(), ReportError> {
    if got == want {
        Ok(())
    } else {
        Err(ReportError::MissingCoverage {
            artifact: artifact.to_owned(),
            workload: workload.to_owned(),
            engine,
            got,
            want,
        })
    }
}

fn validate_run_id(run_id: &str) -> Result<(), ReportError> {
    nonempty("run_id", run_id)
}

fn validate_artifact_run_id(
    artifact: &str,
    expected: &str,
    actual: &str,
) -> Result<(), ReportError> {
    validate_run_id(actual)?;
    if expected == actual {
        Ok(())
    } else {
        Err(ReportError::WrongRunId {
            artifact: artifact.to_owned(),
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        })
    }
}

fn validate_file(field: &str, file: &FileIdentity) -> Result<(), ReportError> {
    nonempty(&format!("{field}.path"), &file.path)?;
    nonempty(&format!("{field}.sha256"), &file.sha256)
}

fn nonempty(field: &str, value: &str) -> Result<(), ReportError> {
    if value.trim().is_empty() {
        Err(ReportError::EmptyField {
            field: field.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn valid_nonnegative(field: &str, value: f64) -> Result<(), ReportError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(ReportError::InvalidNumber {
            field: field.to_owned(),
        })
    }
}

fn invalid_contract<T>(reason: &str) -> Result<T, ReportError> {
    Err(ReportError::InvalidContract {
        reason: reason.to_owned(),
    })
}
fn incompatible<T>(reason: &str) -> Result<T, ReportError> {
    Err(ReportError::IncompatibleCalibration {
        reason: reason.to_owned(),
    })
}
fn invalid_sample<T>(sample: &Sample, reason: &str) -> Result<T, ReportError> {
    Err(ReportError::InvalidSampleRecord {
        workload: sample.workload.clone(),
        engine: sample.engine,
        process_index: sample.process_index,
        sample_index: sample.sample_index,
        reason: reason.to_owned(),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code"
)]
mod tests {
    use super::*;

    const RUN: &str = "run-20260901";
    const CALIBRATION_RUN: &str = "run-20260901-noise";

    fn hex64(tag: &str) -> String {
        let mut hash = String::with_capacity(64);
        while hash.len() < 64 {
            hash.push_str(tag);
        }
        hash.truncate(64);
        hash
    }

    fn file_identity(name: &str) -> FileIdentity {
        FileIdentity {
            path: format!("/fixtures/{name}"),
            sha256: hex64(name),
        }
    }

    fn fixture_hash(name: &str) -> String {
        hex64(name)
    }

    fn contract_with(cells: Vec<CellContract>) -> MeasurementContract {
        MeasurementContract {
            schema_version: 1,
            profile: Profile::Quick,
            release_harness_profile: "release".to_owned(),
            host: HostFingerprint {
                hostname: "host".to_owned(),
                os: "linux".to_owned(),
                kernel: "7.0.0".to_owned(),
                architecture: "x86_64".to_owned(),
                cpu: "test-cpu".to_owned(),
                logical_cpus: 8,
                memory_kib: 16_777_216,
            },
            oracle: file_identity("oracle"),
            candidate: file_identity("candidate"),
            ui: UiContract {
                width: 200,
                height: 60,
                rgb: true,
                ext_linegrid: true,
            },
            order: AlternationOrder::AlternatingFirstArm,
            cells,
        }
    }

    fn steady_cell(workload: &str) -> CellContract {
        CellContract {
            workload: workload.to_owned(),
            processes_per_engine: 2,
            samples_per_process: 2,
            warmup_per_process: 1,
            timeout_ms: 1_000,
            wall_limit_ms: 5_000,
            allocation_runs_per_engine: 1,
            input_definition: "steady input".to_owned(),
            stage_contract: StageContract::SteadyState {
                response: true,
                flush: true,
            },
            fixture_hashes: vec![fixture_hash(workload)],
        }
    }

    fn startup_cell(workload: &str) -> CellContract {
        CellContract {
            stage_contract: StageContract::Startup,
            ..steady_cell(workload)
        }
    }

    fn steady_sample(
        run_id: &str,
        workload: &str,
        engine: EngineLabel,
        process_index: usize,
        sample_index: usize,
        wall_us: u64,
    ) -> Sample {
        Sample {
            run_id: run_id.to_owned(),
            workload: workload.to_owned(),
            engine,
            process_index,
            sample_index,
            wall_us,
            response_us: Some(wall_us / 2),
            flush_us: Some(wall_us / 2),
            startup_delta_us: None,
            grid_sha256: hex64("grid"),
            raw_source: "samples.ndjson".to_owned(),
        }
    }

    fn startup_sample(
        run_id: &str,
        workload: &str,
        engine: EngineLabel,
        process_index: usize,
        sample_index: usize,
        wall_us: u64,
    ) -> Sample {
        Sample {
            startup_delta_us: Some(wall_us),
            response_us: None,
            flush_us: None,
            ..steady_sample(
                run_id,
                workload,
                engine,
                process_index,
                sample_index,
                wall_us,
            )
        }
    }

    fn process_run(
        run_id: &str,
        workload: &str,
        engine: EngineLabel,
        process_index: usize,
        peak_rss_kib: u64,
    ) -> Result<ProcessRun, ReportError> {
        let process_id = u32::try_from(process_index)
            .ok()
            .and_then(|index| 4_000_u32.checked_add(index))
            .ok_or_else(|| ReportError::ArithmeticOverflow {
                field: "process_run.pid".to_owned(),
            })?;
        Ok(ProcessRun {
            run_id: run_id.to_owned(),
            workload: workload.to_owned(),
            engine,
            process_index,
            pid: process_id,
            peak_rss_kib,
            warmup_discarded: 1,
            samples_recorded: 2,
        })
    }

    fn steady_population(
        run_id: &str,
        workload: &str,
    ) -> (Vec<Sample>, Vec<ProcessRun>, Vec<AllocationRun>) {
        let mut samples = Vec::new();
        let mut processes = Vec::new();
        for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
            for process_index in 0..2 {
                for sample_index in 0..2 {
                    samples.push(steady_sample(
                        run_id,
                        workload,
                        engine,
                        process_index,
                        sample_index,
                        100,
                    ));
                }
                processes
                    .push(process_run(run_id, workload, engine, process_index, 1_000).unwrap());
            }
        }
        let allocations = [EngineLabel::Neovim, EngineLabel::Oxvim]
            .into_iter()
            .map(|engine| AllocationRun {
                run_id: run_id.to_owned(),
                workload: workload.to_owned(),
                engine,
                process_index: 0,
                total_blocks: 1_000,
                total_bytes: 2_000,
                raw_files: vec![file_identity("dhat")],
            })
            .collect();
        (samples, processes, allocations)
    }

    fn arm_population(
        run_id: &str,
        cell: &CellContract,
        a_wall_us: u64,
        b_wall_us: u64,
    ) -> (Vec<Sample>, Vec<ProcessRun>) {
        let startup_shaped = matches!(cell.stage_contract, StageContract::Startup);
        let mut samples = Vec::new();
        let mut processes = Vec::new();
        for (engine, wall_us, rss_kib) in [
            (EngineLabel::NoiseA, a_wall_us, 1_000),
            (EngineLabel::NoiseB, b_wall_us, 1_010),
        ] {
            for process_index in 0..2 {
                for sample_index in 0..2 {
                    let sample = if startup_shaped {
                        startup_sample(
                            run_id,
                            &cell.workload,
                            engine,
                            process_index,
                            sample_index,
                            wall_us,
                        )
                    } else {
                        steady_sample(
                            run_id,
                            &cell.workload,
                            engine,
                            process_index,
                            sample_index,
                            wall_us,
                        )
                    };
                    samples.push(sample);
                }
                processes.push(
                    process_run(run_id, &cell.workload, engine, process_index, rss_kib).unwrap(),
                );
            }
        }
        (samples, processes)
    }

    fn noise_calibration(
        contract: &MeasurementContract,
        a_wall_us: u64,
        b_wall_us: u64,
    ) -> NoiseRunSummary {
        let (samples, processes) =
            arm_population(CALIBRATION_RUN, &contract.cells[0], a_wall_us, b_wall_us);
        summarize_noise(CALIBRATION_RUN, contract, &samples, &processes).unwrap()
    }

    fn nearly(got: f64, want: f64) -> bool {
        (got - want).abs() < 1e-12
    }

    #[test]
    fn noise_summary_is_tagged_with_real_workload_evidence() {
        let contract = contract_with(vec![steady_cell("input")]);
        let calibration = noise_calibration(&contract, 100, 106);
        assert_eq!(calibration.run_id, CALIBRATION_RUN);
        assert_eq!(calibration.profile, Profile::Quick);
        assert_eq!(calibration.verdict, NoiseVerdict::Pass);
        assert_eq!(calibration.workloads.len(), 1);
        let noise = &calibration.workloads[0];
        assert_eq!(noise.run_id, CALIBRATION_RUN);
        assert!(nearly(noise.noise_lat, 0.06));
        assert!(nearly(noise.noise_rss, 0.01));
        assert_eq!(noise.latency_a.p95, 100);
        assert_eq!(noise.latency_b.p95, 106);
        assert_eq!(noise.rss_a.p95, 1_000);
        assert_eq!(noise.rss_b.p95, 1_010);

        let tagged = RunSummary::NoiseCalibration(calibration.clone());
        let json = sonic_rs::to_string(&tagged).unwrap();
        assert!(
            json.contains(r#""mode":"noise_calibration""#),
            "noise summary must carry an explicit mode tag: {json}"
        );
        assert!(
            json.contains(r#""summary":{"schema_version""#),
            "noise payload must be nested under summary: {json}"
        );
        let decoded: RunSummary = sonic_rs::from_str(&json).unwrap();
        assert_eq!(decoded, tagged);
        let RunSummary::NoiseCalibration(decoded) = decoded else {
            panic!("decoded summary must stay a noise calibration");
        };
        assert_eq!(decoded, calibration);
    }

    #[test]
    fn comparison_summary_is_tagged_and_keeps_calibration_identity() {
        let contract = contract_with(vec![steady_cell("input")]);
        let calibration = noise_calibration(&contract, 100, 106);
        let (samples, processes, allocations) = steady_population(RUN, "input");
        let summary = judge_comparison(
            RUN,
            &contract,
            &samples,
            &processes,
            &allocations,
            &calibration,
        )
        .unwrap();
        assert_eq!(summary.run_id, RUN);
        assert_eq!(summary.profile, Profile::Quick);
        assert_eq!(summary.calibration_run_id, CALIBRATION_RUN);
        assert_eq!(summary.verdict, Verdict::Pass);
        assert_eq!(summary.workloads.len(), 1);

        let tagged = RunSummary::Comparison(summary.clone());
        let json = sonic_rs::to_string(&tagged).unwrap();
        assert!(
            json.contains(r#""mode":"comparison""#),
            "comparison summary must carry an explicit mode tag: {json}"
        );
        assert!(
            json.contains(&format!(r#""calibration_run_id":"{CALIBRATION_RUN}""#)),
            "comparison payload must keep the calibration source id: {json}"
        );
        let decoded: RunSummary = sonic_rs::from_str(&json).unwrap();
        assert_eq!(decoded, tagged);
        let RunSummary::Comparison(decoded) = decoded else {
            panic!("decoded summary must stay a comparison");
        };
        assert_eq!(decoded, summary);
    }

    #[test]
    fn builders_require_nonempty_run_ids() {
        let contract = contract_with(vec![steady_cell("input")]);
        let (samples, processes) = arm_population(CALIBRATION_RUN, &contract.cells[0], 100, 106);
        assert!(matches!(
            summarize_noise("", &contract, &samples, &processes),
            Err(ReportError::EmptyField { field }) if field == "run_id"
        ));
        let calibration = noise_calibration(&contract, 100, 106);
        let (samples, processes, allocations) = steady_population(RUN, "input");
        assert!(matches!(
            judge_comparison("", &contract, &samples, &processes, &allocations, &calibration),
            Err(ReportError::EmptyField { field }) if field == "run_id"
        ));
    }

    #[test]
    fn failed_calibration_never_becomes_an_empty_comparison() {
        let contract = contract_with(vec![steady_cell("input")]);
        let calibration = noise_calibration(&contract, 100, 300);
        assert_eq!(
            calibration.verdict,
            NoiseVerdict::Fail(vec![Rejection::NoisyHost {
                workload: "input".to_owned(),
                metric: "latency".to_owned(),
                measured: 2.0,
                ceiling: 0.10,
            }])
        );
        let (samples, processes, allocations) = steady_population(RUN, "input");
        match judge_comparison(
            RUN,
            &contract,
            &samples,
            &processes,
            &allocations,
            &calibration,
        ) {
            Err(ReportError::IncompatibleCalibration { reason }) => {
                assert!(reason.contains("ceiling"), "unexpected reason: {reason}");
            }
            other => panic!("failed calibration must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn stages_artifact_reports_not_collected_startup_and_steady_rows() {
        let contract = contract_with(vec![steady_cell("input")]);
        let calibration = noise_calibration(&contract, 100, 106);
        let (samples, _, _) = steady_population(RUN, "input");
        let artifact = summarize_stages(RUN, &contract, &samples, &[], &calibration).unwrap();
        assert_eq!(artifact.schema_version, STAGES_SCHEMA_VERSION);
        assert_eq!(artifact.run_id, RUN);
        assert_eq!(artifact.startup, StartupStages::NotCollected);
        assert_eq!(artifact.steady_state.len(), 2);
        assert!(matches!(
            artifact.steady_state[0],
            StageArtifactRow::SteadyState { .. }
        ));
        assert!(!artifact.notes.is_empty());
    }

    #[test]
    fn stages_artifact_aggregates_startup_rows_with_materiality() {
        let contract = contract_with(vec![startup_cell("startup")]);
        let calibration = noise_calibration(&contract, 100, 106);
        let samples = [EngineLabel::Neovim, EngineLabel::Oxvim]
            .into_iter()
            .flat_map(|engine| {
                (0..2).flat_map(move |process_index| {
                    (0..2).map(move |sample_index| {
                        startup_sample(RUN, "startup", engine, process_index, sample_index, 100)
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut startup = Vec::new();
        for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
            for process_index in 0..2 {
                let deltas: [(usize, &str, u64, u64); 2] = if process_index == 0 {
                    [(0, "parsing", 40, 40), (1, "ui", 60, 100)]
                } else {
                    [(0, "parsing", 50, 50), (1, "ui", 50, 100)]
                };
                startup.push(StartupStageRun {
                    run_id: RUN.to_owned(),
                    workload: "startup".to_owned(),
                    engine,
                    process_index,
                    raw_source: "startuptime.json".to_owned(),
                    marks: deltas
                        .into_iter()
                        .map(|(ordinal, label, delta_us, clock_us)| StartupStageMark {
                            ordinal,
                            label: label.to_owned(),
                            delta_us,
                            clock_us,
                        })
                        .collect(),
                });
            }
        }
        let artifact = summarize_stages(RUN, &contract, &samples, &startup, &calibration).unwrap();
        let StartupStages::Collected { rows } = &artifact.startup else {
            panic!("collected startup runs must aggregate into startup rows");
        };
        assert_eq!(rows.len(), 4);
        let first = &rows[0];
        assert_eq!(first.run_id, RUN);
        assert_eq!(first.workload, "startup");
        assert_eq!(first.engine, EngineLabel::Neovim);
        assert_eq!(first.ordinal, 0);
        assert_eq!(first.label, "parsing");
        assert_eq!(
            first.delta,
            Percentiles {
                n: 2,
                p50: 40,
                p95: 50,
                p99: 50,
                max: 50
            }
        );
        assert_eq!(
            first.whole_startup,
            Percentiles {
                n: 2,
                p50: 100,
                p95: 100,
                p99: 100,
                max: 100
            }
        );
        assert!(nearly(first.materiality.share, 0.5));
        assert!(nearly(first.materiality.minimum_share, 0.18));
        assert!(nearly(first.materiality.minimum_candidate_gain, 0.18));
        assert!(!first.materiality.material);
        let second = &rows[1];
        assert_eq!(second.ordinal, 1);
        assert_eq!(second.label, "ui");
        assert_eq!(second.delta.p95, 60);
    }

    #[test]
    fn startup_aggregation_requires_exact_process_coverage() {
        let contract = contract_with(vec![startup_cell("startup")]);
        let calibration = noise_calibration(&contract, 100, 106);
        let samples = [EngineLabel::Neovim, EngineLabel::Oxvim]
            .into_iter()
            .flat_map(|engine| {
                (0..2).flat_map(move |process_index| {
                    (0..2).map(move |sample_index| {
                        startup_sample(RUN, "startup", engine, process_index, sample_index, 100)
                    })
                })
            })
            .collect::<Vec<_>>();
        let startup = vec![StartupStageRun {
            run_id: RUN.to_owned(),
            workload: "startup".to_owned(),
            engine: EngineLabel::Neovim,
            process_index: 0,
            raw_source: "startuptime.json".to_owned(),
            marks: vec![
                StartupStageMark {
                    ordinal: 0,
                    label: "parsing".to_owned(),
                    delta_us: 40,
                    clock_us: 40,
                },
                StartupStageMark {
                    ordinal: 1,
                    label: "ui".to_owned(),
                    delta_us: 60,
                    clock_us: 100,
                },
            ],
        }];
        assert!(matches!(
            summarize_stages(RUN, &contract, &samples, &startup, &calibration),
            Err(ReportError::MissingCoverage {
                artifact,
                workload,
                engine,
                got,
                want,
            }) if artifact == "startup_stages"
                && workload == "startup"
                && engine == EngineLabel::Neovim
                && got == 1
                && want == 2
        ));
    }

    #[test]
    fn startup_aggregation_rejects_sequence_drift_between_processes() {
        let contract = contract_with(vec![startup_cell("startup")]);
        let calibration = noise_calibration(&contract, 100, 106);
        let samples = [EngineLabel::Neovim, EngineLabel::Oxvim]
            .into_iter()
            .flat_map(|engine| {
                (0..2).flat_map(move |process_index| {
                    (0..2).map(move |sample_index| {
                        startup_sample(RUN, "startup", engine, process_index, sample_index, 100)
                    })
                })
            })
            .collect::<Vec<_>>();
        let marks = |label_two: &str| {
            vec![
                StartupStageMark {
                    ordinal: 0,
                    label: "parsing".to_owned(),
                    delta_us: 40,
                    clock_us: 40,
                },
                StartupStageMark {
                    ordinal: 1,
                    label: label_two.to_owned(),
                    delta_us: 60,
                    clock_us: 100,
                },
            ]
        };
        let mut startup = Vec::new();
        for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
            for process_index in 0..2 {
                let drifted = engine == EngineLabel::Neovim && process_index == 1;
                startup.push(StartupStageRun {
                    run_id: RUN.to_owned(),
                    workload: "startup".to_owned(),
                    engine,
                    process_index,
                    raw_source: "startuptime.json".to_owned(),
                    marks: marks(if drifted { "renamed" } else { "ui" }),
                });
            }
        }
        match summarize_stages(RUN, &contract, &samples, &startup, &calibration) {
            Err(ReportError::InvalidContract { reason }) => {
                assert!(
                    reason.contains("sequences must match"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("sequence drift must fail closed, got {other:?}"),
        }
    }
}
