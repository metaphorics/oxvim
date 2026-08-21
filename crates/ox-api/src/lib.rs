#![forbid(unsafe_code)]
//! #[api]-annotated nvim_* API implementations.

extern crate self as ox_api;

mod convert;
mod metadata;
mod registry;

pub use convert::{FromObject, IntoObject, LuaRef, Nil};
pub use metadata::{ApiType, FunctionMetadata, TypeRef};
pub use ox_api_macros::api;
pub use ox_types::{ApiError, BufHandle, Dict, Object, OxStr, TabHandle, WinHandle};
pub use registry::{DispatchFn, Registry, RegistryError};