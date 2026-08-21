//! Build-time embedded Neovim Lua builtin modules.

use mlua::{Lua, MultiValue, Table, Value};

use crate::RuntimeRoot;

include!(concat!(env!("OUT_DIR"), "/ox_lua_embedded.rs"));

/// Install Neovim's built-in Lua preloaders and the single-runtime-root
/// implementation of `nvim__get_runtime` used by Lua package discovery.
pub(crate) fn install(lua: &Lua, runtime_root: RuntimeRoot) -> mlua::Result<()> {
    let package: Table = lua.globals().get("package")?;
    let preload: Table = package.get("preload")?;

    // Mirrors executor.c:nlua_module_preloader and nlua_init_packages: each
    // package.preload entry compiles its captured bytes and calls the chunk with
    // no arguments, returning exactly one Lua value to require().
    for module in EMBEDDED_MODULES {
        preload.set(
            module.name,
            lua.create_function(move |lua, _: MultiValue| {
                lua.load(module.bytes)
                    .set_name(module.source_name)
                    .call::<Value>(())
            })?,
        )?;
    }

    let vim: Table = lua.globals().get("vim")?;
    let api: Table = vim.get("api")?;
    api.set(
        "nvim__get_runtime",
        lua.create_function(move |_, (patterns, all, opts): (Vec<String>, bool, Table)| {
            // executor.c:nlua_thr_api_nvim__get_runtime requires this boolean.
            // RuntimeRoot already represents the single Lua-aware runtime root,
            // so the value validates the API contract without changing the root.
            let _: bool = opts.get("is_lua")?;
            let mut matches = Vec::new();
            for pattern in patterns {
                for entry in runtime_root.runtime_entries(pattern) {
                    matches.push(entry.to_string_lossy().into_owned());
                    if !all {
                        return Ok(matches);
                    }
                }
            }
            Ok(matches)
        })?,
    )?;

    Ok(())
}
