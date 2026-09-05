//! `LPeg` 1.1.0 exposed as `vim.lpeg`.
//!
//! The upstream C module (vendored under `third_party/lpeg`, compiled by the
//! crate build script) is opened on the host Lua state and registered the same
//! way Neovim does in `src/nvim/lua/stdlib.c:821-832`: the module table
//! becomes both `vim.lpeg` and `package.loaded.lpeg`, so `require('lpeg')`
//! inside Neovim runtime code (for example `vim.re` and `vim.glob`) resolves
//! to the shared instance.

use std::os::raw::c_int;

use mlua::{Lua, Table, ffi};

unsafe extern "C" {
    /// `luaopen_lpeg` from the statically linked `LPeg` 1.1.0 module; returns the
    /// module table pushed onto the stack.
    fn luaopen_lpeg(state: *mut ffi::lua_State) -> c_int;
}

/// Registers `LPeg` 1.1.0 as `vim.lpeg` and `package.loaded.lpeg`.
pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    let module: Table = lua.exec_raw_lua(|raw| unsafe {
        if luaopen_lpeg(raw.state()) != 1 {
            return Err(mlua::Error::RuntimeError(
                "luaopen_lpeg did not return the module table".to_owned(),
            ));
        }
        raw.pop()
    })?;
    let vim: Table = lua.globals().get("vim")?;
    vim.set("lpeg", &module)?;
    let package: Table = lua.globals().get("package")?;
    let loaded: Table = package.get("loaded")?;
    loaded.set("lpeg", &module)
}
