//! LuaJIT state creation and runtime-root configuration.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, LuaOptions, StdLib, Table, Value};
use thiserror::Error;

use crate::vim::{install_vim_core, BuiltinHost, FastCallbackState, Scheduler};

/// Caller-provided root of the checked-out Neovim-compatible runtime tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRoot(PathBuf);

impl RuntimeRoot {
    /// Wrap a runtime directory path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Borrow the configured runtime root.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Resolve one runtime-relative entry without consulting process globals.
    #[must_use]
    pub fn resolve(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.0.join(relative)
    }

    /// Return an existing runtime-relative entry, mirroring the single-root
    /// portion of `nvim__get_runtime`.
    #[must_use]
    pub fn runtime_entries(&self, relative: impl AsRef<Path>) -> Vec<PathBuf> {
        let entry = self.resolve(relative);
        if entry.exists() { vec![entry] } else { Vec::new() }
    }
}

/// Failure constructing or configuring a Lua host.
#[derive(Debug, Error)]
pub enum HostError {
    /// Lua state setup failed.
    #[error(transparent)]
    Lua(#[from] mlua::Error),
}

/// An initialized LuaJIT state and its editor integration context.
pub struct LuaHost {
    lua: Lua,
    runtime_root: RuntimeRoot,
    fast_callbacks: FastCallbackState,
}

impl LuaHost {
    /// Create a LuaJIT state with Neovim's opened libraries and C-side `vim` core.
    pub fn new(
        runtime_root: RuntimeRoot,
        builtins: Rc<dyn BuiltinHost>,
        scheduler: Rc<dyn Scheduler>,
    ) -> Result<Self, HostError> {
        let libraries = StdLib::TABLE
            | StdLib::IO
            | StdLib::OS
            | StdLib::STRING
            | StdLib::MATH
            | StdLib::PACKAGE
            | StdLib::DEBUG
            | StdLib::BIT
            | StdLib::JIT
            | StdLib::FFI;
        // Contract: only the trusted libraries opened by upstream luaL_openlibs are enabled.
        let lua = unsafe { Lua::unsafe_new_with(libraries, LuaOptions::default()) };
        // LuaJIT's luaL_openlibs preloads ffi but does not create a global "ffi" table.
        // mlua's StdLib::FFI loads it via luaL_requiref with glb=1, so undo the global side
        // effect to keep upstream's package-only placement while leaving package.loaded.ffi.
        lua.globals().set("ffi", Value::Nil)?;
        configure_package_path(&lua, &runtime_root)?;
        let fast_callbacks = install_vim_core(&lua, builtins, scheduler)?;
        Ok(Self { lua, runtime_root, fast_callbacks })
    }

    /// Borrow the initialized Lua state.
    #[must_use]
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Borrow the runtime-root seam.
    #[must_use]
    pub fn runtime_root(&self) -> &RuntimeRoot {
        &self.runtime_root
    }

    /// Clone the fast-callback counter handle for event adapters.
    #[must_use]
    pub fn fast_callbacks(&self) -> FastCallbackState {
        self.fast_callbacks.clone()
    }
}

fn configure_package_path(lua: &Lua, runtime_root: &RuntimeRoot) -> mlua::Result<()> {
    let package: Table = lua.globals().get("package")?;
    let existing: String = package.get("path")?;
    let lua_root = runtime_root.resolve("lua");
    let module = lua_root.join("?.lua");
    let package_init = lua_root.join("?/init.lua");
    package.set(
        "path",
        format!(
            "{};{};{existing}",
            module.to_string_lossy(),
            package_init.to_string_lossy()
        ),
    )
}
