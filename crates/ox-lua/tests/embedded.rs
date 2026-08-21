//! Behavioral contracts for build-time embedded Neovim core modules.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mlua::{Function, Lua, Table, Value};
pub use ox_lua::RuntimeRoot;

#[path = "../src/embedded.rs"]
mod embedded;

const UPSTREAM_BUILTIN_MODULES: &[&str] = &[
    "vim._init_packages",
    "vim.inspect",
    "vim.filetype",
    "vim.fs",
    "vim.F",
    "vim.keymap",
    "vim.loader",
    "vim.text",
    "vim.tty",
    "vim._core.cmdwin",
    "vim._core.defaults",
    "vim._core.editor",
    "vim._core.ex_cmd",
    "vim._core.exmode",
    "vim._core.exrc",
    "vim._core.help",
    "vim._core.log",
    "vim._core.marks",
    "vim._core.options",
    "vim._core.proc",
    "vim._core.server",
    "vim._core.shared",
    "vim._core.spell",
    "vim._core.stringbuffer",
    "vim._core.swapfile",
    "vim._core.system",
    "vim._core.table",
    "vim._core.tag",
    "vim._core.time",
    "vim._core.ui",
    "vim._core.ui2",
    "vim._core.util",
    "vim._core.vimfn",
];

struct TemporaryRuntime(PathBuf);

impl TemporaryRuntime {
    fn empty() -> Result<Self, Box<dyn Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "oxvim-embedded-runtime-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bare_lua() -> mlua::Result<Lua> {
    let lua = Lua::new();
    let vim = lua.create_table()?;
    vim.set("api", lua.create_table()?)?;
    lua.globals().set("vim", vim)?;
    Ok(lua)
}

#[test]
fn preloads_exact_upstream_builtin_set() -> Result<(), Box<dyn Error>> {
    let runtime = TemporaryRuntime::empty()?;
    let lua = bare_lua()?;
    embedded::install(&lua, RuntimeRoot::new(runtime.path()))?;

    let embedded_names = embedded::EMBEDDED_MODULES
        .iter()
        .map(|module| module.name)
        .collect::<Vec<_>>();
    assert_eq!(embedded_names, UPSTREAM_BUILTIN_MODULES);
    let package: Table = lua.globals().get("package")?;
    let preload: Table = package.get("preload")?;
    for name in UPSTREAM_BUILTIN_MODULES {
        assert!(matches!(preload.get::<Value>(*name)?, Value::Function(_)), "missing {name}");
    }
    Ok(())
}

#[test]
fn requires_shared_core_from_bytes_with_empty_runtime_root() -> Result<(), Box<dyn Error>> {
    let runtime = TemporaryRuntime::empty()?;
    let lua = bare_lua()?;
    embedded::install(&lua, RuntimeRoot::new(runtime.path()))?;

    let shared: Table = lua.load("return require('vim._core.shared')").eval()?;
    let _: Function = shared.get("deepcopy")?;
    assert!(runtime.path().read_dir()?.next().is_none());
    Ok(())
}

#[test]
fn runtime_listing_uses_the_supplied_root_and_all_flag() -> Result<(), Box<dyn Error>> {
    let runtime = TemporaryRuntime::empty()?;
    fs::create_dir(runtime.path().join("lua"))?;
    fs::write(runtime.path().join("lua/first.lua"), b"return true\n")?;
    fs::write(runtime.path().join("lua/second.lua"), b"return true\n")?;

    let lua = bare_lua()?;
    embedded::install(&lua, RuntimeRoot::new(runtime.path()))?;
    let found: Vec<String> = lua
        .load(
            "return vim.api.nvim__get_runtime(\
             {'lua/first.lua', 'lua/second.lua'}, false, {is_lua = true})",
        )
        .eval()?;

    assert_eq!(
        found,
        vec![runtime
            .path()
            .join("lua/first.lua")
            .to_string_lossy()
            .into_owned()]
    );
    Ok(())
}
