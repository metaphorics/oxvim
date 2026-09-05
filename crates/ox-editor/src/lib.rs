#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]
//! Single-writer editor state, frame-tree layout, options, registers, and marks.

pub mod arglist;

mod builtins;

pub mod autocmd;
pub mod buffer;
pub mod decoration;
pub mod efm;
pub mod editor;
pub mod excmd_exec;
pub mod extmark;
pub mod fold;
pub mod fs_builtins;
pub mod include_search;
pub mod layout;
pub mod lvalue;
pub mod mapping;
pub mod mode;
pub mod motion;
pub mod quickfix;
pub mod ops;
pub mod search;
pub mod tags;
pub mod textobject;
pub mod script;
pub mod diffmode;
pub mod visual;
pub mod insert;
pub mod indent;
pub mod job;
pub mod marks;
pub mod options;
mod put;
pub mod register;
pub mod userfunc;
pub mod typeahead;

#[cfg(test)]
pub(crate) mod test_guard;
pub use arglist::{ArgList, ArgRangeError};
pub use autocmd::{
    AutocmdAction, AutocmdContext, AutocmdDefinition, AutocmdError, AutocmdFilter, AutocmdKind,
    AutocmdOptions, AutocmdSink, Autocmds, AugroupId, Event, FiringPlan, PatternKind, EVENT_COUNT,
};
pub use buffer::{
    BufferAttachSubscription, BufferFlags, BufferState, BufferStateError, BufferTextEditError,
    BufferTextEditRequest,
};
pub use decoration::Decorations;
pub use editor::{
    expand_buffer_name, BufferEditMode, BufferRelease, ChannelIds, DirectoryError, DirectoryScope,
    Editor, EditorError, HighlightDefinition, LineReplaceRequest, Message, MessageDestination,
    MessageKind, MessageRouting,
};
pub use excmd_exec::{
    vim_variable_is_writable, ExEditorAccess, ExExecutor, ExecError, ExecOutcome, LuaExec,
    LuaExecError, PendingEditMode, UserCommand, UserCommandComplete, UserCommandRange,
    VimException, VimExceptionKind,
};
#[cfg(any(test, feature = "testutils"))]
pub use excmd_exec::TestEditorAccess;
pub use extmark::{
    Extmark, ExtmarkAttributes, ExtmarkEnd, ExtmarkFlags, ExtmarkGravity, ExtmarkHighlightMode,
    ExtmarkId, ExtmarkPlacement, ExtmarkPosition, ExtmarkVirtualLinesOverflow,
    ExtmarkVirtualTextPosition, Extmarks, NamespaceId, VirtualLine, VirtualTextChunk,
};
pub use fold::Folds;
pub use indent::{ExprEval, IndentEvalContext, IndentExprError, NullExprEval};
pub use layout::{
    Anchor, Border, BorderText, FloatingWindow, Frame, Geometry, Layout, LayoutError, LeafFrame,
    Margins, RelativeTo, TabpageState, TextAlignment, WinConfig, WindowApiState, WindowState,
};
pub use mapping::{
    Abbreviation, Lookup, MapFlags, MapMode, MapModes, MapScope, Mapping, MappingAction,
    MappingError, MappingOptions, Mappings,
};
pub use mode::{CmdlineKind, CmdlineState, InsertState, Mode, ModeError, ModeMachine, NormalState, OperatorPendingState, Step};
pub use script::{
    FileEntry, FileIO, FileKind, FileMetadata, LogicalLine, RealFileIO, RuntimeRoot, ScriptCtx, ScriptError, ScriptInfo, Sid,
    SourceFrame, StdPath, default_runtimepath, stdpath,
};
pub use userfunc::{
    CallFrame, FunctionSignature, UserFunc, UserFuncError, UserFuncFlags, UserFunctions,
    MAX_FUNC_DEPTH,
};
pub use motion::{FindDirection, FindMotion, Motion, MotionKind};
pub use ops::{EditRange, Operator, OperatorError, OperatorRequest, OperatorResult};
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
    ClipboardProvider, RegisterContent, RegisterError, RegisterKind, Registers, Selection,
};
pub use typeahead::{
    Key, KeyDecodeError, Keys, Remap, Typeahead, TypeaheadError, TypeaheadFlags, KE_EVENT, KE_FILLER,
    KS_EXTRA, KS_SPECIAL, KS_ZERO, K_SPECIAL,
};

#[cfg(test)]
pub(crate) static PROCESS_STATE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod excmd_exec_control_tests;
#[cfg(test)]
mod excmd_exec_editor_tests;
#[cfg(test)]
mod eval_lang_contract_tests;
#[cfg(test)]
mod excmd_exec_function_tests;
#[cfg(test)]
mod lvalue_tests;
#[cfg(test)]
mod excmd_exec_regex_tests;
#[cfg(test)]
mod excmd_exec_state_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod indent_tests;
#[cfg(test)]
mod mode_tests;
#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod position_tests;
#[cfg(test)]
mod task09d_tests;
#[cfg(test)]
mod tests;
