#![forbid(unsafe_code)]
//! #[api]-annotated nvim_* API implementations.

extern crate self as ox_api;

mod api_function_names;
mod autocmd;
mod buffer;
mod channel;
mod command;
mod context;
mod convert;
mod deprecated;
mod extmark;
mod global;
mod keymap;
mod metadata;
mod mode;
mod option_merge;
mod registry;
mod runtime;
mod session;

pub use session::{ApiCallerGuard, ApiSession};
mod tabpage;
mod ui;
mod window;

#[cfg(test)]
mod tests;

pub use autocmd::execute_firing_plan;
pub use convert::{FromObject, IntoObject, LuaRef, Nil};
pub use deprecated::decode_atomic_call;
pub use global::{CommandExecutor, execute_command, execute_nvim_cmd};
pub use metadata::{ApiType, FunctionMetadata, TypeRef};
pub use mode::{
    command_history, current_cmdline_text, current_cmdline_type, current_mode_name,
    recording_register,
};
pub use ox_api_macros::api;
pub use ox_excmd::ExCommand;
pub use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};
pub use registry::{DispatchFn, Registry, RegistryError, core};
pub use runtime::{
    AutocmdExecution, AutocmdExecutor, ChannelInfo, ChannelSink, FileIO, LuaExecutor, MatchKind,
    StdFileIO, close_channel, register_channel, runtime_get_named, set_autocmd_executor,
    set_channel_sink, set_command_executor, set_file_io, set_job_sink, set_lua_executor,
    set_mode_machine,
};
pub use ui::nvim_paste;
