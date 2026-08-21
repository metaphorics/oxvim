#![forbid(unsafe_code)]
//! Single-writer editor state, frame-tree layout, options, registers, and marks.

pub mod autocmd;
pub mod buffer;
pub mod editor;
pub mod layout;
pub mod mapping;
pub mod marks;
pub mod options;
pub mod register;
pub mod typeahead;

pub use autocmd::{
    AutocmdAction, AutocmdContext, AutocmdError, AutocmdKind, AutocmdOptions, AutocmdSink,
    Autocmds, AugroupId, DeleteAutocmds, Event, FiringPlan, PatternKind, EVENT_COUNT,
};
pub use buffer::{BufferState, BufferStateError};
pub use editor::{BufferRelease, Editor, EditorError};
pub use layout::{
    Anchor, Border, BorderText, FloatingWindow, Frame, Geometry, Layout, LayoutError, LeafFrame,
    Margins, RelativeTo, TabpageState, TextAlignment, WinConfig, WindowState,
};
pub use mapping::{
    Abbreviation, Lookup, MapMode, MapModes, MapScope, Mapping, MappingAction, MappingError,
    MappingExprSink, MappingOptions, Mappings,
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
pub use typeahead::{
    Key, KeyDecodeError, Keys, Remap, Typeahead, TypeaheadError, TypeaheadFlags, KE_FILLER,
    KS_EXTRA, KS_SPECIAL, KS_ZERO, K_SPECIAL,
};

#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod tests;
