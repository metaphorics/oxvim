// unsafe-permitted crate: FFI surface; safe API exposed to dependents.
//! mlua-hosted LuaJIT executor, converters, and the C-side `vim` Lua table core.

pub mod converter;
mod embedded;
pub mod host;
mod stdlib;
mod treesitter;
pub mod typval_bridge;
mod uv_core;
mod uv_handles;
pub mod vim;

pub use converter::{
    free_lua_ref, lua_to_object, object_to_lua, ConversionError, CONVERSION_RECURSION_LIMIT,
};
pub use host::{ExecError, HostError, LuaHost, RuntimeRoot};
pub use typval_bridge::{lua_to_typval, typval_to_lua};
pub use vim::{
    bind_api, bind_variables, call_with_traceback, install_vim_core, ApiDispatchContext, BuiltinHost,
    FastCallbackGuard, FastCallbackState, Scheduler, TextlockGuard, VariableHost, VariableScope, Work,
};
