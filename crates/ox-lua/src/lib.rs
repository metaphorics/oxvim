// unsafe-permitted crate: FFI surface; safe API exposed to dependents.
//! mlua-hosted LuaJIT executor, converters, and the C-side `vim` table core.

pub mod converter;
pub mod host;
pub mod typval_bridge;
pub mod vim;

pub use converter::{
    lua_to_object, object_to_lua, ConversionError, CONVERSION_RECURSION_LIMIT,
};
pub use host::{HostError, LuaHost, RuntimeRoot};
pub use typval_bridge::{lua_to_typval, typval_to_lua};
pub use vim::{
    bind_api, call_with_traceback, install_vim_core, ApiFunction, ApiRegistry, BuiltinHost,
    FastCallbackGuard, FastCallbackState, Scheduler, Work,
};
