//! Locale inspection and mutation through the C library's `setlocale(3)`.
//!
//! Nvim's `:language` command (`os/lang.c` `ex_language`) delegates locale
//! validity, per-category state, and the current-locale queries behind
//! `v:lang`/`v:ctype` to the C library. This module is the audited unsafe
//! boundary for those calls, mirroring `os/lang.c` `get_locale_val`.

use std::ffi::{c_char, c_int, CStr, CString};

/// Locale category understood by [`current_locale`] and [`set_locale`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocaleCategory {
    /// `LC_ALL` — every category at once.
    All,
    /// `LC_MESSAGES` — message translation language.
    Messages,
    /// `LC_CTYPE` — character classification and encoding.
    CType,
    /// `LC_TIME` — date and time formatting.
    Time,
    /// `LC_COLLATE` — string collation order.
    Collate,
    /// `LC_NUMERIC` — number formatting.
    Numeric,
}

impl LocaleCategory {
    /// Raw `setlocale(3)` category constant.
    fn raw(self) -> c_int {
        match self {
            Self::All => libc::LC_ALL,
            Self::Messages => libc::LC_MESSAGES,
            Self::CType => libc::LC_CTYPE,
            Self::Time => libc::LC_TIME,
            Self::Collate => libc::LC_COLLATE,
            Self::Numeric => libc::LC_NUMERIC,
        }
    }
}

/// Returns the process's current locale for `category` — the
/// `setlocale(category, NULL)` query behind `get_locale_val`.
///
/// The C library always answers a query; `None` is kept for the
/// null-pointer shape callers translate to an empty string upstream.
#[must_use]
pub fn current_locale(category: LocaleCategory) -> Option<String> {
    // SAFETY: a NULL locale is a pure query that never mutates locale state.
    // The returned pointer into the C library's static buffer is copied by
    // `cstr_to_string` before this function returns.
    let value = unsafe { libc::setlocale(category.raw(), std::ptr::null()) };
    cstr_to_string(value)
}

/// Sets the locale for `category` to `name`, returning the effective locale
/// string, or `None` when the C library rejects `name` (`setlocale` NULL —
/// the signal upstream reports as E197).
///
/// # Safety contract
///
/// `setlocale` mutates process-wide locale state and returns a pointer into
/// a library-owned buffer. Callers must invoke this only on the main thread
/// during initialization or script execution — the same process-wide
/// exclusion contract as [`crate::set_env`] — and must not query or set
/// locales from other threads concurrently. The returned `String` is copied
/// before returning, so it stays valid after later `setlocale` calls.
// Not `#[must_use]`: the call itself performs the mutation; the `Option` is
// only the success report, and best-effort callers may legitimately drop it.
#[allow(clippy::must_use_candidate)]
pub fn set_locale(category: LocaleCategory, name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    // SAFETY: `name` is a valid NUL-terminated C string for the duration of
    // the call. Interior NUL bytes were rejected above, so no silent locale
    // truncation is possible. The returned static-buffer pointer is copied
    // by `cstr_to_string` before this function returns.
    let value = unsafe { libc::setlocale(category.raw(), name.as_ptr()) };
    cstr_to_string(value)
}

fn cstr_to_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: non-NULL `setlocale` results are NUL-terminated strings owned
    // by the C library that remain valid until the next `setlocale` call;
    // copying here closes that window for the caller.
    Some(unsafe { CStr::from_ptr(value) }.to_string_lossy().into_owned())
}
