#![forbid(unsafe_code)]
//! Rope-backed text, marks, undo history, and Neovim recovery formats.

pub mod buffer;
pub mod marks;
pub mod shada;
pub mod swapfile;
pub mod undo;
pub mod undo_file;

pub use buffer::{Buffer, BufferError};
pub use marks::{MarkId, Marks, Position};
pub use shada::{Entry as ShaDaEntry, EntryType as ShaDaEntryType, ShaDa, ShaDaError};
pub use swapfile::{SwapError, SwapFile};
pub use undo::{Cursor, HeaderRecord, LineEdit, UndoEntry, UndoError, UndoStep, UndoSummary, UndoTree};
pub use undo_file::{UndoFile, UndoFileError};

#[cfg(test)]
mod tests;
