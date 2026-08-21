#![forbid(unsafe_code)]
//! Single-writer editor state, frame-tree layout, options, registers, and marks.

pub mod buffer;
pub mod editor;
pub mod layout;
pub mod marks;
pub mod options;
pub mod register;

pub use buffer::{BufferState, BufferStateError};
pub use editor::{BufferRelease, Editor, EditorError};
pub use layout::{
    Anchor, Border, BorderText, FloatingWindow, Frame, Geometry, Layout, LayoutError, LeafFrame,
    Margins, RelativeTo, TabpageState, TextAlignment, WinConfig, WindowState,
};
pub use marks::{
    Changelists, GlobalMarks, Jumplist, LocalMarks, MarkError, MarkLocation, MarkTarget,
    HISTORY_CAPACITY,
};
pub use options::{
    option_metadata, OptionDefault, OptionDefaultValue, OptionError, OptionListKind, OptionMetadata,
    OptionScope, OptionStore, OptionType, OptionValue, OPTION_COUNT, OPTION_METADATA,
};
pub use register::{
    put_content, ClipboardProvider, ExpressionEvaluator, RegisterContent, RegisterError,
    RegisterKind, Registers, Selection,
};

#[cfg(test)]
mod tests;
