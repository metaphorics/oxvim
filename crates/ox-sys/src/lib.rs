//! Audited boundary for system operations that require unsafe Rust.

use std::ffi::OsStr;

/// Sets a process environment variable.
///
/// # Safety contract
///
/// Callers must guarantee that no other thread concurrently reads or writes the
/// process environment. Oxvim calls this only on the main thread during
/// initialization or script execution; the `ox-uv` worker pool never reads the
/// process environment. Miri cannot validate this process-wide concurrency
/// contract.
pub fn set_env(name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
    // SAFETY: Callers uphold the process-wide exclusion contract documented above.
    unsafe { std::env::set_var(name, value) }
}

/// Removes a process environment variable.
///
/// # Safety contract
///
/// Callers must guarantee that no other thread concurrently reads or writes the
/// process environment. Oxvim calls this only on the main thread during
/// initialization or script execution; the `ox-uv` worker pool never reads the
/// process environment. Miri cannot validate this process-wide concurrency
/// contract.
pub fn unset_env(name: impl AsRef<OsStr>) {
    // SAFETY: Callers uphold the process-wide exclusion contract documented above.
    unsafe { std::env::remove_var(name) }
}
