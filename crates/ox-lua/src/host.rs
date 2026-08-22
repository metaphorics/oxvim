//! `LuaJIT` state creation and runtime-root configuration.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value};
use ox_types::Object;
use thiserror::Error;

use crate::converter::{lua_to_object, object_to_lua, ConversionError};
use crate::vim::{call_with_traceback, install_vim_core, BuiltinHost, FastCallbackState, Scheduler};
use crate::{embedded, stdlib, treesitter, uv_core};

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

/// Failure executing Lua code in a [`LuaHost`].
#[derive(Debug, Error)]
pub enum ExecError {
    /// A Lua runtime or compile error, including traceback.
    #[error("Lua: {0}")]
    Runtime(String),
    /// A Lua file chunk load or runtime error, including traceback.
    #[error("Lua chunk: {0}")]
    Load(String),
    /// Failure converting between Lua and the API object model.
    #[error(transparent)]
    Conversion(#[from] ConversionError),
}

impl From<mlua::Error> for ExecError {
    fn from(err: mlua::Error) -> Self {
        Self::Runtime(err.to_string())
    }
}

/// An initialized `LuaJIT` state and its editor integration context.
pub struct LuaHost {
    lua: Lua,
    runtime_root: RuntimeRoot,
    fast_callbacks: FastCallbackState,
}

impl LuaHost {
    /// Create a `LuaJIT` state with Neovim's opened libraries and C-side `vim` core.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Lua`] if state construction, library opening, or
    /// runtime-path configuration fails.
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
        let fast_callbacks = install_vim_core(&lua, builtins.clone(), scheduler.clone())?;
        stdlib::install(&lua)?;
        embedded::install(&lua, runtime_root.clone())?;
        treesitter::install(&lua, scheduler.clone())?;
        uv_core::install(
            &lua,
            scheduler,
            fast_callbacks.clone(),
            runtime_root.clone(),
            builtins,
        )?;

        // executor.c:nlua_init_packages tail: with the builtin preloaders in
        // place, require the runtime prelude. vim._init_packages merges
        // vim._core.shared (vim.startswith, vim.split, ...) into the global
        // vim table, then runs the vim._core.editor assembly on the main
        // state, matching upstream's load order.
        let require: Function = lua.globals().get("require")?;
        require.call::<()>("vim._init_packages")?;
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

    /// Execute a Lua chunk with `...` bound to `args`, returning the first
    /// result converted to an API [`Object`].
    ///
    /// Mirrors `nlua_exec`: the chunk is compiled under the name `<nvim>`,
    /// arguments are pushed as Lua values, the chunk is called through
    /// `debug.traceback`, and the first result is converted back.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Runtime`] for Lua load or runtime failures,
    /// [`ExecError::Conversion`] for Object conversion failures, or the
    /// [`mlua::Error`] conversion thereof.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub fn exec(&mut self, code: &str, args: Vec<Object>) -> Result<Object, ExecError> {
        let function = self.lua.load(code).set_name("<nvim>").into_function()?;
        let lua_args = args
            .into_iter()
            .map(|arg| object_to_lua(&self.lua, &arg))
            .collect::<Result<Vec<_>, _>>()?
            .into();
        let mut results = call_with_traceback(&self.lua, &function, lua_args)?;
        match results.pop_front() {
            Some(value) => Ok(lua_to_object(&self.lua, &value)?),
            None => Ok(Object::Nil),
        }
    }

    /// Execute a Lua file through the `loadfile` global.
    ///
    /// Mirrors `nlua_exec_file`: `loadfile` is called (so it may be overridden),
    /// and the returned chunk is executed with no arguments. Errors carry the
    /// Lua message and traceback.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Load`] when `loadfile` reports a compile error or
    /// the loaded chunk raises at runtime, or other [`ExecError`] variants for
    /// conversion and internal Lua errors.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub fn exec_file(&mut self, path: &Path) -> Result<(), ExecError> {
        let loadfile: Function = self.lua.globals().get("loadfile")?;
        let path_arg = MultiValue::from_vec(vec![Value::String(
            self.lua.create_string(path.to_string_lossy().as_bytes())?,
        )]);
        let mut results = call_with_traceback(&self.lua, &loadfile, path_arg)
            .map_err(|e| ExecError::Load(e.to_string()))?;
        let chunk_value = results.pop_front();
        let error_value = results.pop_front();
        match chunk_value {
            Some(Value::Function(chunk)) => {
                call_with_traceback(&self.lua, &chunk, MultiValue::new())
                    .map_err(|e| ExecError::Load(e.to_string()))?;
                Ok(())
            }
            Some(Value::Nil) => {
                let message = match error_value {
                    Some(Value::String(s)) => {
                        String::from_utf8_lossy(&s.as_bytes()).into_owned()
                    }
                    Some(other) => format!("{other:?}"),
                    None => "loadfile returned nil without an error message".to_string(),
                };
                Err(ExecError::Load(message))
            }
            Some(other) => Err(ExecError::Load(format!("loadfile returned {other:?}"))),
            None => Err(ExecError::Load("loadfile returned no values".to_string())),
        }
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
