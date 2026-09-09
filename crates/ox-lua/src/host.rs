//! `LuaJIT` state creation and runtime-root configuration.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Table, Value};
use ox_types::Object;
use thiserror::Error;

use crate::converter::{ConversionError, lua_to_object, object_to_lua};
use crate::uv_core::EventLoopPump;
use crate::vim::{
    BuiltinHost, FastCallbackState, Scheduler, call_with_traceback, install_vim_core,
};
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
        if entry.exists() {
            vec![entry]
        } else {
            Vec::new()
        }
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
        Self::Runtime(crate::vim::mlua_error_text(&err))
    }
}

/// An initialized `LuaJIT` state and its editor integration context.
pub struct LuaHost {
    lua: Lua,
    runtime_root: RuntimeRoot,
    fast_callbacks: FastCallbackState,
    event_loop: EventLoopPump,
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
        embedded::install(&lua)?;
        treesitter::install(&lua, scheduler.clone())?;
        let event_loop = uv_core::install(
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
        Ok(Self {
            lua,
            runtime_root,
            fast_callbacks,
            event_loop,
        })
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

    /// Clone the non-blocking UV event-loop pump.
    #[must_use]
    pub fn event_loop_pump(&self) -> EventLoopPump {
        self.event_loop.clone()
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
                    Some(Value::String(s)) => String::from_utf8_lossy(&s.as_bytes()).into_owned(),
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
    // Strip every relative `package.path`/`package.cpath` entry before
    // anything else, including the unresolved-root exit below: the LuaJIT
    // defaults start with `./?.lua` and `./?.so` (vendored luaconf.h
    // LUA_PATH_DEFAULT / LUA_CPATH_DEFAULT), so a module planted in the
    // launch directory would load with full editor privileges. The default
    // absolute system entries survive: `_init_packages.lua:3-13` harvests
    // their `/?.so`-style suffix trails for `vim._so_trails` and
    // `_load_package` resolves native modules over 'runtimepath' (:25-31).
    //
    // Upstream builds `package.path` from 'runtimepath' via
    // `runtime/lua/vim/_init_packages.lua` and never includes a `.` entry;
    // a runtime-less host therefore resolves nothing rather than falling
    // back to the launch directory.
    for field in ["path", "cpath"] {
        let existing: String = package.get(field)?;
        let trusted = existing
            .split(';')
            .filter(|entry| Path::new(entry).is_absolute())
            .collect::<Vec<_>>()
            .join(";");
        package.set(field, trusted)?;
    }
    if runtime_root.resolve("").as_os_str().is_empty() {
        return Ok(());
    }
    let existing: String = package.get("path")?;
    let lua_root = runtime_root.resolve("lua");
    let module = lua_root.join("?.lua");
    let package_init = lua_root.join("?/init.lua");
    let mut entries = vec![
        module.to_string_lossy().into_owned(),
        package_init.to_string_lossy().into_owned(),
    ];
    entries.extend(
        existing
            .split(';')
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned),
    );
    package.set("path", entries.join(";"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn configures_package_paths_from_the_runtime_root() {
        let package_field = |lua: &Lua, field: &str| {
            lua.globals()
                .get::<Table>("package")
                .unwrap()
                .get::<String>(field)
                .unwrap()
        };
        let path = |lua: &Lua| package_field(lua, "path");
        let cpath = |lua: &Lua| package_field(lua, "cpath");
        let has_relative = |field: &str| -> bool {
            !field.is_empty()
                && field
                    .split(';')
                    .any(|entry| !Path::new(entry).is_absolute())
        };
        let contains_entry = |field: &str, entry: &str| {
            field.split(';').any(|candidate| candidate == entry)
        };
        let defaults = Lua::new();
        let default_path = path(&defaults);
        let default_cpath = cpath(&defaults);

        // A runtime-less host keeps only absolute system entries: the LuaJIT
        // defaults start with `./?.lua` / `./?.so`, so leaving them intact
        // would let a module planted in the launch directory load with
        // editor privileges. Absolute entries survive so `vim._so_trails`
        // keeps harvestable suffixes (_init_packages.lua:3-13).
        let lua = Lua::new();
        configure_package_path(&lua, &RuntimeRoot::new(PathBuf::new())).unwrap();
        let unresolved_path = path(&lua);
        let unresolved_cpath = cpath(&lua);
        assert!(
            !has_relative(&unresolved_path),
            "unresolved root left relative path entries: {unresolved_path}"
        );
        assert!(
            !has_relative(&unresolved_cpath),
            "unresolved root left relative cpath entries: {unresolved_cpath}"
        );
        for entry in default_path.split(';').filter(|e| Path::new(e).is_absolute()) {
            assert!(
                contains_entry(&unresolved_path, entry),
                "absolute path entry {entry} was dropped: {unresolved_path}"
            );
        }
        for entry in default_cpath.split(';').filter(|e| Path::new(e).is_absolute()) {
            assert!(
                contains_entry(&unresolved_cpath, entry),
                "absolute cpath entry {entry} was dropped: {unresolved_cpath}"
            );
        }

        // A resolved host prepends its runtime entries, which win by
        // precedence over the surviving defaults, and strips relative
        // entries from both fields while keeping the absolute system ones.
        let fresh = Lua::new();
        configure_package_path(&fresh, &RuntimeRoot::new(PathBuf::from("/rt"))).unwrap();
        let seeded = path(&fresh);
        assert!(seeded.starts_with("/rt/lua/?.lua;/rt/lua/?/init.lua;"), "{seeded}");
        assert!(
            !has_relative(&seeded),
            "resolved root left relative path entries: {seeded}"
        );
        for entry in default_path.split(';').filter(|e| Path::new(e).is_absolute()) {
            assert!(
                contains_entry(&seeded, entry),
                "absolute path entry {entry} was dropped: {seeded}"
            );
        }
        let seeded_cpath = cpath(&fresh);
        assert!(
            !has_relative(&seeded_cpath),
            "resolved root left relative cpath entries: {seeded_cpath}"
        );
        for entry in default_cpath.split(';').filter(|e| Path::new(e).is_absolute()) {
            assert!(
                contains_entry(&seeded_cpath, entry),
                "absolute cpath entry {entry} was dropped: {seeded_cpath}"
            );
        }

        // Empty Lua path elements are default-path substitutions. They must
        // not reappear when the filtered remainder is empty.
        let edge = Lua::new();
        let edge_package: Table = edge.globals().get("package").unwrap();
        edge_package.set("path", ";;").unwrap();
        edge_package.set("cpath", ";;").unwrap();
        configure_package_path(&edge, &RuntimeRoot::new(PathBuf::from("/rt"))).unwrap();
        assert_eq!(path(&edge), "/rt/lua/?.lua;/rt/lua/?/init.lua");
        assert_eq!(cpath(&edge), "");

        // The runtime entries remain usable by the ordinary Lua searcher.
        let runtime = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        let real_runtime = Lua::new();
        configure_package_path(&real_runtime, &RuntimeRoot::new(runtime)).unwrap();
        let _: Table = real_runtime
            .load("return require('vim.version')")
            .eval()
            .unwrap();
    }

    #[test]
    fn unresolved_runtime_root_rejects_modules_planted_in_the_launch_directory() {
        let planted = std::env::temp_dir().join(format!("ox-lua-p3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&planted);
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::write(planted.join("evil.lua"), "return 'hijacked'\n").unwrap();
        std::fs::create_dir_all(planted.join("evil_native")).unwrap();
        std::fs::write(planted.join("evil_native/init.lua"), "return 'hijacked'\n").unwrap();
        let previous_dir = std::env::current_dir().unwrap();
        // The CWD switch must stay inside the guard: a panic or early return
        // would strand later tests in the planted directory.
        std::env::set_current_dir(&planted).unwrap();
        let outcome = std::panic::catch_unwind(|| {
            let lua = Lua::new();
            configure_package_path(&lua, &RuntimeRoot::new(PathBuf::new())).unwrap();
            let source: Result<String, mlua::Error> = lua
                .load("local ok, value = pcall(require, 'evil') return ok and value or ''")
                .eval();
            let native: Result<String, mlua::Error> = lua
                .load(
                    "local ok, value = pcall(require, 'evil_native') \
                     return ok and value or ''",
                )
                .eval();
            (source, native)
        });
        std::env::set_current_dir(previous_dir).unwrap();
        let (loaded, native) = outcome.unwrap();
        assert_eq!(
            loaded.unwrap(),
            "",
            "planted launch-directory module was loaded"
        );
        assert_eq!(
            native.unwrap(),
            "",
            "planted launch-directory package was loaded"
        );
        std::fs::remove_dir_all(&planted).unwrap();
    }
}
