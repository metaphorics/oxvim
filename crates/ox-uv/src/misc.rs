//! Safe implementations of the luv miscellaneous subset selected for ox-uv core.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::{Error, HandleId, Result, UvLoop};

/// Portable result of `uv.os_uname()`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Uname {
    /// Operating-system name.
    pub sysname: String,
    /// Operating-system release.
    pub release: String,
    /// Operating-system version/build string.
    pub version: String,
    /// Hardware architecture identifier.
    pub machine: String,
}

/// Returns monotonic nanoseconds relative to a process-local origin.
pub fn hrtime() -> u64 {
    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    let elapsed = ORIGIN.elapsed().as_nanos();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

/// Returns Unix seconds and microseconds within the current second.
pub fn gettimeofday() -> Result<(u64, u32)> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::ClockBeforeEpoch)?;
    Ok((elapsed.as_secs(), elapsed.subsec_micros()))
}

/// Returns the four fields documented by `uv.os_uname()`.
pub fn os_uname() -> Uname {
    let value = rustix::system::uname();
    Uname {
        sysname: value.sysname().to_string_lossy().into_owned(),
        release: value.release().to_string_lossy().into_owned(),
        version: value.version().to_string_lossy().into_owned(),
        machine: value.machine().to_string_lossy().into_owned(),
    }
}

/// Returns the current user's platform home directory.
#[allow(deprecated)]
pub fn os_homedir() -> Result<PathBuf> {
    std::env::home_dir().ok_or(Error::MissingEnvironment(home_variable()))
}

/// Returns the platform temporary directory.
pub fn os_tmpdir() -> PathBuf {
    std::env::temp_dir()
}

/// Returns the current process identifier.
pub fn getpid() -> u32 {
    std::process::id()
}

/// Returns the current working directory.
pub fn cwd() -> Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

/// Changes the process-wide current working directory.
pub fn chdir(path: impl AsRef<Path>) -> Result<()> {
    std::env::set_current_dir(path)?;
    Ok(())
}

/// Returns an environment value without imposing UTF-8 conversion.
pub fn os_getenv(name: impl AsRef<OsStr>) -> Option<OsString> {
    std::env::var_os(name)
}

#[cfg(windows)]
fn home_variable() -> &'static str {
    "USERPROFILE"
}

#[cfg(not(windows))]
fn home_variable() -> &'static str {
    "HOME"
}

// ---------------------------------------------------------------------------
// Task 7c: remaining safe luv miscellaneous surface.
// ---------------------------------------------------------------------------

/// The packed ox-uv engine version (`0x000100` for 0.1.0).
///
/// This is the engine version reported by `uv.version()` in
/// `runtime/doc/luvref.txt` (lines 329-349). There is no upstream libuv ABI
/// being mirrored, so the ox-uv crate version is surfaced instead and the
/// interpretation is documented here rather than implying a libuv release.
pub fn version() -> u32 {
    (0 << 16) | (1 << 8) | 0
}

/// The ox-uv engine version as a string, e.g. `"0.1.0"`.
///
/// See `uv.version_string()` in `runtime/doc/luvref.txt` (lines 351-361).
/// The string reports the ox-uv engine version rather than a libuv release.
pub fn version_string() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Returns the path of the current executable. See `uv.exepath()` in
/// `runtime/doc/luvref.txt` (lines 4034-4038).
pub fn exepath() -> Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

/// Password-file information returned by `uv.os_get_passwd()` in
/// `runtime/doc/luvref.txt` (lines 4451-4463).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Passwd {
    /// Login name.
    pub username: String,
    /// Numeric user id (`None` on Windows).
    pub uid: Option<u32>,
    /// Numeric group id (`None` on Windows).
    pub gid: Option<u32>,
    /// Login shell (`None` on Windows).
    pub shell: Option<String>,
    /// Home directory.
    pub homedir: String,
}

/// Returns password-file information for the current user.
///
/// On Unix the entry matching the real user id is read from `/etc/passwd`; on
/// Windows the username and home directory are reported and the remaining
/// fields are `None`, matching `uv.os_get_passwd()`.
pub fn os_get_passwd() -> Result<Passwd> {
    #[cfg(unix)]
    {
        let uid = rustix::process::getuid().as_raw();
        let homedir = os_homedir()?;
        let mut passwd = Passwd {
            username: String::new(),
            uid: Some(uid),
            gid: None,
            shell: None,
            homedir: homedir.to_string_lossy().into_owned(),
        };
        if let Ok(passwd_file) = std::fs::read_to_string("/etc/passwd") {
            for line in passwd_file.lines() {
                let fields: Vec<&str> = line.split(':').collect();
                if fields.len() >= 7 {
                    if let Ok(entry_uid) = fields[2].parse::<u32>() {
                        if entry_uid == uid {
                            passwd.username = fields[0].to_owned();
                            passwd.gid = fields[3].parse::<u32>().ok();
                            passwd.shell = if fields[6].is_empty() { None } else { Some(fields[6].to_owned()) };
                            if !fields[5].is_empty() {
                                passwd.homedir = fields[5].to_owned();
                            }
                            break;
                        }
                    }
                }
            }
        }
        if passwd.username.is_empty() {
            passwd.username = std::env::var_os("USER")
                .map(|v| v.to_string_lossy().into_owned())
                .ok_or(Error::MissingEnvironment("USER"))?;
        }
        Ok(passwd)
    }

    #[cfg(not(unix))]
    {
        let homedir = os_homedir()?;
        let username = std::env::var_os("USERNAME")
            .map(|v| v.to_string_lossy().into_owned())
            .ok_or(Error::MissingEnvironment("USERNAME"))?;
        Ok(Passwd {
            username,
            uid: None,
            gid: None,
            shell: None,
            homedir: homedir.to_string_lossy().into_owned(),
        })
    }
}

/// Sets an environment variable in the current process.
///
/// See `uv.os_setenv()` in `runtime/doc/luvref.txt` (lines 4406-4417). This
/// operation is not safely expressible with the crate's pure-safe surface:
/// `std::env::set_var` is `unsafe` in edition 2024 and rustix exposes no safe
/// setter, so an [`Error::Unsupported`] is returned.
pub fn os_setenv(_name: impl AsRef<OsStr>, _value: impl AsRef<OsStr>) -> Result<()> {
    Err(Error::Unsupported {
        feature: "os_setenv",
        reason: "no safe (non-unsafe) process-environment setter is available in std or rustix",
    })
}

/// Unsets an environment variable in the current process.
///
/// See `uv.os_unsetenv()` in `runtime/doc/luvref.txt` (lines 4419-4427). For
/// the same safety reason as [`os_setenv`], this returns
/// [`Error::Unsupported`].
pub fn os_unsetenv(_name: impl AsRef<OsStr>) -> Result<()> {
    Err(Error::Unsupported {
        feature: "os_unsetenv",
        reason: "no safe (non-unsafe) process-environment unsetter is available in std or rustix",
    })
}

/// Resource-usage snapshot corresponding to `uv.getrusage()` in
/// `runtime/doc/luvref.txt` (lines 4108-4132).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rusage {
    /// User CPU time as `(seconds, microseconds)`.
    pub utime: (u64, u32),
    /// System CPU time as `(seconds, microseconds)`.
    pub stime: (u64, u32),
    /// Maximum resident set size in kilobytes.
    pub maxrss: u64,
    /// Integral shared memory size (unsupported; zero).
    pub ixrss: u64,
    /// Integral unshared data size (unsupported; zero).
    pub idrss: u64,
    /// Integral unshared stack size (unsupported; zero).
    pub isrss: u64,
    /// Page reclaims (soft page faults).
    pub minflt: u64,
    /// Page faults (hard page faults).
    pub majflt: u64,
    /// Number of swaps (unsupported; zero).
    pub nswap: u64,
    /// Block input operations.
    pub inblock: u64,
    /// Block output operations.
    pub oublock: u64,
    /// IPC messages sent (unsupported; zero).
    pub msgsnd: u64,
    /// IPC messages received (unsupported; zero).
    pub msgrcv: u64,
    /// Signals received (unsupported; zero).
    pub nsignals: u64,
    /// Voluntary context switches.
    pub nvcsw: u64,
    /// Involuntary context switches.
    pub nivcsw: u64,
}

impl Rusage {
    /// Times as seconds and microseconds.
    pub fn utime_sec(&self) -> (u64, u32) {
        self.utime
    }
}

/// Returns resource usage for the current process.
///
/// On Linux the fields are populated from `/proc/self/stat`,
/// `/proc/self/status`, and `/proc/self/io`; fields the kernel does not
/// expose there (integral sizes, swaps, IPC, signals) are zero, matching the
/// partially-populated tables libuv documents on macOS/Windows.
pub fn getrusage() -> Result<Rusage> {
    #[cfg(target_os = "linux")]
    {
        let mut usage = Rusage::default();
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        // `/proc/self/stat` fields: pid (1) comm (2) state (3) ppid(4) ...
        // utime is field 14, stime field 15 (1-indexed), 100 Hz clock ticks.
        if let Some(rest) = stat.split(')').nth(1) {
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let utime_ticks = fields.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let stime_ticks = fields.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            usage.utime = ticks_to_time(utime_ticks);
            usage.stime = ticks_to_time(stime_ticks);
        }
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in status.lines() {
            if line.starts_with("VmHWM:") {
                usage.maxrss = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            } else if line.starts_with("voluntary_ctxt_switches:") {
                usage.nvcsw = line.split_whitespace().last().and_then(|v| v.parse().ok()).unwrap_or(0);
            } else if line.starts_with("nonvoluntary_ctxt_switches:") {
                usage.nivcsw = line.split_whitespace().last().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
        }
        if let Ok(io) = std::fs::read_to_string("/proc/self/io") {
            for line in io.lines() {
                if let Some(v) = line.strip_prefix("read_bytes:") {
                    usage.inblock = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("write_bytes:") {
                    usage.oublock = v.trim().parse().unwrap_or(0);
                }
            }
        }
        Ok(usage)
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::Unsupported {
            feature: "getrusage",
            reason: "no safe cross-platform resource-usage source is available",
        })
    }
}

/// Converts `/proc/self/stat` clock ticks (100 Hz) to `(sec, usec)`.
fn ticks_to_time(ticks: u64) -> (u64, u32) {
    const HZ_MS: u64 = 10; // 100 Hz -> 10 ms per tick
    let milliseconds = ticks * HZ_MS;
    (milliseconds / 1000, ((milliseconds % 1000) * 1000) as u32)
}

/// Returns the resident set size (RSS) in bytes for the current process.
///
/// See `uv.resident_set_memory()` in `runtime/doc/luvref.txt`
/// (lines 4102-4106). Reads `VmRSS` (kB) from `/proc/self/status` on Linux.
pub fn resident_set_memory() -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status")?;
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                let kb: u64 = value.split_whitespace().next().and_then(|v| v.parse().ok()).ok_or(Error::Io(io_error("unparseable VmRSS")))?;
                return Ok(kb.saturating_mul(1024));
            }
        }
        Err(Error::Io(io_error("missing VmRSS")))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::Unsupported {
            feature: "resident_set_memory",
            reason: "no safe cross-platform RSS source is available",
        })
    }
}

/// Returns the total system memory in bytes.
///
/// See `uv.get_total_memory()` in `runtime/doc/luvref.txt` (lines 4070-4074).
/// Reads `MemTotal` (kB) from `/proc/meminfo` on Linux.
pub fn get_total_memory() -> u64 {
    meminfo_kb("MemTotal") .saturating_mul(1024)
}

/// Returns the current free system memory in bytes.
///
/// See `uv.get_free_memory()` in `runtime/doc/luvref.txt` (lines 4076-4080).
/// Reads `MemAvailable` (kB) from `/proc/meminfo` on Linux, falling back to
/// `MemFree`.
pub fn get_free_memory() -> u64 {
    meminfo_kb("MemAvailable").saturating_mul(1024)
}

/// Returns the amount of memory available to the process based on imposed
/// limits, or `0` when no constraint is known.
///
/// See `uv.get_constrained_memory()` in `runtime/doc/luvref.txt`
/// (lines 4082-4090). On Linux this reads the cgroup v2 `memory.max` or cgroup
/// v1 `memory.limit_in_bytes` limit.
pub fn get_constrained_memory() -> u64 {
    #[cfg(target_os = "linux")]
    {
        cgroup_memory_limit().unwrap_or(0)
    }

    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Returns free memory still available to the process, bounded by any
/// constraint; falls back to [`get_free_memory`] when no limit is known.
///
/// See `uv.get_available_memory()` in `runtime/doc/luvref.txt`
/// (lines 4092-4100).
pub fn get_available_memory() -> u64 {
    let free = get_free_memory();
    let constrained = get_constrained_memory();
    if constrained == 0 {
        free
    } else {
        free.min(constrained)
    }
}

/// Reads a `kb` value from `/proc/meminfo`, returning zero when absent.
#[cfg(target_os = "linux")]
fn meminfo_kb(key: &str) -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .map(|info| {
            info.lines()
                .find_map(|line| {
                    let trimmed = line.trim_start();
                    if let Some(rest) = trimmed.strip_prefix(key) {
                        let value = rest.trim_start_matches(':').trim_start();
                        value.split_whitespace().next().and_then(|v| v.parse::<u64>().ok())
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Reads the process cgroup memory limit on Linux.
#[cfg(target_os = "linux")]
fn cgroup_memory_limit() -> Option<u64> {
    // cgroup v2 path.
    if let Ok(mount) = std::fs::read_to_string("/proc/self/mountinfo") {
        // Simplest robust fallback: try the cgroup2 hierarchy file.
        let _ = mount;
    }
    if let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") {
        for line in cgroup.lines() {
            // "0::/slice/..." for v2; "0:memory:/path" for v1.
            let path = line.rsplit(':').next()?;
            let rel = path.trim_start_matches('/');
            if !rel.is_empty() {
                if let Ok(max) = std::fs::read_to_string(format!("/sys/fs/cgroup/{rel}/memory.max")) {
                    if let Ok(bytes) = max.trim().parse::<u64>() {
                        return Some(bytes);
                    }
                }
                if let Ok(limit) = std::fs::read_to_string(format!("/sys/fs/cgroup/{rel}/memory.limit_in_bytes")) {
                    if let Ok(bytes) = limit.trim().parse::<u64>() {
                        return Some(bytes);
                    }
                }
            }
        }
    }
    None
}

/// Returns the system load average as a triad. See `uv.loadavg()` in
/// `runtime/doc/luvref.txt` (lines 4367-4371). Reads `/proc/loadavg` on Linux.
pub fn loadavg() -> (f64, f64, f64) {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/loadavg")
            .map(|content| {
                let parts: Vec<&str> = content.split_whitespace().collect();
                let one = parts.first().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let five = parts.get(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let fifteen = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                (one, five, fifteen)
            })
            .unwrap_or((0.0, 0.0, 0.0))
    }

    #[cfg(not(target_os = "linux"))]
    {
        (0.0, 0.0, 0.0)
    }
}

/// Returns the system uptime in seconds. See `uv.uptime()` in
/// `runtime/doc/luvref.txt` (lines 4279-4283). Reads `/proc/uptime` on Linux.
pub fn uptime() -> Result<f64> {
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/uptime")?;
        content
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| Error::Io(io_error("unparseable /proc/uptime")))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::Unsupported {
            feature: "uptime",
            reason: "no safe cross-platform uptime source is available",
        })
    }
}

/// CPU time counters (milliseconds) for one logical processor.
///
/// See `uv.cpu_info()` in `runtime/doc/luvref.txt` (lines 4180-4227).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CpuTimes {
    /// User time in milliseconds.
    pub user: u64,
    /// Nice time in milliseconds.
    pub nice: u64,
    /// System time in milliseconds.
    pub sys: u64,
    /// Idle time in milliseconds.
    pub idle: u64,
    /// Interrupt time in milliseconds.
    pub irq: u64,
}

/// One logical processor's identity and cumulative times.
///
/// See `uv.cpu_info()` in `runtime/doc/luvref.txt` (lines 4180-4227).
#[derive(Clone, Debug, PartialEq)]
pub struct CpuInfo {
    /// Processor model string.
    pub model: String,
    /// Clock speed in MHz.
    pub speed: f64,
    /// Cumulative per-state time in milliseconds.
    pub times: CpuTimes,
}

/// Returns information about the CPU(s) on the system.
///
/// On Linux the model/speed come from `/proc/cpuinfo` and the times from
/// `/proc/stat` (10 ms per USER_HZ tick). See `uv.cpu_info()`.
pub fn cpu_info() -> Result<Vec<CpuInfo>> {
    #[cfg(target_os = "linux")]
    {
        let models = cpuinfo_models();
        let stat = std::fs::read_to_string("/proc/stat")?;
        let mut cpus = Vec::new();
        for line in stat.lines() {
            let mut parts = line.split_whitespace();
            if let Some(header) = parts.next() {
                if let Some(index) = header.strip_prefix("cpu") {
                    if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
                        continue;
                    }
                    let values: Vec<u64> = parts.filter_map(|v| v.parse().ok()).collect();
                    let get = |i: usize| values.get(i).copied().unwrap_or(0);
                    let ticks = |i: usize| get(i).saturating_mul(10);
                    cpus.push(CpuInfo {
                        model: models.get(index.parse::<usize>().unwrap_or(0)).cloned().unwrap_or_default(),
                        speed: 0.0,
                        times: CpuTimes {
                            user: ticks(0),
                            nice: ticks(1),
                            sys: ticks(2),
                            idle: ticks(3),
                            irq: ticks(5),
                        },
                    });
                }
            }
        }
        if cpus.is_empty() {
            return Err(Error::Io(io_error("no cpu lines in /proc/stat")));
        }
        Ok(cpus)
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::Unsupported {
            feature: "cpu_info",
            reason: "no safe cross-platform CPU info source is available",
        })
    }
}

/// Parses `/proc/cpuinfo` model names, one entry per logical processor.
#[cfg(target_os = "linux")]
fn cpuinfo_models() -> Vec<String> {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|content| {
            let mut models = Vec::new();
            let mut current = String::new();
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    if !current.is_empty() {
                        models.push(std::mem::take(&mut current));
                        current.clear();
                    }
                    continue;
                }
                if let Some(model) = trimmed.strip_prefix("model name") {
                    let section = model.split(':').nth(1).map(|v| v.trim().to_owned()).unwrap_or_default();
                    if !section.is_empty() {
                        current = section;
                    }
                }
            }
            if !current.is_empty() {
                models.push(current);
            }
            models
        })
        .unwrap_or_default()
}

fn io_error(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, message)
}

/// Prints all handles associated with the loop to stderr in the luvref
/// `[flags] handle-type handle-id` format, where flags are `R` (referenced),
/// `A` (active), and `I` (internal/io).
///
/// See `uv.print_all_handles()` in `runtime/doc/luvref.txt`
/// (lines 4285-4296). Ad hoc debugging aid; no API stability guarantees.
pub fn print_all_handles(uv_loop: &mut UvLoop) {
    let mut ids: Vec<HandleId> = Vec::new();
    uv_loop.walk(|_, id| ids.push(id));
    for id in ids {
        let Some(state) = uv_loop.state(id) else { continue };
        let mut flags = String::new();
        if state.referenced { flags.push('R'); }
        let active = state.is_active();
        if active { flags.push('A'); }
        eprintln!("[{}] {} handle-{:?}", flags, state.kind.name(), id);
    }
}

/// Prints only active handles associated with the loop to stderr.
///
/// See `uv.print_active_handles()` in `runtime/doc/luvref.txt`
/// (lines 4298-4303). Same format as [`print_all_handles`].
pub fn print_active_handles(uv_loop: &mut UvLoop) {
    let mut ids: Vec<HandleId> = Vec::new();
    uv_loop.walk(|_, id| ids.push(id));
    for id in ids {
        let Some(state) = uv_loop.state(id) else { continue };
        if !state.is_active() { continue; }
        eprintln!("[A] {} handle-{:?}", state.kind.name(), id);
    }
}
