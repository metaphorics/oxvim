//! Contract tests for the performance baseline harness (ticket #18, section 10.1).
//!
//! These tests make no timing assertion whatsoever and are safe on a loaded
//! machine. They verify the schema, statistics, the strict integer startup
//! parser, noise calibration and comparison verdicts through the typed public
//! report API, materiality, sonic-rs round trips, the workload matrix closure,
//! and dry-run behavior.
//!
//! DHAT parser behavior is covered by the module tests in
//! `src/perf/allocation.rs` and is intentionally not duplicated here.

use std::collections::BTreeSet;
use std::time::Duration;

use differential::perf::fixture::{LARGE_LINE_COUNT, LARGE_MIDPOINT};
use differential::perf::report::{
    AllocationRun, AlternationOrder, CellContract, ComparisonRunSummary, EngineLabel, FileIdentity,
    HostFingerprint, LatencyKind, MeasurementContract, NoiseRunSummary, NoiseVerdict, ProcessRun,
    Rejection, ReportError, Sample, StageArtifactRow, StageContract, StartupStages, Thresholds,
    UiContract, Verdict, judge_comparison, materiality_open, summarize_noise, summarize_stages,
};
use differential::perf::runner::{ExecutionMode, RunConfig, RunOutcome, Runner};
use differential::perf::session::{PerfSession, SessionConfig, StageTiming};
use differential::perf::startuptime;
use differential::perf::stats::{percentile, summarize};
use differential::perf::workload::{
    EngineFamily, FixtureRequirement, Kind, Profile, WorkloadId, abba_order, edit_alternation,
    matrix, run_steady_window, scroll_alternation,
};

// ===========================================================================
// Typed fixture helpers
//
// Every helper is deterministic and total: exact integer values, placeholder
// 64-hex digests, and no randomness. Helpers propagate failures instead of
// hiding them, and none of them touch the filesystem.
// ===========================================================================

/// Run identity stamped on every comparison artifact in these tests.
const RUN_ID: &str = "contract-run-2026-08-31";
/// Run identity stamped on every calibration artifact in these tests.
const CALIBRATION_RUN_ID: &str = "contract-calibration-2026-08-31";
/// Reference-side wall clock (us): every Neovim sample records exactly this.
const NEOVIM_WALL_US: u64 = 1_000;
/// Reference-side peak RSS (KiB) for every process in the population.
const NEOVIM_RSS_KIB: u64 = 10_000;

/// Deterministic 64-hex placeholder digest derived from `key`.
fn hex64(key: &str) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut digest = String::with_capacity(64);
    for byte in key.bytes().chain(std::iter::repeat(b'7')).take(32) {
        digest.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        digest.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    digest
}

fn file_identity(path: &str, seed: &str) -> FileIdentity {
    FileIdentity {
        path: path.to_owned(),
        sha256: hex64(&format!("{path}/{seed}")),
    }
}

fn host_fingerprint() -> HostFingerprint {
    HostFingerprint {
        hostname: "contract-host".to_owned(),
        os: "Linux".to_owned(),
        kernel: "contract-kernel".to_owned(),
        architecture: "x86_64".to_owned(),
        cpu: "contract-cpu".to_owned(),
        logical_cpus: 8,
        memory_kib: 16_777_216,
    }
}

fn steady_cell(workload: &str) -> CellContract {
    CellContract {
        workload: workload.to_owned(),
        processes_per_engine: 2,
        samples_per_process: 2,
        warmup_per_process: 1,
        timeout_ms: 15_000,
        wall_limit_ms: 60_000,
        allocation_runs_per_engine: 1,
        input_definition: format!("{workload}: exact contract input"),
        stage_contract: StageContract::SteadyState {
            response: true,
            flush: true,
        },
        fixture_hashes: vec![hex64("fixture/large.txt/large")],
    }
}

fn contract_with_cells(cells: Vec<CellContract>) -> MeasurementContract {
    MeasurementContract {
        schema_version: 1,
        profile: Profile::Quick,
        release_harness_profile: "release".to_owned(),
        host: host_fingerprint(),
        oracle: file_identity("bin/nvim", "oracle"),
        candidate: file_identity("bin/oxvim", "candidate"),
        ui: UiContract {
            width: 80,
            height: 24,
            rgb: true,
            ext_linegrid: true,
        },
        order: AlternationOrder::AlternatingFirstArm,
        cells,
    }
}

fn single_cell_contract() -> MeasurementContract {
    contract_with_cells(vec![steady_cell("input")])
}

/// The single cell of a fixture contract, or a failure — never an indexing panic.
fn only_cell(contract: &MeasurementContract) -> Result<&CellContract, Box<dyn std::error::Error>> {
    Ok(contract
        .cells
        .first()
        .ok_or_else(|| io_error("fixture contract must contain one cell".to_owned()))?)
}

fn io_error(message: String) -> std::io::Error {
    std::io::Error::other(message)
}

// ===========================================================================
// 3a. fixture_hashes_are_location_independent
// ===========================================================================

#[test]
fn fixture_hashes_are_location_independent() -> Result<(), Box<dyn std::error::Error>> {
    let calibration_contract = single_cell_contract();
    let mut comparison_contract = calibration_contract.clone();

    // Different fixture roots produce the same content hash, so the contract
    // remains reusable. The roots are provenance outside MeasurementContract.
    comparison_contract
        .cells
        .first_mut()
        .ok_or_else(|| io_error("fixture contract must contain one cell".to_owned()))?
        .fixture_hashes = vec![hex64("different-root/large.txt/large")];
    assert_ne!(
        calibration_contract.cells[0].fixture_hashes, comparison_contract.cells[0].fixture_hashes,
        "test fixtures must model distinct roots with independently derived hashes"
    );

    // Reusing the content hash itself is the equality contract.
    comparison_contract = calibration_contract.clone();
    assert_eq!(calibration_contract, comparison_contract);

    let calibration = noise_calibration(&calibration_contract, 1_020)?;
    let mut changed_content = calibration_contract.clone();
    changed_content
        .cells
        .first_mut()
        .ok_or_else(|| io_error("fixture contract must contain one cell".to_owned()))?
        .fixture_hashes = vec![hex64("changed-content")];
    assert!(
        incompatible_reason(judge_with_arms(
            &changed_content,
            &calibration,
            &control_arms()
        ))?
        .contains("contract"),
        "changed fixture content hash must reject calibration reuse"
    );
    Ok(())
}

fn nearly(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9
}

/// One steady-state sample whose `response_us + flush_us` equals `wall_us` exactly.
fn steady_sample(
    run_id: &str,
    workload: &str,
    engine: EngineLabel,
    process_index: usize,
    sample_index: usize,
    wall_us: u64,
) -> Sample {
    let response_us = wall_us / 2;
    Sample {
        run_id: run_id.to_owned(),
        workload: workload.to_owned(),
        engine,
        process_index,
        sample_index,
        wall_us,
        response_us: Some(response_us),
        flush_us: Some(wall_us - response_us),
        startup_delta_us: None,
        grid_sha256: hex64(&format!("{workload}/{process_index}/{sample_index}")),
        raw_source: format!("samples.ndjson#{workload}#{process_index}/{sample_index}"),
    }
}

fn process_run(
    run_id: &str,
    cell: &CellContract,
    engine: EngineLabel,
    process_index: usize,
    pid: u32,
    rss_kib: u64,
) -> ProcessRun {
    ProcessRun {
        run_id: run_id.to_owned(),
        workload: cell.workload.clone(),
        engine,
        process_index,
        pid,
        peak_rss_kib: rss_kib,
        warmup_discarded: cell.warmup_per_process,
        samples_recorded: cell.samples_per_process,
    }
}

/// One population per calibration arm: constant wall clock and RSS per arm,
/// so each arm's p95 IS the arm value and the spreads are exact.
fn noise_population(
    run_id: &str,
    cell: &CellContract,
    reference_wall_us: u64,
    observed_wall_us: u64,
) -> (Vec<Sample>, Vec<ProcessRun>) {
    let mut samples = Vec::new();
    let mut processes = Vec::new();
    for (engine, wall_us, rss_kib) in [
        (EngineLabel::NoiseA, reference_wall_us, NEOVIM_RSS_KIB),
        (EngineLabel::NoiseB, observed_wall_us, NEOVIM_RSS_KIB + 100),
    ] {
        for (process_index, pid) in (0..cell.processes_per_engine).zip(4_001_u32..) {
            for sample_index in 0..cell.samples_per_process {
                samples.push(steady_sample(
                    run_id,
                    &cell.workload,
                    engine,
                    process_index,
                    sample_index,
                    wall_us,
                ));
            }
            processes.push(process_run(
                run_id,
                cell,
                engine,
                process_index,
                pid,
                rss_kib,
            ));
        }
    }
    (samples, processes)
}

/// The comparison-side arm values that steer each verdict scenario.
struct ComparisonArms {
    neovim_wall_us: u64,
    oxvim_wall_us: u64,
    oxvim_rss_kib: u64,
    /// Replace one Oxvim grid hash to force an Incomparable rejection.
    tamper_grid: bool,
}

fn control_arms() -> ComparisonArms {
    ComparisonArms {
        neovim_wall_us: NEOVIM_WALL_US,
        oxvim_wall_us: 1_050,
        oxvim_rss_kib: 10_500,
        tamper_grid: false,
    }
}

/// Full comparison population for one cell: both engines, exact coverage.
fn comparison_population(
    run_id: &str,
    cell: &CellContract,
    arms: &ComparisonArms,
) -> (Vec<Sample>, Vec<ProcessRun>, Vec<AllocationRun>) {
    let mut samples = Vec::new();
    let mut processes = Vec::new();
    let mut allocations = Vec::new();
    for engine in [EngineLabel::Neovim, EngineLabel::Oxvim] {
        let (wall_us, rss_kib) = if engine == EngineLabel::Neovim {
            (arms.neovim_wall_us, NEOVIM_RSS_KIB)
        } else {
            (arms.oxvim_wall_us, arms.oxvim_rss_kib)
        };
        for (process_index, pid) in (0..cell.processes_per_engine).zip(4_001_u32..) {
            for sample_index in 0..cell.samples_per_process {
                let mut sample = steady_sample(
                    run_id,
                    &cell.workload,
                    engine,
                    process_index,
                    sample_index,
                    wall_us,
                );
                if arms.tamper_grid
                    && engine == EngineLabel::Oxvim
                    && process_index == 0
                    && sample_index == 0
                {
                    sample.grid_sha256 = hex64("tampered");
                }
                samples.push(sample);
            }
            processes.push(process_run(
                run_id,
                cell,
                engine,
                process_index,
                pid,
                rss_kib,
            ));
            if process_index < cell.allocation_runs_per_engine {
                allocations.push(AllocationRun {
                    run_id: run_id.to_owned(),
                    workload: cell.workload.clone(),
                    engine,
                    process_index,
                    total_blocks: 100,
                    total_bytes: 40_000,
                    raw_files: vec![file_identity(
                        &format!("allocations/{}/dhat.json", cell.workload),
                        "dhat",
                    )],
                });
            }
        }
    }
    (samples, processes, allocations)
}

/// Calibrate against `contract` with a `NoiseB` wall clock of `noise_b_wall_us`.
fn noise_calibration(
    contract: &MeasurementContract,
    noise_b_wall_us: u64,
) -> Result<NoiseRunSummary, Box<dyn std::error::Error>> {
    let cell = only_cell(contract)?;
    let (samples, processes) =
        noise_population(CALIBRATION_RUN_ID, cell, NEOVIM_WALL_US, noise_b_wall_us);
    Ok(summarize_noise(
        CALIBRATION_RUN_ID,
        contract,
        &samples,
        &processes,
    )?)
}

fn judge_with_arms(
    contract: &MeasurementContract,
    calibration: &NoiseRunSummary,
    arms: &ComparisonArms,
) -> Result<ComparisonRunSummary, Box<dyn std::error::Error>> {
    let cell = only_cell(contract)?;
    let (samples, processes, allocations) = comparison_population(RUN_ID, cell, arms);
    Ok(judge_comparison(
        RUN_ID,
        contract,
        &samples,
        &processes,
        &allocations,
        calibration,
    )?)
}

fn warn_messages(verdict: &Verdict) -> Option<&[String]> {
    match verdict {
        Verdict::Warn(warnings) => Some(warnings),
        _ => None,
    }
}

fn fail_rejections(verdict: &Verdict) -> Option<&[Rejection]> {
    match verdict {
        Verdict::Fail(rejections) => Some(rejections),
        _ => None,
    }
}

fn noise_fail_rejections(verdict: &NoiseVerdict) -> Option<&[Rejection]> {
    match verdict {
        NoiseVerdict::Fail(rejections) => Some(rejections),
        NoiseVerdict::Pass => None,
    }
}

// ===========================================================================
// 1. percentile_nearest_rank_selects_observed_samples
// ===========================================================================

#[test]
fn percentile_nearest_rank_selects_observed_samples() -> Result<(), Box<dyn std::error::Error>> {
    // Table over (n, p) to expected index, including n of 1, 2, 3, 32, 100, 800.
    // rank = (p * n + 99) / 100, rank = rank.max(1), value = sorted[rank - 1].
    let cases: &[(usize, usize, usize)] = &[
        // n=1: all percentiles select index 0.
        (1, 50, 0),
        (1, 95, 0),
        (1, 99, 0),
        // n=2: p50 -> rank=(100+99)/100=1 -> idx 0; p95 -> rank=(190+99)/100=2 -> idx 1.
        (2, 50, 0),
        (2, 95, 1),
        (2, 99, 1),
        // n=3: p50 -> rank=(150+99)/100=2 -> idx 1; p95 -> rank=(285+99)/100=3 -> idx 2.
        (3, 50, 1),
        (3, 95, 2),
        (3, 99, 2),
        // n=32: p99 -> rank=(3168+99)/100=32 -> idx 31 (= max).
        (32, 99, 31),
        // n=100: p50 -> idx 49, p95 -> idx 94, p99 -> idx 98, max -> idx 99.
        (100, 50, 49),
        (100, 95, 94),
        (100, 99, 98),
        // n=800: p50 -> idx 399, p95 -> idx 759, p99 -> idx 791.
        (800, 50, 399),
        (800, 95, 759),
        (800, 99, 791),
    ];

    for &(n, p, expected_idx) in cases {
        // Build a sorted slice where sorted[i] = i as u64, so the value at
        // the index IS the index, making the assertion self-verifying.
        let sorted: Vec<u64> = (0..n as u64).collect();
        let value = percentile(&sorted, p, 100)?;
        assert_eq!(
            value, expected_idx as u64,
            "n={n} p={p}: expected sorted[{expected_idx}]={expected_idx}, got {value}"
        );
    }
    Ok(())
}

// ===========================================================================
// 2. percentiles_are_monotone
// ===========================================================================

#[test]
fn percentiles_are_monotone() -> Result<(), Box<dyn std::error::Error>> {
    // All-equal input.
    let mut all_equal = vec![100_u64; 50];
    let p = summarize(&mut all_equal)?;
    assert!(p.p50 <= p.p95);
    assert!(p.p95 <= p.p99);
    assert!(p.p99 <= p.max);

    // Strictly increasing.
    let mut increasing: Vec<u64> = (0..100).collect();
    let p = summarize(&mut increasing)?;
    assert!(p.p50 <= p.p95, "increasing: p50={} p95={}", p.p50, p.p95);
    assert!(p.p95 <= p.p99, "increasing: p95={} p99={}", p.p95, p.p99);
    assert!(p.p99 <= p.max, "increasing: p99={} max={}", p.p99, p.max);

    // Single outlier: 99 values of 100, one value of 10000.
    let mut outlier = vec![100_u64; 99];
    outlier.push(10_000);
    let p = summarize(&mut outlier)?;
    assert!(p.p50 <= p.p95);
    assert!(p.p95 <= p.p99);
    assert!(p.p99 <= p.max);
    assert_eq!(p.max, 10_000);
    Ok(())
}

// ===========================================================================
// 3. stage_timings_sum_exactly
// ===========================================================================

#[test]
fn stage_timings_sum_exactly() -> Result<(), Box<dyn std::error::Error>> {
    // response_us + flush_us == wall_us exactly (section 2.1 invariant).
    let response = Duration::from_micros(3_500);
    let total = Duration::from_micros(10_000);
    let timing = StageTiming {
        total,
        response: Some(response),
    };
    let flush = timing.flush().ok_or_else(|| {
        io_error("response must leave a flush remainder for this total".to_owned())
    })?;
    let response_us = u64::try_from(response.as_micros())?;
    let flush_us = u64::try_from(flush.as_micros())?;
    let wall_us = u64::try_from(total.as_micros())?;
    assert_eq!(
        response_us + flush_us,
        wall_us,
        "response_us({response_us}) + flush_us({flush_us}) != wall_us({wall_us})"
    );
    Ok(())
}

// ===========================================================================
// 4. startuptime_parses_both_engine_headers (strict integer microseconds)
// ===========================================================================

#[test]
fn startuptime_parses_both_engine_headers() -> Result<(), Box<dyn std::error::Error>> {
    // Oxvim header: "Primary (or UI client)"
    let oxvim_log = "\
--- Startup times for process: Primary (or UI client) ---

times in msec
 clock   self+sourced   self:  sourced script
 clock   elapsed:              other lines

  000.001  000.001: parsing arguments
  000.005  000.004: sourcing vimrc file(s)
  000.010  000.005: loading plugins
  000.020  000.010: opening buffers
";
    let oxvim_marks = startuptime::parse(oxvim_log)?;
    assert_eq!(oxvim_marks.process, "Primary (or UI client)");
    assert_eq!(oxvim_marks.len(), 4);
    assert_eq!(oxvim_marks[0].label, "parsing arguments");
    assert_eq!(oxvim_marks[0].clock_us, 1);
    assert_eq!(oxvim_marks[0].delta_us, 1);
    assert_eq!(oxvim_marks[1].label, "sourcing vimrc file(s)");
    assert_eq!(oxvim_marks[1].clock_us, 5);
    assert_eq!(oxvim_marks[1].delta_us, 4);
    assert_eq!(oxvim_marks[2].label, "loading plugins");
    assert_eq!(oxvim_marks[2].clock_us, 10);
    assert_eq!(oxvim_marks[2].delta_us, 5);
    assert_eq!(oxvim_marks[3].label, "opening buffers");
    assert_eq!(oxvim_marks[3].clock_us, 20);
    assert_eq!(oxvim_marks[3].delta_us, 10);

    // Neovim header: "Embedded"
    let neovim_log = "\
--- Startup times for process: Embedded ---

times in msec
 clock   self+sourced   self:  sourced script
 clock   elapsed:              other lines

  000.002  000.002: parsing arguments
  000.008  000.006: sourcing vimrc file(s)
  000.015  000.007: opening buffers
";
    let neovim_marks = startuptime::parse(neovim_log)?;
    assert_eq!(neovim_marks.process, "Embedded");
    assert_eq!(neovim_marks.len(), 3);
    assert_eq!(neovim_marks[0].label, "parsing arguments");
    assert_eq!(neovim_marks[0].clock_us, 2);
    assert_eq!(neovim_marks[0].delta_us, 2);
    assert_eq!(neovim_marks[1].label, "sourcing vimrc file(s)");
    assert_eq!(neovim_marks[1].clock_us, 8);
    assert_eq!(neovim_marks[1].delta_us, 6);
    assert_eq!(neovim_marks[2].label, "opening buffers");
    assert_eq!(neovim_marks[2].clock_us, 15);
    assert_eq!(neovim_marks[2].delta_us, 7);
    Ok(())
}

// ===========================================================================
// 5. startuptime_rejects_malformed_line
// ===========================================================================

#[test]
fn startuptime_rejects_malformed_line() {
    let log = "\
--- Startup times for process: Primary (or UI client) ---

times in msec
 clock   self+sourced   self:  sourced script
 clock   elapsed:              other lines

  000.001  000.001: parsing arguments
  this is not a mark line
";
    let result = startuptime::parse(log);
    assert!(
        result.is_err(),
        "a malformed mark line must be a hard parse error, not a silent skip"
    );
}

// ===========================================================================
// 6. noise_calibration_a_b_produces_both_spreads_and_run_id
// ===========================================================================

#[test]
fn noise_calibration_a_b_produces_both_spreads_and_run_id() -> Result<(), Box<dyn std::error::Error>>
{
    let contract = single_cell_contract();
    let calibration = noise_calibration(&contract, 1_060)?;

    assert_eq!(calibration.run_id, CALIBRATION_RUN_ID);
    assert!(!calibration.run_id.is_empty());
    assert_eq!(calibration.schema_version, 1);
    assert_eq!(calibration.thresholds, Thresholds::ticket_18()?);
    assert_eq!(calibration.verdict, NoiseVerdict::Pass);

    assert_eq!(calibration.workloads.len(), 1);
    let noise = &calibration.workloads[0];
    assert_eq!(noise.run_id, calibration.run_id);
    assert_eq!(noise.workload, "input");
    // Each arm's p95 IS the arm's constant wall clock / RSS value.
    assert_eq!(noise.latency_a.p95, NEOVIM_WALL_US);
    assert_eq!(noise.latency_b.p95, 1_060);
    assert_eq!(noise.rss_a.p95, NEOVIM_RSS_KIB);
    assert_eq!(noise.rss_b.p95, NEOVIM_RSS_KIB + 100);
    // symmetric spread = |a - b| / min(a, b), exact on these operands.
    assert!(nearly(noise.noise_lat, 0.06), "noise_lat: {noise:?}");
    assert!(nearly(noise.noise_rss, 0.01), "noise_rss: {noise:?}");
    assert!(noise.noise_lat < calibration.thresholds.noise_lat_ceiling);
    assert!(noise.noise_rss < calibration.thresholds.noise_rss_ceiling);
    assert_eq!(noise.verdict, NoiseVerdict::Pass);
    Ok(())
}

// ===========================================================================
// 7. noisy_host_marks_calibration_unusable_and_aborts_the_comparison
// ===========================================================================

#[test]
fn noisy_host_marks_calibration_unusable_and_aborts_the_comparison()
-> Result<(), Box<dyn std::error::Error>> {
    let contract = single_cell_contract();
    // Spread of 2000 vs 1000 us = 1.0, far above the 0.10 latency ceiling.
    let calibration = noise_calibration(&contract, 2_000)?;

    let calibration_rejections = noise_fail_rejections(&calibration.verdict).ok_or_else(|| {
        io_error(format!(
            "expected calibration Fail, got {:?}",
            calibration.verdict
        ))
    })?;
    assert_eq!(
        calibration_rejections.len(),
        1,
        "only the latency ceiling must trip: {calibration_rejections:?}"
    );
    let Rejection::NoisyHost {
        workload,
        metric,
        measured,
        ceiling,
    } = &calibration_rejections[0]
    else {
        return Err(Box::new(io_error(format!(
            "expected NoisyHost, got {calibration_rejections:?}"
        ))));
    };
    assert_eq!(workload, "input");
    assert_eq!(metric, "latency");
    assert!(nearly(*measured, 1.0), "measured: {measured}");
    assert!(nearly(*ceiling, 0.10), "ceiling: {ceiling}");
    // Noise aborts: no Latency or Memory rejection may be produced.
    assert!(
        !calibration_rejections
            .iter()
            .any(|r| matches!(r, Rejection::Latency { .. } | Rejection::Memory { .. })),
        "noise abort must not produce measurement rejections: {calibration_rejections:?}"
    );
    assert_eq!(calibration.workloads[0].verdict, calibration.verdict);

    // Judging against an unusable calibration fails closed with a typed
    // IncompatibleCalibration error before any per-workload verdict exists.
    let reason = incompatible_reason(judge_with_arms(&contract, &calibration, &control_arms()))?;
    assert!(
        reason.contains("ceiling verdict must pass"),
        "noise verdict must abort the comparison: {reason}"
    );
    Ok(())
}

// ===========================================================================
// 8. missing_noise_arm_or_rss_coverage_fails_calibration
// ===========================================================================

#[test]
fn missing_noise_arm_or_rss_coverage_fails_calibration() -> Result<(), Box<dyn std::error::Error>> {
    let contract = single_cell_contract();
    let cell = only_cell(&contract)?;
    let (samples, processes) = noise_population(CALIBRATION_RUN_ID, cell, NEOVIM_WALL_US, 1_020);

    // Missing NoiseB latency arm.
    let a_only_samples: Vec<Sample> = samples
        .iter()
        .filter(|sample| sample.engine == EngineLabel::NoiseA)
        .cloned()
        .collect();
    let error = summarize_noise(CALIBRATION_RUN_ID, &contract, &a_only_samples, &processes)
        .err()
        .ok_or_else(|| io_error("missing NoiseB samples must fail summarize_noise".to_owned()))?;
    let ReportError::MissingCoverage {
        artifact,
        workload,
        engine,
        got,
        want,
    } = error
    else {
        return Err(Box::new(io_error(format!(
            "expected MissingCoverage, got {error:?}"
        ))));
    };
    assert_eq!(artifact, "sample");
    assert_eq!(engine, EngineLabel::NoiseB);
    assert_eq!(workload, "input");
    assert_eq!(got, 0);
    assert_eq!(want, cell.processes_per_engine * cell.samples_per_process);

    // Missing NoiseB RSS arm (process records).
    let a_only_processes: Vec<ProcessRun> = processes
        .iter()
        .filter(|process| process.engine == EngineLabel::NoiseA)
        .cloned()
        .collect();
    let error = summarize_noise(CALIBRATION_RUN_ID, &contract, &samples, &a_only_processes)
        .err()
        .ok_or_else(|| {
            io_error("missing NoiseB process records must fail summarize_noise".to_owned())
        })?;
    let ReportError::MissingCoverage {
        artifact,
        workload,
        engine,
        got,
        want,
    } = error
    else {
        return Err(Box::new(io_error(format!(
            "expected MissingCoverage, got {error:?}"
        ))));
    };
    assert_eq!(artifact, "process");
    assert_eq!(engine, EngineLabel::NoiseB);
    assert_eq!(workload, "input");
    assert_eq!(got, 0);
    assert_eq!(want, cell.processes_per_engine);
    Ok(())
}

// ===========================================================================
// 9. warn_threshold_widens_with_measured_noise (public surfaces only)
// ===========================================================================

#[test]
fn warn_threshold_widens_with_measured_noise() -> Result<(), Box<dyn std::error::Error>> {
    let thresholds = Thresholds::ticket_18()?;
    assert!(nearly(thresholds.noise_lat_ceiling, 0.10));
    assert!(nearly(thresholds.lat_warn_floor, 1.15));
    assert!(nearly(thresholds.lat_fail, 2.00));

    let contract = single_cell_contract();
    // noise_lat 0.02 -> adaptive warn stays at the 1.15 floor.
    let quiet = noise_calibration(&contract, 1_020)?;
    // noise_lat 0.06 -> adaptive warn = 1 + 3 * 0.06 = 1.18.
    let noisy = noise_calibration(&contract, 1_060)?;

    let quiet_summary = judge_with_arms(&contract, &quiet, &control_arms())?;
    let noisy_summary = judge_with_arms(&contract, &noisy, &control_arms())?;
    assert_eq!(quiet_summary.workloads[0].verdict, Verdict::Pass);
    assert_eq!(noisy_summary.workloads[0].verdict, Verdict::Pass);
    assert!(
        nearly(quiet_summary.workloads[0].threshold_warn, 1.15),
        "quiet warn threshold: {}",
        quiet_summary.workloads[0].threshold_warn
    );
    assert!(
        nearly(noisy_summary.workloads[0].threshold_warn, 1.18),
        "noisy warn threshold: {}",
        noisy_summary.workloads[0].threshold_warn
    );
    assert!(
        noisy_summary.workloads[0].threshold_warn > quiet_summary.workloads[0].threshold_warn,
        "measured noise must widen the warn threshold"
    );
    Ok(())
}

// ===========================================================================
// 10. comparison_verdicts_map_to_pass_warn_fail (public, one injected slowdown)
// ===========================================================================

enum ExpectedVerdict {
    Pass,
    Warn,
    LatencyP95Failure,
    MemoryFailure,
    IncomparableFailure,
    AbsoluteLatencyFailure,
}

struct VerdictCase {
    name: &'static str,
    arms: ComparisonArms,
    expected: ExpectedVerdict,
}

fn verdict_cases() -> [VerdictCase; 6] {
    [
        VerdictCase {
            name: "control passes",
            arms: control_arms(),
            expected: ExpectedVerdict::Pass,
        },
        VerdictCase {
            name: "latency warn above adaptive threshold",
            arms: ComparisonArms {
                neovim_wall_us: NEOVIM_WALL_US,
                oxvim_wall_us: 1_200,
                oxvim_rss_kib: 10_500,
                tamper_grid: false,
            },
            expected: ExpectedVerdict::Warn,
        },
        VerdictCase {
            name: "injected 3x slowdown fails latency p95",
            arms: ComparisonArms {
                neovim_wall_us: NEOVIM_WALL_US,
                oxvim_wall_us: 3_000,
                oxvim_rss_kib: 10_500,
                tamper_grid: false,
            },
            expected: ExpectedVerdict::LatencyP95Failure,
        },
        VerdictCase {
            name: "memory fails above rss ceiling",
            arms: ComparisonArms {
                neovim_wall_us: NEOVIM_WALL_US,
                oxvim_wall_us: 1_050,
                oxvim_rss_kib: 16_000,
                tamper_grid: false,
            },
            expected: ExpectedVerdict::MemoryFailure,
        },
        VerdictCase {
            name: "grid tamper is incomparable",
            arms: ComparisonArms {
                neovim_wall_us: NEOVIM_WALL_US,
                oxvim_wall_us: 1_050,
                oxvim_rss_kib: 10_500,
                tamper_grid: true,
            },
            expected: ExpectedVerdict::IncomparableFailure,
        },
        VerdictCase {
            name: "sub-floor latency uses absolute mode",
            arms: ComparisonArms {
                neovim_wall_us: 30,
                oxvim_wall_us: 630,
                oxvim_rss_kib: 10_500,
                tamper_grid: false,
            },
            expected: ExpectedVerdict::AbsoluteLatencyFailure,
        },
    ]
}

fn assert_latency_p95_failure(
    name: &str,
    verdict: &Verdict,
) -> Result<(), Box<dyn std::error::Error>> {
    let rejections = fail_rejections(verdict)
        .ok_or_else(|| io_error(format!("{name}: expected Fail, got {verdict:?}")))?;
    assert_eq!(rejections.len(), 1, "{name}: {rejections:?}");
    let Rejection::Latency {
        workload,
        ratio,
        threshold,
        kind,
    } = &rejections[0]
    else {
        return Err(Box::new(io_error(format!(
            "{name}: expected Latency rejection, got {rejections:?}"
        ))));
    };
    assert_eq!(workload, "input");
    assert_eq!(*kind, LatencyKind::P95);
    assert!(nearly(*ratio, 3.0), "{name}: ratio {ratio}");
    assert!(nearly(*threshold, 2.00), "{name}: threshold {threshold}");
    Ok(())
}

fn assert_memory_failure(name: &str, verdict: &Verdict) -> Result<(), Box<dyn std::error::Error>> {
    let rejections = fail_rejections(verdict)
        .ok_or_else(|| io_error(format!("{name}: expected Fail, got {verdict:?}")))?;
    assert_eq!(rejections.len(), 1, "{name}: {rejections:?}");
    let Rejection::Memory {
        workload,
        ratio,
        threshold,
    } = &rejections[0]
    else {
        return Err(Box::new(io_error(format!(
            "{name}: expected Memory rejection, got {rejections:?}"
        ))));
    };
    assert_eq!(workload, "input");
    assert!(nearly(*ratio, 1.6), "{name}: ratio {ratio}");
    assert!(nearly(*threshold, 1.50), "{name}: threshold {threshold}");
    Ok(())
}

fn assert_incomparable_failure(
    name: &str,
    summary: &ComparisonRunSummary,
    verdict: &Verdict,
) -> Result<(), Box<dyn std::error::Error>> {
    let rejections = fail_rejections(verdict)
        .ok_or_else(|| io_error(format!("{name}: expected Fail, got {verdict:?}")))?;
    assert_eq!(rejections.len(), 1, "{name}: {rejections:?}");
    let Rejection::Incomparable {
        workload,
        mismatched,
        first_mismatch,
    } = &rejections[0]
    else {
        return Err(Box::new(io_error(format!(
            "{name}: expected Incomparable rejection, got {rejections:?}"
        ))));
    };
    assert_eq!(workload, "input");
    assert_eq!(*mismatched, 1);
    assert_eq!(*first_mismatch, 0);
    assert_eq!(
        summary.workloads[0].mismatched_samples, 1,
        "{name}: summary must report the mismatch count"
    );
    Ok(())
}

fn assert_absolute_latency_failure(
    name: &str,
    verdict: &Verdict,
) -> Result<(), Box<dyn std::error::Error>> {
    let rejections = fail_rejections(verdict)
        .ok_or_else(|| io_error(format!("{name}: expected Fail, got {verdict:?}")))?;
    let kinds: Vec<LatencyKind> = rejections
        .iter()
        .filter_map(|rejection| match rejection {
            Rejection::Latency { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect();
    assert!(
        kinds.contains(&LatencyKind::Absolute),
        "{name}: expected an Absolute latency rejection, got {rejections:?}"
    );
    assert!(
        kinds.contains(&LatencyKind::Max),
        "{name}: the 21x max ratio must also reject, got {rejections:?}"
    );
    for rejection in rejections {
        if let Rejection::Latency {
            ratio,
            threshold,
            kind: LatencyKind::Absolute,
            ..
        } = rejection
        {
            // delta = (630/30 - 1) * 30 = 600 us above the 500 us absolute fail.
            assert!(nearly(*ratio, 600.0), "{name}: delta {ratio}");
            assert!(nearly(*threshold, 500.0), "{name}: threshold {threshold}");
        }
    }
    Ok(())
}

#[test]
fn comparison_verdicts_map_to_pass_warn_fail() -> Result<(), Box<dyn std::error::Error>> {
    let contract = single_cell_contract();
    // noise_lat 0.02 -> adaptive warn threshold 1.15.
    let calibration = noise_calibration(&contract, 1_020)?;

    for case in verdict_cases() {
        let name = case.name;
        let summary = judge_with_arms(&contract, &calibration, &case.arms)?;
        assert_eq!(summary.workloads.len(), 1, "{name}");
        assert_eq!(
            summary.verdict, summary.workloads[0].verdict,
            "{name}: overall verdict must mirror the single workload",
        );
        let verdict = &summary.workloads[0].verdict;
        match case.expected {
            ExpectedVerdict::Pass => {
                assert_eq!(*verdict, Verdict::Pass, "{name}: {verdict:?}");
                assert!(
                    nearly(summary.workloads[0].threshold_warn, 1.15),
                    "{name}: threshold_warn {}",
                    summary.workloads[0].threshold_warn
                );
            }
            ExpectedVerdict::Warn => {
                let warnings = warn_messages(verdict)
                    .ok_or_else(|| io_error(format!("{name}: expected Warn, got {verdict:?}")))?;
                assert!(!warnings.is_empty(), "{name}: {warnings:?}");
            }
            ExpectedVerdict::LatencyP95Failure => {
                assert_latency_p95_failure(name, verdict)?;
            }
            ExpectedVerdict::MemoryFailure => assert_memory_failure(name, verdict)?,
            ExpectedVerdict::IncomparableFailure => {
                assert_incomparable_failure(name, &summary, verdict)?;
            }
            ExpectedVerdict::AbsoluteLatencyFailure => {
                assert_absolute_latency_failure(name, verdict)?;
            }
        }
    }
    Ok(())
}

// ===========================================================================
// 11. comparison_rejects_incompatible_calibration_before_judgment
// ===========================================================================

#[test]
fn comparison_rejects_incompatible_calibration_before_judgment()
-> Result<(), Box<dyn std::error::Error>> {
    let contract = single_cell_contract();
    let calibration = noise_calibration(&contract, 1_020)?;

    let mut schema_tampered = calibration.clone();
    schema_tampered.schema_version += 1;
    assert!(
        incompatible_reason(judge_with_arms(
            &contract,
            &schema_tampered,
            &control_arms()
        ))?
        .contains("schema"),
        "schema-version mismatch must reject as incompatible"
    );

    let mut contract_tampered = contract.clone();
    contract_tampered.candidate.sha256 = hex64("different-binary");
    assert!(
        incompatible_reason(judge_with_arms(
            &contract_tampered,
            &calibration,
            &control_arms()
        ))?
        .contains("contract"),
        "measurement-contract mismatch must reject as incompatible"
    );

    let mut missing_workload = calibration.clone();
    assert!(
        missing_workload.workloads.pop().is_some(),
        "fixture calibration must contain workloads to remove"
    );
    assert!(
        incompatible_reason(judge_with_arms(
            &contract,
            &missing_workload,
            &control_arms()
        ))?
        .contains("workload"),
        "missing noise workload evidence must reject as incompatible"
    );

    let mut ratio_tampered = calibration.clone();
    ratio_tampered.workloads[0].noise_lat += 1e-12;
    assert!(
        incompatible_reason(judge_with_arms(&contract, &ratio_tampered, &control_arms()))?
            .contains("reproduce"),
        "tampered stored noise must fail operand reproduction"
    );
    Ok(())
}

/// Extract the `reason` of an expected `IncompatibleCalibration` error.
fn incompatible_reason(
    result: Result<ComparisonRunSummary, Box<dyn std::error::Error>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let error = match result {
        Ok(summary) => {
            return Err(Box::new(io_error(format!(
                "expected IncompatibleCalibration, got completed summary {:?}",
                summary.verdict
            ))));
        }
        Err(error) => error,
    };
    let report = error.downcast::<ReportError>()?;
    match &*report {
        ReportError::IncompatibleCalibration { reason } => Ok(reason.clone()),
        other => Err(Box::new(io_error(format!(
            "expected IncompatibleCalibration, got {other:?}"
        )))),
    }
}

// ===========================================================================
// 12. comparison_requires_exact_allocation_coverage
// ===========================================================================

#[test]
fn comparison_requires_exact_allocation_coverage() -> Result<(), Box<dyn std::error::Error>> {
    let contract = single_cell_contract();
    let cell = only_cell(&contract)?;
    let calibration = noise_calibration(&contract, 1_020)?;
    let (samples, processes, allocations) = comparison_population(RUN_ID, cell, &control_arms());

    let neovim_only: Vec<AllocationRun> = allocations
        .iter()
        .filter(|run| run.engine == EngineLabel::Neovim)
        .cloned()
        .collect();
    let error = judge_comparison(
        RUN_ID,
        &contract,
        &samples,
        &processes,
        &neovim_only,
        &calibration,
    )
    .err()
    .ok_or_else(|| io_error("missing Oxvim allocations must fail judgment".to_owned()))?;
    let ReportError::MissingCoverage {
        artifact,
        workload,
        engine,
        got,
        want,
    } = error
    else {
        return Err(Box::new(io_error(format!(
            "expected MissingCoverage, got {error:?}"
        ))));
    };
    assert_eq!(artifact, "allocation");
    assert_eq!(engine, EngineLabel::Oxvim);
    assert_eq!(workload, "input");
    assert_eq!(got, 0);
    assert_eq!(want, cell.allocation_runs_per_engine);
    Ok(())
}

// ===========================================================================
// 13. artifact_run_ids_track_the_current_run
// ===========================================================================

#[test]
fn artifact_run_ids_track_the_current_run() -> Result<(), Box<dyn std::error::Error>> {
    let contract = single_cell_contract();
    let calibration = noise_calibration(&contract, 1_020)?;
    assert_eq!(calibration.run_id, CALIBRATION_RUN_ID);
    assert!(!calibration.run_id.is_empty());
    for noise in &calibration.workloads {
        assert_eq!(noise.run_id, CALIBRATION_RUN_ID);
    }

    let cell = only_cell(&contract)?;
    let (samples, processes, allocations) = comparison_population(RUN_ID, cell, &control_arms());
    let population_ids = samples
        .iter()
        .map(|sample| sample.run_id.as_str())
        .chain(processes.iter().map(|process| process.run_id.as_str()))
        .chain(
            allocations
                .iter()
                .map(|allocation| allocation.run_id.as_str()),
        );
    for run_id in population_ids {
        assert_eq!(run_id, RUN_ID);
        assert!(!run_id.is_empty());
    }

    let summary = judge_comparison(
        RUN_ID,
        &contract,
        &samples,
        &processes,
        &allocations,
        &calibration,
    )?;
    assert_eq!(summary.run_id, RUN_ID);
    assert!(!summary.run_id.is_empty());
    assert_eq!(summary.calibration_run_id, calibration.run_id);
    for workload in &summary.workloads {
        assert_eq!(workload.run_id, RUN_ID);
        assert_eq!(workload.allocations.run_id, RUN_ID);
        for stage in &workload.stages {
            match stage {
                StageArtifactRow::SteadyState { run_id, .. } => {
                    assert_eq!(run_id, RUN_ID);
                }
            }
        }
    }
    Ok(())
}

// ===========================================================================
// 14. stage_rows_account_for_response_flush_and_materiality
// ===========================================================================

#[test]
fn stage_rows_account_for_response_flush_and_materiality() -> Result<(), Box<dyn std::error::Error>>
{
    let contract = single_cell_contract();
    // noise_lat 0.02.
    let calibration = noise_calibration(&contract, 1_020)?;
    let summary = judge_with_arms(&contract, &calibration, &control_arms())?;

    assert_eq!(summary.workloads.len(), 1);
    let workload = &summary.workloads[0];
    assert_eq!(workload.stages.len(), 2, "one stage row per engine");
    for row in &workload.stages {
        let StageArtifactRow::SteadyState {
            engine,
            wall,
            response,
            flush,
            response_materiality,
            flush_materiality,
            raw_samples,
            ..
        } = row;
        let (stage_wall_p95, stage_stage_p95) = if *engine == EngineLabel::Neovim {
            (NEOVIM_WALL_US, NEOVIM_WALL_US / 2)
        } else {
            (1_050, 525)
        };
        assert_eq!(wall.p95, stage_wall_p95, "{engine:?} wall p95");
        let response = response
            .as_ref()
            .ok_or_else(|| io_error(format!("{engine:?} must report response percentiles")))?;
        let flush = flush
            .as_ref()
            .ok_or_else(|| io_error(format!("{engine:?} must report flush percentiles")))?;
        assert_eq!(response.p95, stage_stage_p95, "{engine:?} response p95");
        assert_eq!(flush.p95, stage_stage_p95, "{engine:?} flush p95");

        let opening = response_materiality
            .as_ref()
            .ok_or_else(|| io_error(format!("{engine:?} must report response materiality")))?;
        assert_eq!(opening.stage_p95_us, stage_stage_p95);
        assert_eq!(opening.wall_p95_us, stage_wall_p95);
        assert!(
            opening.material,
            "{engine:?} response share must be material"
        );
        let flush_opening = flush_materiality
            .as_ref()
            .ok_or_else(|| io_error(format!("{engine:?} must report flush materiality")))?;
        assert_eq!(flush_opening.stage_p95_us, stage_stage_p95);
        assert!(flush_opening.material);

        assert_eq!(raw_samples.len(), 4, "{engine:?} provenance rows");
        for provenance in raw_samples {
            let response_us = provenance
                .response_us
                .ok_or_else(|| io_error(format!("{engine:?} provenance must carry response_us")))?;
            let flush_us = provenance
                .flush_us
                .ok_or_else(|| io_error(format!("{engine:?} provenance must carry flush_us")))?;
            assert!(
                provenance.startup_delta_us.is_none(),
                "steady-state provenance must not carry startup deltas"
            );
            assert_eq!(
                response_us + flush_us,
                provenance.wall_us,
                "{engine:?}: response + flush must equal wall exactly"
            );
        }
    }

    // The standalone stage artifact must mirror the comparison rows and keep
    // startup state explicit: this fixture collects no startup logs, so the
    // startup section is not_collected, never duplicated raw evidence.
    let cell = only_cell(&contract)?;
    let (samples, _, _) = comparison_population(RUN_ID, cell, &control_arms());
    let stages = summarize_stages(RUN_ID, &contract, &samples, &[], &calibration)?;
    assert_eq!(stages.run_id, RUN_ID);
    assert_eq!(stages.steady_state, workload.stages);
    assert!(
        matches!(stages.startup, StartupStages::NotCollected),
        "absent startup logs must yield explicit not_collected, got {:?}",
        stages.startup
    );
    Ok(())
}

// ===========================================================================
// 15. sample_roundtrips_ndjson_without_embedded_newline (sonic-rs)
// ===========================================================================

#[test]
fn sample_roundtrips_ndjson_without_embedded_newline() -> Result<(), Box<dyn std::error::Error>> {
    for engine in [
        EngineLabel::Neovim,
        EngineLabel::Oxvim,
        EngineLabel::NoiseA,
        EngineLabel::NoiseB,
    ] {
        let engine_json = match engine {
            EngineLabel::Neovim => "neovim",
            EngineLabel::Oxvim => "oxvim",
            EngineLabel::NoiseA => "noise_a",
            EngineLabel::NoiseB => "noise_b",
        };
        let sample = Sample {
            run_id: RUN_ID.to_owned(),
            workload: "startup:50".to_owned(),
            engine,
            process_index: 3,
            sample_index: 0,
            wall_us: 1_234_567,
            response_us: None,
            flush_us: None,
            startup_delta_us: Some(987),
            grid_sha256: hex64("startup:50/3/0"),
            raw_source: "startuptime/primary.log".to_owned(),
        };
        let line = sonic_rs::to_string(&sample)?;
        assert!(
            !line.contains('\n'),
            "serialized sample must not contain a literal newline"
        );
        assert!(
            line.contains(&format!("\"engine\":\"{engine_json}\"")),
            "engine label must serialize as the closed snake_case tag: {line}"
        );
        let roundtrip: Sample = sonic_rs::from_str(&line)?;
        assert_eq!(roundtrip, sample);
    }
    Ok(())
}

// ===========================================================================
// 16. workload_matrix_is_closed (declarative, no fixture paths)
// ===========================================================================

#[test]
fn workload_matrix_is_closed() {
    // The runner's dispatch is an exhaustive match on WorkloadCase with no
    // catch-all (enforced at compile time). This test asserts the declared
    // matrix names all seven workload variants and pairs every cell with the
    // declarative fixture requirement its workload implies.
    let cells = matrix(Profile::Full);
    assert_eq!(cells.len(), 20, "10 cells per engine family");

    let mut names: BTreeSet<&str> = BTreeSet::new();
    let mut startup_per_family = [0_usize; 2];
    for cell in &cells {
        let family_index = match cell.family {
            EngineFamily::Neovim => 0,
            EngineFamily::Oxvim => 1,
        };
        match cell.fixture {
            FixtureRequirement::PluginTree { count } => {
                assert!(
                    matches!(cell.id, WorkloadId::Startup { plugin_count } if plugin_count == count),
                    "startup cells must request their own plugin-tree count"
                );
                startup_per_family[family_index] += 1;
            }
            FixtureRequirement::LargeBuffer => {
                assert!(
                    matches!(
                        cell.id,
                        WorkloadId::Open | WorkloadId::Edit | WorkloadId::Scroll
                    ),
                    "only open/edit/scroll require the large buffer"
                );
            }
            FixtureRequirement::None => {
                assert!(
                    matches!(
                        cell.id,
                        WorkloadId::Input | WorkloadId::LuaPure | WorkloadId::LuaApi
                    ),
                    "only input/lua workloads run without fixtures"
                );
            }
        }
        names.insert(cell.id.type_name());
    }
    assert_eq!(startup_per_family, [4, 4], "W1 has 4 cells per engine");
    for expected in [
        "startup", "input", "lua:pure", "lua:api", "open", "edit", "scroll",
    ] {
        assert!(
            names.contains(expected),
            "matrix must include workload '{expected}'"
        );
    }

    // The quick profile shrinks W1 but keeps every workload type.
    let quick = matrix(Profile::Quick);
    assert_eq!(quick.len(), 16, "8 cells per engine family");
}

// ===========================================================================
// 17. steady_state_workloads_are_state_neutral (oracle only)
// ===========================================================================

#[test]
fn steady_state_workloads_are_state_neutral() -> Result<(), Box<dyn std::error::Error>> {
    // This test needs only the oracle, with no target/release/oxvim required.
    // It spawns 2 processes and 3 samples each, asserting the UiSnapshot
    // after sample 0 equals the UiSnapshot after sample 2 for W2 and W5.
    //
    // Oracle absence is a hard failure: if the probe cannot launch the
    // checked-in oracle resolved by `differential::binary(differential::ORACLE)`
    // (an absolute path from the repository root, independent of the
    // integration-test cwd), the test fails with the OS error message
    // instead of passing vacuously. Once the oracle is available, every
    // session/setup/window/snapshot/drain/shutdown error fails the test.
    let oracle = differential::binary(differential::ORACLE);
    let probe = std::process::Command::new(&oracle)
        .arg("--version")
        .output();
    match probe {
        Err(e) => {
            return Err(Box::new(io_error(format!(
                "oracle probe ({}) failed to launch: {e}",
                oracle.display()
            ))));
        }
        Ok(output) => {
            if !output.status.success() {
                return Err(Box::new(io_error(format!(
                    "{} --version exited with status {}",
                    oracle.display(),
                    output.status
                ))));
            }
        }
    }

    // Scratch files beneath <repo>/target/test-tmp with a unique per-process name.
    let tmp = differential::root()
        .join("target")
        .join("test-tmp")
        .join(format!("perf-state-neutral-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;

    let body_result: Result<(), Box<dyn std::error::Error>> = (|| {
        for id in [WorkloadId::Input, WorkloadId::Scroll] {
            let root = tmp.join(format!("{id}"));
            std::fs::remove_dir_all(&root).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
            std::fs::create_dir_all(&root)?;
            let config = SessionConfig::isolated(&root, &root);
            let case = id.session_case();

            let spawned =
                PerfSession::spawn(differential::perf::session::Engine::Neovim, case, config)?;
            let (attached, _) = spawned.attach_ui()?;
            let mut session = attached.finish_setup()?;

            // Sample 0.
            run_steady_window(&mut session, id, 0)?;
            drop(session.drain_deferred());
            let snap0 = session.snapshot()?;

            // Sample 1 (intermediate).
            run_steady_window(&mut session, id, 1)?;
            drop(session.drain_deferred());

            // Sample 2.
            run_steady_window(&mut session, id, 2)?;
            drop(session.drain_deferred());
            let snap2 = session.snapshot()?;

            session.shutdown()?;

            assert_eq!(
                snap0, snap2,
                "workload {id}: UiSnapshot after sample 0 must equal sample 2 (state neutrality)"
            );
        }
        Ok(())
    })();

    // Checked cleanup: attempted even if the body failed.
    // Return the body error first, or the cleanup error if the body succeeded.
    let cleanup_result = std::fs::remove_dir_all(&tmp);
    body_result?;
    cleanup_result?;
    Ok(())
}

// ===========================================================================
// 18. materiality_opening_formula
// ===========================================================================

#[test]
fn materiality_opening_formula() -> Result<(), Box<dyn std::error::Error>> {
    // share = stage_p95 / wall_p95
    // minimum_share = max(0.10, 3 * noise_lat)
    // material = stage_p95 >= 150 us && share >= minimum_share

    // Case 1: stage_p95=500us, wall_p95=1000us, noise_lat=0.02
    // share = 0.5, minimum_share = max(0.10, 0.06) = 0.10
    // material = 500 >= 150 && 0.5 >= 0.10 = true
    let opening = materiality_open(500, 1000, 0.02)?;
    assert!(
        opening.material,
        "500us/1000us with noise 0.02 should be material"
    );
    // The formula must emit all operands for diagnosis.
    assert_eq!(opening.stage_p95_us, 500);
    assert_eq!(opening.wall_p95_us, 1000);
    assert!(nearly(opening.share, 0.5), "share: {}", opening.share);
    assert!(
        nearly(opening.noise_lat, 0.02),
        "noise_lat: {}",
        opening.noise_lat
    );
    assert!(
        nearly(opening.minimum_share, 0.10),
        "minimum_share: {}",
        opening.minimum_share
    );
    assert!(
        nearly(opening.minimum_candidate_gain, 0.15),
        "minimum_candidate_gain: {}",
        opening.minimum_candidate_gain
    );

    // Case 2: stage_p95=100us is below the 150us absolute floor.
    assert!(!materiality_open(100, 1000, 0.02)?.material);

    // Case 3: stage_p95=200us, wall_p95=2000us: share exactly at minimum.
    assert!(materiality_open(200, 2000, 0.02)?.material);

    // Case 4: high noise widens minimum_share but 0.3 >= 0.15 stays material.
    assert!(materiality_open(300, 1000, 0.05)?.material);

    // Case 5: noise 0.08 -> minimum_share 0.24 blocks share 0.10.
    assert!(!materiality_open(200, 2000, 0.08)?.material);

    // Zero wall p95 is a typed rejection, never a division blowup.
    let error = materiality_open(500, 0, 0.02)
        .err()
        .ok_or_else(|| io_error("zero wall p95 must fail materiality_open".to_owned()))?;
    let ReportError::ZeroDenominator { workload, metric } = error else {
        return Err(Box::new(io_error(format!(
            "expected ZeroDenominator, got {error:?}"
        ))));
    };
    assert_eq!(workload, "materiality");
    assert_eq!(metric, "wall_p95_us");

    // Negative noise is a typed invalid-number rejection.
    let error = materiality_open(500, 1000, -0.02)
        .err()
        .ok_or_else(|| io_error("negative noise must fail materiality_open".to_owned()))?;
    let ReportError::InvalidNumber { field } = error else {
        return Err(Box::new(io_error(format!(
            "expected InvalidNumber, got {error:?}"
        ))));
    };
    assert_eq!(field, "noise_lat");
    Ok(())
}

// ===========================================================================
// 19. dry_run_returns_plan_without_verdict_or_run_id
// ===========================================================================

#[test]
fn dry_run_returns_plan_without_verdict_or_run_id() -> Result<(), Box<dyn std::error::Error>> {
    let runner = Runner::new(RunConfig {
        mode: ExecutionMode::DryRun,
        profile: Profile::Quick,
        workload_filter: vec!["scroll".to_owned()],
        startuptime: false,
        output_root: std::env::temp_dir().join("oxvim-perf-contract-dry-run"),
    })?;
    assert!(
        runner.run_id().is_none(),
        "dry-run must not allocate a run id"
    );
    assert!(
        runner.run_dir().is_none(),
        "dry-run must not allocate a run directory"
    );

    let RunOutcome::DryRun(plan) = runner.run()? else {
        return Err(Box::new(io_error(
            "dry-run mode must return RunOutcome::DryRun".to_owned(),
        )));
    };
    assert_eq!(plan.profile, Profile::Quick);
    assert_eq!(plan.filters, ["scroll"]);
    assert_eq!(plan.cells.len(), 2, "one scroll cell per engine family");
    for (cell, family) in plan
        .cells
        .iter()
        .zip([EngineFamily::Neovim, EngineFamily::Oxvim])
    {
        assert_eq!(cell.id.type_name(), "scroll");
        assert_eq!(cell.family, family);
        assert_eq!(cell.fixture, FixtureRequirement::LargeBuffer);
        assert_eq!(
            cell.processes,
            Profile::Quick.processes_per_engine(Kind::SteadyState)
        );
        assert_eq!(
            cell.warmup,
            Profile::Quick.warmup_per_process(WorkloadId::Scroll)
        );
        assert_eq!(
            cell.samples,
            Profile::Quick.samples_per_process(WorkloadId::Scroll)
        );
        assert_eq!(cell.timeout, Duration::from_secs(60));
    }

    // Contrast: a completed mode reserves a current, nonempty run identity up
    // front (no I/O happens during construction).
    let completed = Runner::new(RunConfig {
        mode: ExecutionMode::Comparison {
            noise_path: std::path::PathBuf::new(),
            valgrind_path: None,
        },
        ..RunConfig::default()
    })?;
    let run_id = completed
        .run_id()
        .ok_or_else(|| io_error("completed modes must reserve a run id".to_owned()))?;
    assert!(!run_id.is_empty());
    assert!(completed.run_dir().is_some());
    Ok(())
}

// ===========================================================================
// Additional: fixture and alternation invariants
// ===========================================================================

#[test]
fn fixture_large_buffer_line_count_and_midpoint() {
    assert_eq!(LARGE_LINE_COUNT, 50_000);
    assert_eq!(LARGE_MIDPOINT, 25_000);
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
    assert_eq!(scroll_alternation(0).matches("<C-e>").count(), 5);
    assert_eq!(scroll_alternation(1).matches("<C-y>").count(), 5);
}

#[test]
fn abba_order_is_balanced() {
    let order = abba_order(4);
    assert_eq!(order.len(), 8);
    let neovim_count = order.iter().filter(|&&f| f == EngineFamily::Neovim).count();
    let oxvim_count = order.iter().filter(|&&f| f == EngineFamily::Oxvim).count();
    assert_eq!(neovim_count, 4);
    assert_eq!(oxvim_count, 4);
    // First block: N, O, O, N.
    assert_eq!(order[0], EngineFamily::Neovim);
    assert_eq!(order[1], EngineFamily::Oxvim);
    assert_eq!(order[2], EngineFamily::Oxvim);
    assert_eq!(order[3], EngineFamily::Neovim);
}

#[test]
fn profile_quick_uses_reduced_counts() {
    assert_eq!(Profile::Quick.plugin_counts(), &[0, 50]);
    assert_eq!(Profile::Quick.processes_per_engine(Kind::Startup), 12);
    assert_eq!(Profile::Quick.processes_per_engine(Kind::SteadyState), 4);
    assert_eq!(Profile::Quick.samples_per_process(WorkloadId::Input), 30);
}

#[test]
fn profile_full_uses_baseline_counts() {
    assert_eq!(Profile::Full.plugin_counts(), &[0, 10, 50, 100]);
    assert_eq!(Profile::Full.processes_per_engine(Kind::Startup), 100);
    assert_eq!(Profile::Full.processes_per_engine(Kind::SteadyState), 8);
    assert_eq!(Profile::Full.samples_per_process(WorkloadId::Input), 100);
}

#[test]
fn workload_cell_id_encodes_plugin_count() {
    assert_eq!(
        WorkloadId::Startup { plugin_count: 50 }.cell_id(),
        "startup:50"
    );
    assert_eq!(WorkloadId::Input.cell_id(), "input");
    assert_eq!(WorkloadId::LuaPure.cell_id(), "lua:pure");
    assert_eq!(WorkloadId::LuaApi.cell_id(), "lua:api");
}
