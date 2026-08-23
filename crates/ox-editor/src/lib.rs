#![forbid(unsafe_code)]
//! Single-writer editor state, frame-tree layout, options, registers, and marks.

pub mod arglist;

mod builtins;

pub mod autocmd;
pub mod buffer;
pub mod decoration;
pub mod editor;
pub mod excmd_exec;
pub mod extmark;
pub mod fold;
pub mod fs_builtins;
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
pub mod job;
pub mod marks;
pub mod options;
pub mod register;
pub mod userfunc;
pub mod typeahead;

pub use arglist::{ArgList, ArgRangeError};
pub use autocmd::{
    AutocmdAction, AutocmdContext, AutocmdDefinition, AutocmdError, AutocmdKind, AutocmdOptions,
    AutocmdSink, Autocmds, AugroupId, DeleteAutocmds, Event, FiringPlan, PatternKind, EVENT_COUNT,
};
pub use buffer::{
    BufferAttachSubscription, BufferState, BufferStateError, UserCommandDefinition,
};
pub use decoration::Decorations;
pub use editor::{
    BufferRelease, ChannelIds, Editor, EditorError, HighlightDefinition, Message, MessageDestination,
    MessageKind, MessageRouting,
};
pub use excmd_exec::{
    vim_variable_is_writable, ExExecutor, ExecError, ExecOutcome, LuaExec, LuaExecError, UserCommand,
    VimException, VimExceptionKind,
};
pub use extmark::{
    Extmark, ExtmarkAttributes, ExtmarkEnd, ExtmarkGravity, ExtmarkId, ExtmarkPlacement,
    ExtmarkPosition, Extmarks, NamespaceId, VirtualLine, VirtualTextChunk,
};
pub use fold::Folds;
pub use layout::{
    Anchor, Border, BorderText, FloatingWindow, Frame, Geometry, Layout, LayoutError, LeafFrame,
    Margins, RelativeTo, TabpageState, TextAlignment, WinConfig, WindowApiState, WindowState,
};
pub use mapping::{
    Abbreviation, Lookup, MapMode, MapModes, MapScope, Mapping, MappingAction, MappingError,
    MappingOptions, Mappings,
};
pub use mode::{CmdlineKind, CmdlineState, InsertState, Mode, ModeError, ModeMachine, NormalState, OperatorPendingState, Step};
pub use script::{
    FileEntry, FileIO, FileKind, FileMetadata, LogicalLine, RealFileIO, RuntimeRoot, ScriptCtx, ScriptError, ScriptInfo, Sid,
    SourceFrame, default_runtimepath,
};
pub use userfunc::{
    CallFrame, FunctionSignature, UserFunc, UserFuncError, UserFuncFlags, UserFunctions,
    MAX_FUNC_DEPTH,
};
pub use motion::{FindDirection, FindMotion, Motion, MotionKind};
pub use ops::{EditRange, Operator, OperatorError, OperatorResult};
pub use search::{SearchDirection, SearchError, SearchOffset, SearchResult, SearchState};
pub use visual::{VisualKind, VisualState};
pub use job::{JobCallbacks, JobEvent, JobManager, JobStartOptions};
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
    Key, KeyDecodeError, Keys, Remap, Typeahead, TypeaheadError, TypeaheadFlags, KE_EVENT, KE_FILLER,
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
mod position_tests;
#[cfg(test)]
mod task09d_tests;
#[cfg(test)]
mod tests;
