#![forbid(unsafe_code)]
//! Single-writer editor state, frame-tree layout, options, registers, and marks.

pub mod autocmd;
pub mod buffer;
pub mod decoration;
pub mod editor;
pub mod excmd_exec;
pub mod extmark;
pub mod fold;
pub mod layout;
pub mod mapping;
pub mod mode;
pub mod motion;
pub mod ops;
pub mod search;
pub mod textobject;
pub mod script;
pub mod visual;
pub mod insert;
pub mod marks;
pub mod options;
pub mod register;
pub mod userfunc;
pub mod typeahead;

pub use autocmd::{
    AutocmdAction, AutocmdContext, AutocmdError, AutocmdKind, AutocmdOptions, AutocmdSink,
    Autocmds, AugroupId, DeleteAutocmds, Event, FiringPlan, PatternKind, EVENT_COUNT,
};
pub use buffer::{
    BufferAttachSubscription, BufferState, BufferStateError, UserCommandDefinition,
};
pub use decoration::Decorations;
pub use editor::{BufferRelease, Editor, EditorError, HighlightDefinition, Message, MessageKind};
pub use excmd_exec::{
    ExExecutor, ExecError, ExecOutcome, UserCommand, VimException, VimExceptionKind,
};
pub use extmark::Extmarks;
pub use fold::Folds;
pub use layout::{
    Anchor, Border, BorderText, FloatingWindow, Frame, Geometry, Layout, LayoutError, LeafFrame,
    Margins, RelativeTo, TabpageState, TextAlignment, WinConfig, WindowApiState, WindowState,
};
pub use mapping::{
    Abbreviation, Lookup, MapMode, MapModes, MapScope, Mapping, MappingAction, MappingError,
    MappingExprSink, MappingOptions, Mappings,
};
pub use mode::{CmdlineState, InsertState, Mode, ModeError, ModeMachine, NormalState, OperatorPendingState, Step};
pub use script::{
    FileIO, LogicalLine, RealFileIO, RuntimeRoot, ScriptCtx, ScriptError, ScriptInfo, Sid,
    SourceFrame,
};
pub use userfunc::{
    CallFrame, FunctionSignature, UserFunc, UserFuncError, UserFuncFlags, UserFunctions,
    MAX_FUNC_DEPTH,
};
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
mod excmd_exec_control_tests;
#[cfg(test)]
mod excmd_exec_editor_tests;
#[cfg(test)]
mod excmd_exec_function_tests;
#[cfg(test)]
mod excmd_exec_regex_tests;
#[cfg(test)]
mod excmd_exec_state_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod mode_tests;
#[cfg(test)]
mod task09d_tests;
#[cfg(test)]
mod tests;
