#![forbid(unsafe_code)]
//! #[api]-annotated nvim_* API implementations.

extern crate self as ox_api;

mod autocmd;
mod buffer;
mod channel;
mod context;
mod convert;
mod deprecated;
mod extmark;
mod global;
mod metadata;
mod registry;
mod runtime;
mod tabpage;
mod ui;
mod window;

#[cfg(test)]
mod tests;

pub use convert::{FromObject, IntoObject, LuaRef, Nil};
pub use global::{CommandExecutor, execute_command};
pub use metadata::{ApiType, FunctionMetadata, TypeRef};
pub use ox_api_macros::api;
pub use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};
pub use registry::{core, DispatchFn, Registry, RegistryError};
pub use runtime::{AutocmdExecutor, ChannelSink, FileIO, StdFileIO, set_autocmd_executor, set_channel_sink, set_runtime_files};