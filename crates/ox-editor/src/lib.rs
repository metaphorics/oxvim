#![forbid(unsafe_code)]
//! Single-writer editor state, frame-tree layout, options, registers, and marks.

pub mod autocmd;
pub mod buffer;
pub mod editor;
pub mod layout;
pub mod mapping;
pub mod mode;
pub mod motion;
pub mod ops;
pub mod search;
pub mod textobject;
pub mod visual;
pub mod insert;
pub mod marks;
pub mod options;
pub mod register;
pub mod typeahead;

pub use autocmd::{
    AutocmdAction, AutocmdContext, AutocmdError, AutocmdKind, AutocmdOptions, AutocmdSink,
    Autocmds, AugroupId, DeleteAutocmds, Event, FiringPlan, PatternKind, EVENT_COUNT,
};
pub use buffer::{
    BufferAttachSubscription, BufferState, BufferStateError, UserCommandDefinition,
};
pub use editor::{BufferRelease, Editor, EditorError, Message, MessageKind};
pub use layout::{
    Anchor, Border, BorderText, FloatingWindow, Frame, Geometry, Layout, LayoutError, LeafFrame,
    Margins, RelativeTo, TabpageState, TextAlignment, WinConfig, WindowApiState, WindowState,
};
pub use mapping::{
    Abbreviation, Lookup, MapMode, MapModes, MapScope, Mapping, MappingAction, MappingError,
    MappingExprSink, MappingOptions, Mappings,
};
pub use mode::{CmdlineState, InsertState, Mode, ModeError, ModeMachine, NormalState, OperatorPendingState, Step};
pub use motion::{FindDirection, FindMotion, Motion, MotionKind};
pub use ops::{EditRange, Operator, OperatorError, OperatorResult};
pub use search::{SearchDirection, SearchError, SearchOffset, SearchResult, SearchState};
pub use visual::{VisualKind, VisualState};
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
mod mode_tests;
#[cfg(test)]
mod tests;
