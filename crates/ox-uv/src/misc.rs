//! Safe implementations of the luv miscellaneous subset selected for ox-uv core.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::{Error, Result};

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
