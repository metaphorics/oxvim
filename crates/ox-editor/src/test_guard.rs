//! Process environment restoration for tests.

/// Restores named process environment variables when dropped.
pub(crate) struct EnvGuard {
    values: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    pub(crate) fn new(names: &[&'static str]) -> Self {
        Self {
            values: names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.values {
            if let Some(value) = value {
                let _ = ox_uv::misc::os_setenv(name, value);
            } else {
                let _ = ox_uv::misc::os_unsetenv(name);
            }
        }
    }
}
