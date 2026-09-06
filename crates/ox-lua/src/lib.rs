// unsafe-permitted crate: FFI surface; safe API exposed to dependents.
//! mlua-hosted `LuaJIT` executor, converters, and the C-side `vim` Lua table core.

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
    CONVERSION_RECURSION_LIMIT, ConversionError, free_lua_ref, lua_to_object, lua_to_object_ref,
    object_to_lua,
};
pub use host::{ExecError, HostError, LuaHost, RuntimeRoot};
pub use typval_bridge::{collect_typval_refs, free_typval_refs, lua_to_typval, typval_to_lua};
pub use uv_core::EventLoopPump;
pub use vim::{
    ApiDispatchContext, BuiltinHost, FastCallbackGuard, FastCallbackState, Scheduler,
    TextlockGuard, VariableHost, VariableScope, Work, bind_api, bind_variables,
    call_with_traceback, error_shim, install_vim_core,
};
