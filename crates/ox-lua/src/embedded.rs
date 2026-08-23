//! Build-time embedded Neovim Lua builtin modules.

use mlua::{Lua, MultiValue, Table, Value};

include!(concat!(env!("OUT_DIR"), "/ox_lua_embedded.rs"));

/// Install Neovim's built-in Lua preloaders. Runtime-file discovery itself
/// lives on the editor's 'runtimepath', so `vim.api.nvim__get_runtime` is bound
/// by [`crate::bind_api`] rather than against this single root.
pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
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

    Ok(())
}
