#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]
//! Single-writer editor state, frame-tree layout, options, registers, and marks.

pub mod arglist;

mod builtins;

pub mod autocmd;
pub mod buffer;
pub mod decoration;
pub mod diffmode;
pub mod editor;
pub mod efm;
pub mod excmd_exec;
pub mod extmark;
pub mod fold;
pub mod fs_builtins;
pub mod highlight_init;
pub mod include_search;
pub mod indent;
pub mod insert;
pub mod job;
pub mod layout;
pub mod lvalue;
pub mod mapping;
pub mod marks;
pub mod mode;
pub mod motion;
pub mod ops;
pub mod options;
mod put;
pub mod quickfix;
pub mod register;
pub mod script;
pub mod search;
pub mod server;
pub mod shada;
pub mod tags;
pub mod terminal_screen;
pub mod textobject;
pub mod typeahead;
pub mod userfunc;
pub mod visual;

#[cfg(test)]
pub(crate) mod test_guard;
pub use arglist::{ArgList, ArgRangeError};
pub use autocmd::{
    AugroupId, AutocmdAction, AutocmdContext, AutocmdDefinition, AutocmdError, AutocmdFilter,
    AutocmdKind, AutocmdOptions, AutocmdSink, Autocmds, EVENT_COUNT, Event, FiringPlan,
    PatternKind,
};
pub use buffer::{
    BufferAttachSubscription, BufferBytesEvent, BufferFlags, BufferState, BufferStateError,
    BufferTextEditError, BufferTextEditRequest,
};
pub use decoration::Decorations;
pub use editor::{
    BufferEditMode, BufferRelease, ChannelIds, DirectoryError, DirectoryScope, Editor, EditorError,
    HighlightDefinition, LineReplaceRequest, Message, MessageDestination, MessageKind,
    MessageRouting, expand_buffer_name,
};
#[cfg(any(test, feature = "testutils"))]
pub use excmd_exec::TestEditorAccess;
pub use excmd_exec::{
    ExEditorAccess, ExExecutor, ExecError, ExecOutcome, FocusContainer, FocusTransition, LuaExec,
    LuaExecError, PendingEditMode, UserCommand, UserCommandComplete, UserCommandRange,
    VimException, VimExceptionKind, focus_transition, vim_variable_is_writable,
};
pub use extmark::{
    Extmark, ExtmarkAttributes, ExtmarkEnd, ExtmarkError, ExtmarkFlags, ExtmarkGravity,
    ExtmarkHighlightMode, ExtmarkId, ExtmarkPlacement, ExtmarkPosition,
    ExtmarkVirtualLinesOverflow, ExtmarkVirtualTextPosition, Extmarks, NamespaceId, VirtualLine,
    VirtualTextChunk,
};
pub use fold::Folds;
pub use indent::{ExprEval, IndentEvalContext, IndentExprError, NullExprEval};
pub use job::{JobCallbacks, JobEvent, JobManager, JobStartOptions};
pub use layout::{
    Anchor, Border, BorderText, FloatingWindow, Frame, Geometry, Layout, LayoutError, LeafFrame,
    Margins, RelativeTo, TabpageState, TextAlignment, WinConfig, WindowApiState, WindowState,
};
pub use mapping::{
    Abbreviation, Lookup, MapFlags, MapMode, MapModes, MapScope, Mapping, MappingAction,
    MappingError, MappingOptions, Mappings,
};
pub use marks::{
    Changelists, GlobalMarks, HISTORY_CAPACITY, Jumplist, LocalMarks, MarkError, MarkLocation,
    MarkTarget,
};
pub use mode::{
    CmdlineKind, CmdlineState, InsertState, Mode, ModeError, ModeMachine, NormalState,
    OperatorPendingState, Step,
};
pub use motion::{FindDirection, FindMotion, Motion, MotionKind};
pub use ops::{EditRange, Operator, OperatorError, OperatorRequest, OperatorResult};
pub use options::{
    OPTION_COUNT, OPTION_METADATA, OptionDefault, OptionDefaultValue, OptionError, OptionListKind,
    OptionMetadata, OptionScope, OptionStore, OptionType, OptionValue, option_metadata,
};
pub use register::{
    ClipboardProvider, RegisterContent, RegisterError, RegisterKind, Registers, Selection,
};
pub use script::{
    FileEntry, FileIO, FileKind, FileMetadata, LogicalLine, RealFileIO, RuntimeRoot, ScriptCtx,
    ScriptError, ScriptInfo, Sid, SourceFrame, StdPath, default_runtimepath, stdpath,
};
pub use search::{SearchDirection, SearchError, SearchOffset, SearchResult, SearchState};
pub use server::{ServerHost, prepare_server_address, server_address_new};
pub use typeahead::{
    K_SPECIAL, KE_EVENT, KE_FILLER, KS_EXTRA, KS_MODIFIER, KS_SPECIAL, KS_ZERO, Key,
    KeyDecodeError, Keys, MOD_MASK_ALT, MOD_MASK_CTRL, MOD_MASK_META, MOD_MASK_SHIFT, Remap,
    Typeahead, TypeaheadError, TypeaheadFlags,
};
pub use userfunc::{
    CallFrame, FunctionSignature, MAX_FUNC_DEPTH, UserFunc, UserFuncError, UserFuncFlags,
    UserFunctions,
};
pub use visual::{VisualKind, VisualState};

#[cfg(test)]
pub(crate) static PROCESS_STATE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod eval_lang_contract_tests;
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
mod indent_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod lvalue_tests;
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
