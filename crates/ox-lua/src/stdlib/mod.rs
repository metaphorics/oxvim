//! Neovim-compatible pure and editor-facing Lua standard-library bindings.

mod base64;
mod diff;
mod json;
mod mpack;
mod regex;

use mlua::{Lua, Table};

/// Install this crate's standard-library fields on the existing global `vim` table.
pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    mpack::install(lua, &vim)?;
    json::install(lua, &vim)?;
    diff::install(lua, &vim)?;
    base64::install(lua, &vim)?;
    regex::install(lua, &vim)
}
