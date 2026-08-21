#![forbid(unsafe_code)]
//! #[api]-annotated nvim_* API implementations.

extern crate self as ox_api;

mod buffer;
mod convert;
mod global;
mod metadata;
mod registry;
mod tabpage;
mod window;

#[cfg(test)]
mod tests;

pub use convert::{FromObject, IntoObject, LuaRef, Nil};
pub use global::{CommandExecutor, execute_command};
pub use metadata::{ApiType, FunctionMetadata, TypeRef};
pub use ox_api_macros::api;
pub use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};
pub use registry::{core, DispatchFn, Registry, RegistryError};