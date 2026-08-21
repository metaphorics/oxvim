#![forbid(unsafe_code)]
//! Foundation types for the Oxvim core: the msgpack-RPC [`Object`] model,
//! editor handle newtypes, [`ApiError`], and the Vimscript [`Typval`] model.
//!
//! Dependency-free (standard library only); every other crate builds on these.

mod byte_str;
mod error;
mod handle;
mod object;
mod typval;

pub use byte_str::OxStr;
pub use error::ApiError;
pub use handle::{BufHandle, HandleError, TabHandle, WinHandle};
pub use object::{Dict, Object, EXT_TYPE_BUFFER, EXT_TYPE_TABPAGE, EXT_TYPE_WINDOW};
pub use typval::{
    DictData, DictRef, Funcref, ListData, ListRef, LockScope, LockState, Special, Typval, VAR_BLOB,
    VAR_BOOL, VAR_DICT, VAR_FLOAT, VAR_FUNC, VAR_LIST,
    VAR_NUMBER, VAR_PARTIAL, VAR_SPECIAL, VAR_STRING, VAR_UNKNOWN,
};