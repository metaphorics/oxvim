//! Audited boundary for system operations that require unsafe Rust.
//!
//! Locale state lives in [`locale`]; environment mutation below.

pub mod locale;

pub use locale::{current_locale, set_locale, LocaleCategory};


use std::ffi::OsStr;

/// Whether `name` is a usable environment-variable name.
///
/// `std::env::set_var` and `std::env::remove_var` turn a rejected name into a
/// process abort: an empty name, a name holding `=` or NUL, and a value
/// holding NUL all reach `libc` as `EINVAL` and are unwrapped into a panic.
/// This crate is the only place that can see that shape before it becomes a
/// dead editor, so the refusal lives here rather than in each caller.
///
/// Upstream's matching boundary refuses too: `os_setenv` and `os_unsetenv`
/// (`os/env.c` 175-223) return -1 for an empty name and log a libuv
/// `EINVAL` for the rest without aborting. Callers with a user-visible
/// diagnostic still validate their own input — `ex_unletlock` and
/// `ex_let_env` report `E475` from `get_env_len` before ever reaching
/// `os_unsetenv` — so a refusal here is the backstop, not the message.
fn name_is_usable(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    !bytes.is_empty() && !bytes.contains(&b'=') && !bytes.contains(&0)
}

/// Sets a process environment variable, reporting whether it was applied.
///
/// `false` means the name or value was one the platform rejects, matching
/// `os_setenv`'s -1; the environment is left untouched.
///
/// # Safety contract
///
/// Callers must guarantee that no other thread concurrently reads or writes the
/// process environment. Oxvim calls this only on the main thread during
/// initialization or script execution; the `ox-uv` worker pool never reads the
/// process environment. Miri cannot validate this process-wide concurrency
/// contract.
// Not `#[must_use]`: the call itself performs the mutation, and callers that
// mirror upstream's ignored `os_setenv` status legitimately drop the report.
#[allow(clippy::must_use_candidate)]
pub fn set_env(name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> bool {
    let (name, value) = (name.as_ref(), value.as_ref());
    if !name_is_usable(name) || value.as_encoded_bytes().contains(&0) {
        return false;
    }
    // SAFETY: Callers uphold the process-wide exclusion contract documented
    // above, and the name and value were just checked against every shape
    // `std::env::set_var` panics on.
    unsafe { std::env::set_var(name, value) }
    true
}

/// Removes a process environment variable, reporting whether it was applied.
///
/// `false` means the name was one the platform rejects, matching
/// `os_unsetenv`'s -1; the environment is left untouched.
///
/// # Safety contract
///
/// Callers must guarantee that no other thread concurrently reads or writes the
/// process environment. Oxvim calls this only on the main thread during
/// initialization or script execution; the `ox-uv` worker pool never reads the
/// process environment. Miri cannot validate this process-wide concurrency
/// contract.
// Not `#[must_use]`: see [`set_env`].
#[allow(clippy::must_use_candidate)]
pub fn unset_env(name: impl AsRef<OsStr>) -> bool {
    let name = name.as_ref();
    if !name_is_usable(name) {
        return false;
    }
    // SAFETY: Callers uphold the process-wide exclusion contract documented
    // above, and the name was just checked against every shape
    // `std::env::remove_var` panics on.
    unsafe { std::env::remove_var(name) }
    true
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    // Interior NUL cannot be written as a Rust string literal; on Unix an
    // `OsStr` carries arbitrary bytes, which is exactly the input a caller can
    // hand us from Vimscript.
    fn with_nul(bytes: &[u8]) -> &OsStr {
        std::os::unix::ffi::OsStrExt::from_bytes(bytes)
    }

    // One case per clause of `name_is_usable` plus the value-NUL clause, each
    // failing only the clause it names: drop any single clause and exactly the
    // matching assertion flips, while the accepted-name case pins the
    // non-rejecting direction so a blanket `false` cannot pass either.
    #[test]
    fn env_mutation_refuses_only_the_names_the_platform_rejects() {
        assert!(!super::set_env("", "value"), "empty name accepted");
        assert!(!super::unset_env(""), "empty name accepted");

        assert!(!super::set_env("OX_SYS_EQ=BAD", "value"), "`=` in name accepted");
        assert!(!super::unset_env("OX_SYS_EQ=BAD"), "`=` in name accepted");

        assert!(!super::set_env(with_nul(b"OX_SYS\0NUL"), "value"), "NUL in name accepted");
        assert!(!super::unset_env(with_nul(b"OX_SYS\0NUL")), "NUL in name accepted");

        assert!(
            !super::set_env("OX_SYS_NUL_VALUE", with_nul(b"a\0b")),
            "NUL in value accepted"
        );
        assert_eq!(std::env::var_os("OX_SYS_NUL_VALUE"), None);

        // A name that satisfies every clause is still applied and removable.
        assert!(super::set_env("OX_SYS_GOOD", "value"));
        assert_eq!(std::env::var("OX_SYS_GOOD").as_deref(), Ok("value"));
        assert!(super::unset_env("OX_SYS_GOOD"));
        assert_eq!(std::env::var_os("OX_SYS_GOOD"), None);
    }
}
