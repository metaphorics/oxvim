#![allow(missing_docs)]

//! `cargo run -p differential --bin perf` — performance baseline harness.
//!
//! Mirrors the hand-rolled `std::env::args()` style of `bin/replay.rs`.
//! The default mode is the full cross-engine comparison, which requires the
//! `--noise` calibration artifact and a resolvable `valgrind` binary; every
//! configuration conflict is rejected before [`Runner::new`] runs. `--dry-run`
//! prints the deterministic expanded matrix and spawns no engine. All
//! `println!`/`eprintln!` here is this binary's own CLI output.

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use differential::perf::runner::{
    DryRunPlan, ExecutionMode, RunConfig, RunError, RunOutcome, Runner,
};
use differential::perf::workload::{EngineFamily, FixtureRequirement, Profile};
use differential::perf::{NoiseVerdict, RunSummary, Verdict};

fn main() -> ExitCode {
    match run() {
        Ok(exit) => exit,
        Err(error) => {
            eprintln!("perf: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Failure> {
    let Some(config) = parse_args().map_err(Failure::Config)? else {
        return Ok(ExitCode::SUCCESS);
    };
    let mode = match config.mode {
        ExecutionMode::Comparison { .. } => "comparison",
        ExecutionMode::NoiseCalibration => "noise_calibration",
        ExecutionMode::DryRun => "dry_run",
    };

    let runner = Runner::new(config).map_err(Failure::Run)?;
    match runner.run().map_err(Failure::Run)? {
        RunOutcome::DryRun(plan) => {
            print_dry_run(&plan);
            Ok(ExitCode::SUCCESS)
        }
        RunOutcome::Completed(summary) => Ok(report_completed(mode, &runner, &summary)),
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Parse `std::env::args()` into a [`RunConfig`].
///
/// Returns `Ok(None)` after printing `--help`. Every structural rejection —
/// unknown options, missing values, mode conflicts, missing required values,
/// and valgrind resolution — happens here, before [`Runner::new`]. Workload
/// filter names are validated by the runner itself into typed
/// [`RunError::EmptyFilter`], [`RunError::DuplicateFilter`], and
/// [`RunError::UnknownFilter`] errors.
fn parse_args() -> Result<Option<RunConfig>, ConfigError> {
    decode_args()?.map(build_run_config).transpose()
}

#[derive(Default)]
struct ParsedArguments {
    profile: Option<Profile>,
    workload_filter: Vec<String>,
    output_root: Option<PathBuf>,
    noise: Option<PathBuf>,
    valgrind: Option<String>,
    dry_run: bool,
    noise_only: bool,
    startuptime: bool,
}

fn decode_args() -> Result<Option<ParsedArguments>, ConfigError> {
    let mut parsed = ParsedArguments::default();
    let mut args = env::args().skip(1);

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--profile" => {
                let value = args.next().ok_or(ConfigError::MissingValue {
                    option: "--profile",
                })?;
                parsed.profile = Some(match value.as_str() {
                    "full" => Profile::Full,
                    "quick" => Profile::Quick,
                    other => {
                        return Err(ConfigError::UnknownProfile {
                            value: other.to_owned(),
                        });
                    }
                });
            }
            "--workload" => {
                let value = args.next().ok_or(ConfigError::MissingValue {
                    option: "--workload",
                })?;
                // Repeating the flag merges filter lists; the runner rejects
                // empty, duplicate, and unknown names across the merged list.
                parsed
                    .workload_filter
                    .extend(value.split(',').map(|name| name.trim().to_owned()));
            }
            "--output" => {
                let value = args
                    .next()
                    .ok_or(ConfigError::MissingValue { option: "--output" })?;
                parsed.output_root = Some(PathBuf::from(value));
            }
            "--noise" => {
                let value = args
                    .next()
                    .ok_or(ConfigError::MissingValue { option: "--noise" })?;
                parsed.noise = Some(PathBuf::from(value));
            }
            "--valgrind" => {
                parsed.valgrind = Some(args.next().ok_or(ConfigError::MissingValue {
                    option: "--valgrind",
                })?);
            }
            "--dry-run" => parsed.dry_run = true,
            "--noise-only" => parsed.noise_only = true,
            "--startuptime" => parsed.startuptime = true,
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            other if other.starts_with('-') => {
                return Err(ConfigError::UnknownOption(other.to_owned()));
            }
            other => {
                return Err(ConfigError::UnexpectedPositional(other.to_owned()));
            }
        }
    }

    Ok(Some(parsed))
}

fn build_run_config(parsed: ParsedArguments) -> Result<RunConfig, ConfigError> {
    let mut config = RunConfig::default();
    if parsed.dry_run {
        if parsed.noise.is_some() {
            return Err(ConfigError::Conflict {
                option: "--dry-run",
                with: "--noise",
            });
        }
        if parsed.noise_only {
            return Err(ConfigError::Conflict {
                option: "--dry-run",
                with: "--noise-only",
            });
        }
        if parsed.startuptime {
            return Err(ConfigError::Conflict {
                option: "--dry-run",
                with: "--startuptime",
            });
        }
        if parsed.valgrind.is_some() {
            return Err(ConfigError::Conflict {
                option: "--dry-run",
                with: "--valgrind",
            });
        }
        config.mode = ExecutionMode::DryRun;
    } else if parsed.noise_only {
        if parsed.noise.is_some() {
            return Err(ConfigError::Conflict {
                option: "--noise-only",
                with: "--noise",
            });
        }
        if parsed.startuptime {
            return Err(ConfigError::Conflict {
                option: "--noise-only",
                with: "--startuptime",
            });
        }
        if parsed.valgrind.is_some() {
            return Err(ConfigError::Conflict {
                option: "--noise-only",
                with: "--valgrind",
            });
        }
        config.mode = ExecutionMode::NoiseCalibration;
    } else {
        // Measured comparison: the noise artifact is mandatory and the DHAT
        // profiler must resolve to an absolute path before any work starts.
        let noise_path = parsed.noise.ok_or(ConfigError::NoiseRequired)?;
        let valgrind_path =
            match parsed.valgrind.as_deref() {
                Some(value) => Some(resolve_valgrind(value)?),
                None => Some(resolve_command("valgrind").ok_or_else(|| {
                    ConfigError::ValgrindNotFound {
                        name: "valgrind".to_owned(),
                    }
                })?),
            };
        config.mode = ExecutionMode::Comparison {
            noise_path,
            valgrind_path,
        };
    }

    config.profile = parsed.profile.unwrap_or(Profile::Full);
    config.workload_filter = parsed.workload_filter;
    config.startuptime = parsed.startuptime;
    if let Some(root) = parsed.output_root {
        config.output_root = root;
    }
    Ok(config)
}

/// Resolve a `--valgrind` value: an absolute path passes through; anything
/// else must be a bare command name resolved by scanning `PATH` directly.
fn resolve_valgrind(value: &str) -> Result<PathBuf, ConfigError> {
    if Path::new(value).is_absolute() {
        return Ok(PathBuf::from(value));
    }
    if value.contains('/') {
        return Err(ConfigError::ValgrindNotAbsolute {
            value: value.to_owned(),
        });
    }
    resolve_command(value).ok_or_else(|| ConfigError::ValgrindNotFound {
        name: value.to_owned(),
    })
}

/// Find an executable `name` on `PATH` without spawning a shell.
fn resolve_command(name: &str) -> Option<PathBuf> {
    let segments = env::var_os("PATH")?;
    env::split_paths(&segments)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(candidate)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Print the deterministic dry-run matrix.
///
/// A dry run produces no measurements and no verdict, so the output carries
/// no `verdict=` field.
fn print_dry_run(plan: &DryRunPlan) {
    let filters = if plan.filters.is_empty() {
        "all".to_owned()
    } else {
        plan.filters.join(",")
    };
    println!(
        "perf: dry run profile={} filters={filters} cells={}",
        profile_name(plan.profile),
        plan.cells.len(),
    );
    for cell in &plan.cells {
        println!(
            "  {} {} fixture={} processes={} warmup={} samples={} timeout={:?}",
            cell.id,
            engine_name(cell.family),
            fixture_name(cell.fixture),
            cell.processes,
            cell.warmup,
            cell.samples,
            cell.timeout,
        );
    }
    println!("no measurements collected");
}

/// Print the completed run line and verdict details; return the exit code.
///
/// Both completed variants are matched exhaustively. Pass and warn exit
/// successfully (warnings land on stderr); a calibration or comparison fail
/// exits unsuccessfully with every rejection on stderr.
fn report_completed(mode: &str, runner: &Runner, summary: &RunSummary) -> ExitCode {
    let run_dir = runner
        .run_dir()
        .map(|directory| directory.display().to_string())
        .unwrap_or_default();
    let (run_id, verdict_name, workload_count) = match summary {
        RunSummary::NoiseCalibration(noise) => (
            &noise.run_id,
            noise_verdict_name(&noise.verdict),
            noise.workloads.len(),
        ),
        RunSummary::Comparison(comparison) => (
            &comparison.run_id,
            verdict_name(&comparison.verdict),
            comparison.workloads.len(),
        ),
    };
    println!(
        "perf: run {run_id} mode={mode} run_dir={run_dir} verdict={verdict_name} workloads={workload_count}",
    );
    match summary {
        RunSummary::NoiseCalibration(noise) => match &noise.verdict {
            NoiseVerdict::Pass => ExitCode::SUCCESS,
            NoiseVerdict::Fail(rejections) => {
                for rejection in rejections {
                    eprintln!("perf: FAIL {rejection:?}");
                }
                ExitCode::FAILURE
            }
        },
        RunSummary::Comparison(comparison) => match &comparison.verdict {
            Verdict::Pass => ExitCode::SUCCESS,
            Verdict::Warn(warnings) => {
                for warning in warnings {
                    eprintln!("perf: WARN {warning}");
                }
                ExitCode::SUCCESS
            }
            Verdict::Fail(rejections) => {
                for rejection in rejections {
                    eprintln!("perf: FAIL {rejection:?}");
                }
                ExitCode::FAILURE
            }
        },
    }
}

const fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::Full => "full",
        Profile::Quick => "quick",
    }
}

const fn engine_name(family: EngineFamily) -> &'static str {
    match family {
        EngineFamily::Neovim => "neovim",
        EngineFamily::Oxvim => "oxvim",
    }
}

fn fixture_name(fixture: FixtureRequirement) -> String {
    match fixture {
        FixtureRequirement::None => "none".to_owned(),
        FixtureRequirement::LargeBuffer => "large-buffer".to_owned(),
        FixtureRequirement::PluginTree { count } => format!("plugin-tree:{count}"),
    }
}

const fn verdict_name(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "pass",
        Verdict::Warn(_) => "warn",
        Verdict::Fail(_) => "fail",
    }
}

const fn noise_verdict_name(verdict: &NoiseVerdict) -> &'static str {
    match verdict {
        NoiseVerdict::Pass => "pass",
        NoiseVerdict::Fail(_) => "fail",
    }
}

fn print_help() {
    println!(
        r"perf — Oxvim performance baseline harness

Usage: perf [OPTIONS]

Modes:
  (default)                      Cross-engine comparison (requires --noise)
  --dry-run                      Print the expanded matrix, spawn nothing
  --noise-only                   Run the oracle-vs-oracle noise calibration

Options:
  --profile <full|quick>         Run profile (default: full)
  --workload <name,...>          Workload type-name filter; repeatable,
                                 empty/unknown/duplicate names rejected
  --noise <noise.json>           Noise-calibration JSON backing a comparison
  --valgrind <path|name>         Absolute valgrind path or PATH command name
                                 (default: resolve `valgrind` from PATH)
  --output <dir>                 Output root (default: target/perf)
  --startuptime                  Run the --startuptime side pass
  --help                         Print this help"
    );
}

// ---------------------------------------------------------------------------
// Typed CLI failures
// ---------------------------------------------------------------------------

/// A CLI-argument failure, distinct from runner failures.
#[derive(Debug)]
enum ConfigError {
    UnknownOption(String),
    UnexpectedPositional(String),
    MissingValue {
        option: &'static str,
    },
    UnknownProfile {
        value: String,
    },
    Conflict {
        option: &'static str,
        with: &'static str,
    },
    NoiseRequired,
    ValgrindNotAbsolute {
        value: String,
    },
    ValgrindNotFound {
        name: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(option) => write!(formatter, "unknown option: {option}"),
            Self::UnexpectedPositional(argument) => {
                write!(formatter, "unexpected positional argument: {argument}")
            }
            Self::MissingValue { option } => write!(formatter, "{option} requires an argument"),
            Self::UnknownProfile { value } => {
                write!(formatter, "unknown profile '{value}': expected full|quick")
            }
            Self::Conflict { option, with } => {
                write!(formatter, "{option} cannot be combined with {with}")
            }
            Self::NoiseRequired => write!(
                formatter,
                "comparison requires --noise <noise.json>: the artifact \
                 written by a prior --noise-only run"
            ),
            Self::ValgrindNotAbsolute { value } => write!(
                formatter,
                "--valgrind '{value}' must be an absolute path or a bare command name"
            ),
            Self::ValgrindNotFound { name } => write!(
                formatter,
                "cannot resolve '{name}' from PATH: comparison demands \
                 exact DHAT allocation coverage"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Either a CLI configuration failure or a typed runner failure.
#[derive(Debug)]
enum Failure {
    Config(ConfigError),
    Run(RunError),
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "{error}"),
            Self::Run(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Run(error) => Some(error),
        }
    }
}
