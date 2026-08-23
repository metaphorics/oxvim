#![forbid(unsafe_code)]
//! #[api]-annotated nvim_* API implementations.

extern crate self as ox_api;

mod autocmd;
mod api_function_names;
mod buffer;
mod channel;
mod context;
mod convert;
mod deprecated;
mod extmark;
mod keymap;
mod global;
mod metadata;
mod option_merge;
mod registry;
mod runtime;
mod tabpage;
mod ui;
mod window;

#[cfg(test)]
mod tests;

pub use convert::{FromObject, IntoObject, LuaRef, Nil};
pub use global::{CommandExecutor, execute_command, execute_nvim_cmd};
pub use metadata::{ApiType, FunctionMetadata, TypeRef};
pub use ox_api_macros::api;
pub use ox_excmd::ExCommand;
pub use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};
pub use registry::{core, DispatchFn, Registry, RegistryError};
pub use runtime::{
    AutocmdExecutor, ChannelSink, FileIO, LuaExecutor, MatchKind, StdFileIO, runtime_get_named,
    set_autocmd_executor, set_channel_sink, set_command_executor, set_file_io, set_lua_executor,
};