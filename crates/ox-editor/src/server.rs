//! Editor-side listening-server registry behind `serverstart()`,
//! `serverstop()` and `serverlist()` (upstream `msgpack_rpc/server.c`).
//!
//! The builtins run wherever expression evaluation runs and never see the
//! event loop, so the actual sockets live behind [`ServerHost`]: the process
//! host (`oxvim`) installs an implementation at startup, mirroring how the
//! job manager reaches `jobstart` through [`crate::job::JobManager`].
//! Everything address-shaped — generation, name expansion, the live address
//! book — is defined here so it stays unit-testable without a loop.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::script::{StdPath, stdpath};

/// Process-level listen machinery the server builtins resolve through.
///
/// Error strings are the `%s` suffix of upstream's
/// `Failed to start server: %s` (`eval/funcs.c:6243`): "Unknown system error"
/// for validation/duplicate requests (`server_start` results 1 and 2,
/// `server.c:170,191`) and the `uv_strerror` text for bind failures.
pub trait ServerHost {
    /// Binds `address` (already expanded) and returns the effective bound
    /// address — a random TCP port is resolved here, the way
    /// `socket_watcher_start` rewrites `watcher->addr`
    /// (`event/socket.c:158-170`).
    ///
    /// # Errors
    ///
    /// Returns the `Failed to start server: %s` suffix when the address is
    /// empty, already listened on, or cannot be bound.
    fn start(&mut self, address: &str) -> Result<String, String>;

    /// Stops the listener bound to `address` (exact string match, upstream
    /// `strcmp` after `TO_SLASH`, which is a no-op on unix —
    /// `server.c:224-234`). Returns whether a listener was found.
    fn stop(&mut self, address: &str) -> bool;

    /// Every live listener address in start order (`server_address_list`,
    /// `server.c:260-272`).
    fn list(&self) -> Vec<String>;
}

/// Generates a unique address for a local server (`server_address_new`,
/// `server.c:116-136`): `<run dir>/<name>.<pid>.<counter>`, a named pipe on
/// Windows. The run directory is the raw `$XDG_RUNTIME_DIR` (`StdPath::Run`),
/// which upstream passes through `stdpaths_get_xdg_var` unvalidated
/// (`server.c:126`, `os/stdpaths.c:182-186`); a missing directory surfaces as
/// a bind failure, and the gated specs assert exactly that
/// (`options/defaults_spec.lua:459`).
#[must_use]
pub fn server_address_new(name: Option<&str>) -> String {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed);
    // `get_appname` (`server.c:122,127`); this port always runs as nvim.
    let base = name.unwrap_or("nvim");
    if cfg!(windows) {
        format!("//./pipe/{base}.{}.{}", std::process::id(), count)
    } else {
        let dir = stdpath(StdPath::Run)
            .first()
            .map_or_else(|| "/tmp".to_owned(), Clone::clone);
        format!("{dir}/{base}.{}.{}", std::process::id(), count)
    }
}

/// Expands a user-supplied listen value to the address bound, applying
/// `server_start`'s `isname` rule (`server.c:173`): a value containing no
/// `:`, `/` or `\` is a NAME, appended to a generated per-process address so
/// same-named listeners in one process tree never collide (#8519). Anything
/// else binds verbatim.
#[must_use]
pub fn prepare_server_address(address: &str) -> String {
    if address.contains([':', '/', '\\']) {
        address.to_owned()
    } else {
        server_address_new(Some(address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_address_lands_in_the_run_directory() {
        let dir = stdpath(StdPath::Run).remove(0);
        let address = server_address_new(None);
        assert!(address.starts_with(&format!("{dir}/nvim.")), "{address}");
    }

    #[test]
    fn generated_addresses_are_unique_within_the_process() {
        let first = server_address_new(None);
        let second = server_address_new(None);
        assert_ne!(first, second);
    }

    #[test]
    fn generated_address_carries_the_requested_name() {
        let address = server_address_new(Some("xtest1.2.3.4"));
        let tail = address
            .rsplit('/')
            .next()
            .expect("address always has a separator");
        assert!(tail.starts_with("xtest1.2.3.4."), "{tail}");
    }

    #[test]
    fn bare_names_expand_and_paths_bind_verbatim() {
        let expanded = prepare_server_address("xtest1.2.3.4");
        let tail = expanded.rsplit('/').next().unwrap_or_default();
        assert!(tail.starts_with("xtest1.2.3.4."), "{expanded}");
        assert!(
            tail.contains(&format!(".{}.", std::process::id())),
            "{expanded}"
        );
        assert_eq!(
            prepare_server_address("./Xtest-functional-server-socket"),
            "./Xtest-functional-server-socket"
        );
        assert_eq!(prepare_server_address("127.0.0.1:0"), "127.0.0.1:0");
        assert_eq!(prepare_server_address("C:/temp"), "C:/temp");
    }
}
