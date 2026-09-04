//! Ex command execution against the single-writer [`Editor`] model.
//!
//! Parsing remains in `ox-excmd`; this module owns command/control state,
//! script and function frames, exception transfer, user commands, and the
//! narrow host adapters needed by `ox-eval` and `ox-regex`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ox_eval::scope::{OptionScope as EvalOptionScope, ScopeMap};
use ox_eval::{
    BufferHost, BuiltinHost, Builtins, ClosureRegistry, EvalError, EvalErrorKind, Evaluator,
    Parser as ExprParser, RegexEngine, Scope, ScopeKind,
};
use ox_excmd::{
    AddrType, Address, AddressBase, CommandFlags, CommandModifier, ErrorCode, ExCommand,
    ModifierKind, ParseError, Parser as ExParser, Range, RangeKind, ResolvedCommand,
    UserCommandInfo, UserCommandMatch, UserCommandProvider, effective_addr_type, effective_flags,
    resolve_command,
};
use ox_regex::{
    CompileError as RegexCompileError, Magic, Text as RegexText, compile as compile_regex,
    exec_at as regex_exec_at,
};
use ox_sys::LocaleCategory;
use ox_text::{Buffer, Position};
use ox_types::{
    BufHandle, Dict, DictEntry, DictEntryFlags, DictRef, Funcref, Object, OxStr, Special,
    TabHandle, Typval, WinHandle,
};

use crate::autocmd::{
    AugroupId, AutocmdContext, AutocmdFilter, AutocmdKind, AutocmdOptions, Event, FiringPlan,
};
use crate::buffer::BufferTextEditRequest;
use crate::builtins::position::cell_width;
use crate::decoration::{CallbackPhase, RedrawEntry};
use crate::extmark::{
    ExtmarkAttributes, ExtmarkId, ExtmarkPlacement, ExtmarkPosition, NamespaceId, SignGroup,
};
use crate::fold::{FoldMethod, Position as FoldPosition};
use crate::lvalue::{
    assign_lvalue, assign_vim_variable, expand_curly_target, names_read_only_entry,
    parse_and_bind_lvalue, read_lvalue, remove_lvalue, vim_variable_type,
};
use crate::mapping::{
    MapFlags, MapMode, MapModes, MapScope, Mapping, MappingAction, MappingOptions,
};
use crate::options::{
    CommaItems, OPTION_METADATA, OptionListKind, OptionScope, OptionType, OptionValue,
    find_unescaped,
};
use crate::quickfix::QuickfixMove;
use crate::register::RegisterContent;
use crate::script::{FileIO, LogicalLine, RealFileIO, ScriptCtx, Sid, SourceContext};
use crate::search::{SearchDirection, SearchError, SearchState};
use crate::typeahead::{Keys, Remap, TypeaheadFlags, special_notation};
use crate::userfunc::{UserFuncError, UserFunctions};
use crate::{
    BufferRelease, ChannelIds, DirectoryError, DirectoryScope, Editor, EditorError, Geometry,
    JobManager, Message, MessageKind, Mode, ModeMachine,
};

/// `FILETYPE_FILE` … `INDOFF_FILE` (`globals.h:37-60`): the runtime files
/// `:filetype` sources, in upstream's whitespace-separated pattern form.
const FILETYPE_FILE: &str = "filetype.lua filetype.vim";
const FTPLUGIN_FILE: &str = "ftplugin.vim";
const INDENT_FILE: &str = "indent.vim";
const FTOFF_FILE: &str = "ftoff.vim";
const FTPLUGOF_FILE: &str = "ftplugof.vim";
const INDOFF_FILE: &str = "indoff.vim";

/// Result of one public execution entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecOutcome {
    /// Commands completed normally.
    Completed,
    /// `:finish` terminated the active sourced script.
    Finished,
    /// `:quit` closed the final window or requested host termination;
    /// `:cquit [code]` carries its requested process exit code.
    Quit(i64),
}
/// Editor mode requested by an Ex command and applied by the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingEditMode {
    /// Enter Insert mode at the current cursor.
    Insert,
    /// Enter Insert mode after moving to the end of the current line.
    Append,
    /// Enter Replace mode at the current cursor.
    Replace,
    /// Leave Insert or terminal-input mode.
    StopInsert,
}

/// Classification of a catchable Vim exception.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VimExceptionKind {
    /// Explicit `:throw`.
    Throw,
    /// Editor error with its stable `E` code.
    Error(String),
}

/// Catchable exception object carried by the control-flow interpreter.
#[derive(Clone, Debug)]
pub struct VimException {
    /// Explicit throw or editor error.
    pub kind: VimExceptionKind,
    /// Thrown/evaluated value. Boxed so carrying an exception does not
    /// inflate every control-flow and executor result by the payload size.
    pub value: Box<Typval>,
    /// Upstream-style source/call chain.
    pub throwpoint: String,
    /// `get_exception_string`'s `cmdname` argument (`ex_eval.c:383-401`): the
    /// Ex command the error escaped from, which is what `v:exception` is
    /// prefixed with. `None` is upstream's NULL `cmdname` — a user command, an
    /// unresolvable command name, or no command at all — and prefixes `Vim:`.
    pub command: Option<String>,
}

impl VimException {
    /// String matched by a `:catch` pattern, and the value of `v:exception`.
    ///
    /// `get_exception_string` (`ex_eval.c:383-401`) builds an error
    /// exception's value as `Vim({cmdname}):{message}`, or `Vim:{message}`
    /// with no command name. Without the prefix a `:catch /^Vim(/` pattern —
    /// which is how a script distinguishes an editor error from its own
    /// `:throw` — never matches, and `v:exception` names no command.
    #[must_use]
    pub fn message(&self) -> String {
        let value = typval_to_display(&self.value, false);
        match &self.kind {
            VimExceptionKind::Throw => value,
            VimExceptionKind::Error(code) => match &self.command {
                Some(command) => format!("Vim({command}):{code}: {value}"),
                None => format!("Vim:{code}: {value}"),
            },
        }
    }
}

/// Public Ex execution failure.
#[derive(Debug)]
pub enum ExecError {
    /// Command-line parser failure.
    Parse(ParseError),
    /// Expression parser/evaluator failure outside a catchable script frame.
    Eval(EvalError),
    /// Filesystem failure with path context.
    Io {
        /// Path whose read or write failed.
        path: PathBuf,
        /// Operating-system error text.
        message: String,
    },
    /// Editor-owned state mutation failure.
    Editor(String),
    /// A user command with this name already exists and `force` was not
    /// given, so the API can surface exactly this case.
    DuplicateCommand {
        /// The name that already exists.
        name: String,
    },
    /// Uncaught Vim exception.
    Vim(VimException),
    /// Generated command/function recognized but not implemented.
    NotImplemented(String),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => error.fmt(f),
            Self::Eval(error) => error.fmt(f),
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Editor(message) => f.write_str(message),
            Self::DuplicateCommand { name } => f
                .write_str("E174: Command already exists: add ! to replace it")
                .and_then(|()| write!(f, " ({name})")),
            Self::Vim(exception) => write!(f, "{}\n{}", exception.message(), exception.throwpoint),
            Self::NotImplemented(name) => write!(f, "not implemented: {name}"),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<ParseError> for ExecError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

/// Error returned by the host-independent Lua execution seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LuaExecError {
    /// Lua source could not be compiled or loaded.
    Load(String),
    /// Lua source raised an error while running.
    Runtime(String),
    /// A value could not cross the Lua/Object boundary.
    Conversion(String),
}

impl fmt::Display for LuaExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(message) | Self::Runtime(message) | Self::Conversion(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for LuaExecError {}

/// Host seam used by Ex Lua commands without coupling `ox-editor` to `ox-lua`.
pub trait LuaExec {
    /// Compile and execute one Lua chunk with varargs.
    ///
    /// # Errors
    ///
    /// Returns the host's load, runtime, or value-conversion failure.
    fn execute_chunk(&mut self, code: &str, args: Vec<Object>) -> Result<Object, LuaExecError>;

    /// Load and execute one Lua file.
    ///
    /// # Errors
    ///
    /// Returns the host's load, runtime, or value-conversion failure.
    fn execute_file(&mut self, path: &Path) -> Result<(), LuaExecError>;

    /// Evaluate one Lua expression with `_A` bound to `arg` (`luaeval()`).
    ///
    /// # Errors
    ///
    /// Returns the host's load, runtime, or value-conversion failure; hosts
    /// without a typval bridge report the missing capability.
    ///
    /// Hosts wrap the expression exactly like upstream `nlua_call_luaeval`
    /// (`local _A=select(1,...) return (<expr>)`) and convert the argument
    /// and result with typval semantics.
    fn eval_expression(
        &mut self,
        _expression: &str,
        _arg: Option<&Typval>,
    ) -> Result<Typval, LuaExecError> {
        Err(LuaExecError::Runtime(
            "luaeval host is not installed".to_owned(),
        ))
    }

    /// Invoke a Lua registry callback with values converted by the host.
    ///
    /// # Errors
    ///
    /// Returns the host's runtime or value-conversion failure.
    fn invoke_callback(
        &mut self,
        _reference: usize,
        _args: Vec<Object>,
    ) -> Result<Object, LuaExecError> {
        Err(LuaExecError::Runtime(
            "Lua callbacks are not installed".to_owned(),
        ))
    }

    /// Releases one Lua registry callback reference.
    ///
    /// # Errors
    ///
    /// Returns the host's runtime failure.
    fn free_callback(&mut self, _reference: usize) -> Result<(), LuaExecError> {
        Err(LuaExecError::Runtime(
            "Lua callbacks are not installed".to_owned(),
        ))
    }

    /// Runs one non-blocking event-loop turn for long-running scripts.
    ///
    /// Ex loop back edges call this every [`BREAKCHECK_SKIP`] iterations, the
    /// cadence of upstream's `line_breakcheck()`, so a script cannot starve
    /// event handles. Hosts that re-enter Lua rebind their scoped API surface
    /// over the session for the duration of the turn instead of borrowing a
    /// shared editor.
    ///
    /// # Errors
    ///
    /// Returns the host's runtime failure while servicing the turn.
    fn run_event_turn(&mut self) -> Result<(), LuaExecError> {
        Ok(())
    }

    /// Consumes a result whose caller does not retain it.
    fn discard_result(&mut self, _result: Object) {}
}

/// Accessor seam for Ex execution against a borrowed editor.
///
/// The Ex executor's public entry points take this instead of `&mut Editor`
/// so hosts (:lua, user callbacks, autocmd dispatch) can re-enter the API
/// mid-command without a live editor borrow. Closures passed here MUST NOT
/// run reentrant host code — borrows are statement-scoped.
///
/// Defined in `ox-editor` (dependency-cycle-free); implemented by
/// `ox_api::ApiSession`. Generic methods monomorphize to the one real
/// accessor, so the seam adds no allocation or dynamic dispatch on the
/// command hot path.
pub trait ExEditorAccess {
    /// One statement-scoped mutable editor borrow.
    fn with_ex_editor<R>(&self, operation: impl FnOnce(&mut Editor) -> R) -> R;
}

/// Test-only accessor wrapping an `Editor` in a `RefCell` so the generic
/// executor entry points (`execute_line`, `call_builtin`, ...) can borrow it
/// through the `ExEditorAccess` seam without coupling tests to `ApiSession`.
#[cfg(any(test, feature = "testutils"))]
pub struct TestEditorAccess {
    editor: std::cell::RefCell<Editor>,
}

#[cfg(any(test, feature = "testutils"))]
impl TestEditorAccess {
    /// Wraps an editor for test-only Ex execution.
    #[must_use]
    pub fn new(editor: Editor) -> Self {
        Self {
            editor: std::cell::RefCell::new(editor),
        }
    }

    /// Borrows the editor immutably for post-execution assertions.
    #[must_use]
    pub fn editor(&self) -> std::cell::Ref<'_, Editor> {
        self.editor.borrow()
    }

    /// Borrows the editor mutably for setup or post-execution mutation.
    #[must_use]
    pub fn editor_mut(&self) -> std::cell::RefMut<'_, Editor> {
        self.editor.borrow_mut()
    }

    /// Unwraps the editor, consuming the accessor.
    #[must_use]
    pub fn into_inner(self) -> Editor {
        self.editor.into_inner()
    }
}

#[cfg(any(test, feature = "testutils"))]
impl ExEditorAccess for TestEditorAccess {
    fn with_ex_editor<R>(&self, operation: impl FnOnce(&mut Editor) -> R) -> R {
        operation(&mut self.editor.borrow_mut())
    }
}

/// Default the `-range` attribute selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserCommandRange {
    /// `-range`: the current line (`<line1>` == `<line2>` == cursor line).
    Dot,
    /// `-range=%`: the whole buffer.
    Percent,
    /// `-range={N}`: line `{N}`.
    Count(i64),
}

/// The `-complete` attribute of one user command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserCommandComplete {
    /// A named completion type (`arglist`, `custom`, ...).
    Name(String),
    /// A Lua completion callback registry reference.
    Callback(u64),
}

/// Definition created by `:command` or `nvim_create_user_command`.
///
/// This is the canonical value both creation paths share; the string body and
/// the Lua callback are mutually exclusive.
///
/// The bools mirror independent upstream attributes (`-bang`, `-range`,
/// `-register`, `-bar`, `-count`, `++keepscript`), not a state machine, so
/// grouping them would only hide the one-to-one mapping onto Vim's `EX_*` flags.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "public record mirrors Vim's independent user-command attributes"
)]
pub struct UserCommand {
    /// Canonical uppercase command name.
    pub name: String,
    /// Ex body with `<args>`/`<bang>` placeholders. Empty for a Lua callback.
    pub body: String,
    /// Accepted argument count (`0`, `1`, `?`, `+`, or `*`).
    pub nargs: char,
    /// Whether invocation accepts `!` (`-bang`).
    pub accepts_bang: bool,
    /// Whether invocation accepts a range (`-range`, implied by `-count`).
    pub accepts_range: bool,
    /// Whether invocation accepts a register (`-register`).
    pub accepts_register: bool,
    /// Whether a bar may follow the command (`-bar`).
    pub bar: bool,
    /// Whether invocation accepts a count (`-count`).
    pub accepts_count: bool,
    /// Default for `-count[={N}]`; the count when none is given.
    pub count_default: Option<i64>,
    /// Domain the command's addresses count in (`-addr=`).
    pub addr_type: AddrType,
    /// Default the `-range` attribute selects.
    pub default_range: Option<UserCommandRange>,
    /// `desc` from `nvim_create_user_command`.
    pub desc: String,
    /// `-complete[=…]` or the API `complete` option.
    pub completion: Option<UserCommandComplete>,
    /// Argument of `-complete=custom,{arg}` / `customlist,{arg}`.
    pub complete_arg: Option<String>,
    /// Lua registry reference of the callback; `None` runs `body`.
    pub callback: Option<u64>,
    /// Lua registry reference of the `preview` callback.
    pub preview: Option<u64>,
    /// Serialization SID: `script_context.sid` for Ex-defined commands,
    /// `-8` (`SID_LUA`) for API/Lua-defined ones.
    pub script_id: i64,
    /// SID, sequence, and source line of the `:command` that created this
    /// entry, upstream's `uc_script_ctx`. `SourceContext::default()` outside
    /// any script.
    pub script_context: SourceContext,
    /// Whether invocation keeps the caller's script context instead of
    /// switching to `script_context` (`ex_docmd.c` `EX_KEEPSCRIPT`).
    pub keepscript: bool,
}

impl Default for UserCommand {
    /// `-nargs=0` is the upstream default, not char `\0`.
    fn default() -> Self {
        Self {
            name: String::new(),
            body: String::new(),
            nargs: '0',
            accepts_bang: false,
            accepts_range: false,
            accepts_register: false,
            bar: false,
            accepts_count: false,
            count_default: None,
            addr_type: AddrType::None,
            default_range: None,
            desc: String::new(),
            completion: None,
            complete_arg: None,
            callback: None,
            preview: None,
            script_id: 0,
            script_context: SourceContext::default(),
            keepscript: false,
        }
    }
}

impl UserCommand {
    /// The argument flags the parser must govern this command by.
    ///
    /// One mapping from the definition attributes to upstream's `EX_*` set:
    /// `-nargs` shapes `EXTRA`/`NOSPC`/`NEEDARG` (`usercmd.c:815-840`), and
    /// `-count` implies `-range` with a zero-accepting count.
    #[must_use]
    pub fn flags(&self) -> CommandFlags {
        let mut bits = 0u32;
        if self.accepts_range {
            bits |= CommandFlags::RANGE.bits();
        }
        if self.accepts_bang {
            bits |= CommandFlags::BANG.bits();
        }
        bits |= match self.nargs {
            // `-nargs` to `EX_*` flags, `usercmd.c`'s `-nargs` branch.
            // The API's `_` form carries the same `1` semantics.
            '1' | '_' => {
                CommandFlags::EXTRA.bits()
                    | CommandFlags::NOSPC.bits()
                    | CommandFlags::NEEDARG.bits()
            }
            '?' => CommandFlags::EXTRA.bits() | CommandFlags::NOSPC.bits(),
            '+' => CommandFlags::EXTRA.bits() | CommandFlags::NEEDARG.bits(),
            '*' => CommandFlags::EXTRA.bits(),
            _ => 0,
        };
        if self.bar {
            bits |= CommandFlags::TRLBAR.bits();
        }
        if self.accepts_count {
            bits |= CommandFlags::COUNT.bits() | CommandFlags::ZEROR.bits();
        }
        if self.accepts_register {
            bits |= CommandFlags::REGSTR.bits();
        }
        CommandFlags::from_bits(bits)
    }

    /// The [`UserCommandInfo`] the command resolver hands the parser.
    #[must_use]
    pub fn info(&self) -> UserCommandInfo {
        UserCommandInfo {
            name: self.name.clone(),
            flags: self.flags(),
            addr_type: self.addr_type,
        }
    }
}

/// One user-command registry: the global table plus per-buffer tables.
///
/// The single owner of user commands in the editor; `ExExecutor` holds it as
/// an `Rc<RefCell<_>>` so primary and nested executors can share one table
/// through [`ExExecutor::share_user_commands_from`].
#[derive(Clone, Debug, Default)]
pub(crate) struct UserCommandRegistry {
    commands: BTreeMap<String, UserCommand>,
    buffer_commands: BTreeMap<BufHandle, BTreeMap<String, UserCommand>>,
}

impl UserCommandRegistry {
    /// The table a definition targets: one buffer's, or the global one.
    pub(crate) fn scope_mut(
        &mut self,
        buffer: Option<BufHandle>,
    ) -> &mut BTreeMap<String, UserCommand> {
        match buffer {
            Some(buffer) => self.buffer_commands.entry(buffer).or_default(),
            None => &mut self.commands,
        }
    }

    /// Drops one buffer's local commands; `bwipeout` and the API wipe path
    /// route through this, `:bunload`/`:bdelete` do not.
    pub(crate) fn remove_buffer(&mut self, buffer: BufHandle) {
        self.buffer_commands.remove(&buffer);
    }
}

/// A resolution view of [`UserCommandRegistry`]: the current buffer's local
/// table first, then the global one — upstream `uc_find`'s lookup order.
pub(crate) struct UserCommandLookup<'a> {
    pub(crate) registry: &'a UserCommandRegistry,
    pub(crate) buffer: Option<BufHandle>,
}

impl UserCommandLookup<'_> {
    /// Resolves one exact canonical name against the live view.
    pub(crate) fn get(&self, name: &str) -> Option<&UserCommand> {
        if let Some(buffer) = self.buffer
            && let Some(local) = self.registry.buffer_commands.get(&buffer)
            && let Some(definition) = local.get(name)
        {
            return Some(definition);
        }
        self.registry.commands.get(name)
    }

    fn resolve_exact(&self, typed: &str) -> Option<UserCommandInfo> {
        self.get(typed).map(UserCommand::info)
    }
}

impl UserCommandProvider for UserCommandLookup<'_> {
    fn resolve_user_command(&self, typed: &str) -> UserCommandMatch {
        if !typed.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
            return UserCommandMatch::None;
        }
        if let Some(info) = self.resolve_exact(typed) {
            return UserCommandMatch::Match(info);
        }
        // No exact hit: gather prefix candidates from both tables.
        let mut candidates = Vec::new();
        if let Some(buffer) = self.buffer
            && let Some(local) = self.registry.buffer_commands.get(&buffer)
        {
            candidates.extend(
                local
                    .keys()
                    .filter(|name| name.starts_with(typed))
                    .map(|name| local[name].info()),
            );
        }
        candidates.extend(
            self.registry
                .commands
                .keys()
                .filter(|name| name.starts_with(typed))
                .map(|name| self.registry.commands[name].info()),
        );
        match candidates.as_slice() {
            [] => UserCommandMatch::None,
            [info] => UserCommandMatch::Match(info.clone()),
            _ => UserCommandMatch::Ambiguous,
        }
    }
}

#[derive(Clone, Debug)]
enum RedirTarget {
    Register { name: char },
    Variable { name: String, append: bool },
    File { path: PathBuf },
}

#[derive(Clone, Debug)]
pub(crate) struct Redirection {
    target: RedirTarget,
    output: String,
    seen_messages: usize,
}

/// `:filetype` enablement, mirroring upstream's three `TriState` globals
/// `filetype_detect`, `filetype_plugin`, and `filetype_indent`
/// (`ex_docmd.c:7860-7884`). `None` is `kNone`: never switched either way.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FiletypeState {
    pub(crate) detect: Option<bool>,
    pub(crate) plugin: Option<bool>,
    pub(crate) indent: Option<bool>,
}

/// The command line `do_one_cmd` is executing, and the name it will report an
/// error against.
///
/// `command` is `Some` only for a resolved builtin: `cmdnames[ea.cmdidx].cmd_name`
/// (`ex_docmd.c:2385-2387`), which is the *canonical* name, so `:foldo` reports
/// `Vim(foldopen):`. A user command and an unresolvable name both pass NULL
/// there and report `Vim:`.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutingCommand {
    pub(crate) command: Option<String>,
    pub(crate) line: String,
}

/// Whether `code` is one `do_one_cmd` raises while reading the command line
/// itself, which is what reaches `append_command` (`ex_docmd.c:2375-2384`) and
/// therefore ends with the line it was reading.
///
/// Only the address codes are decided here: range resolution is the port's
/// equivalent of `get_address`, and E16/E493 are the only codes it produces.
/// The parse-time codes are decided at [`ExRuntime::parse_exception`] instead,
/// because a command *implementation* emits some of the same codes and must
/// not get the suffix — checked against the oracle, `:undojoin xyz` is
/// `Vim(undojoin):E488: Trailing characters: xyz:   undojoin xyz` while
/// `:call Foo()trailing` is `Vim(call):E488: Trailing characters: trailing`.
const fn reports_command_line(code: &str) -> bool {
    matches!(code.as_bytes(), b"E16" | b"E493")
}

/// Values bound to `<amatch>`, `<afile>`, and `<abuf>` while one autocmd action
/// runs. Empty/`None` outside an active event.
#[derive(Clone, Debug, Default)]
pub(crate) struct ActiveAutocmdContext {
    pub(crate) matched: String,
    pub(crate) file: String,
    pub(crate) buffer: Option<BufHandle>,
    /// Upstream `autocmd_nested` (`autocmd.c:1996`): whether the running
    /// handler was defined `++nested`, so events raised while it runs may
    /// execute immediately. `false` outside an active event.
    pub(crate) nested: bool,
}

/// One entry of a frame's `fc_defer` list: an explicit `defer()` call or
/// the delete-cleanup `writefile(..., 'D')` registers.
pub(crate) enum DeferredOp {
    /// Call the named function with the stored arguments.
    Call(String, Vec<Typval>),
    /// Remove one path the way deferred `delete()` would.
    Delete(PathBuf, crate::fs_builtins::DeleteMode),
}

pub(crate) struct ExRuntime<F: FileIO> {
    pub(crate) scripts: ScriptCtx<F>,
    pub(crate) functions: UserFunctions,
    /// One shared user-command registry; primary and nested executors hold
    /// the same `Rc` so definitions stay visible across both.
    pub(crate) user_commands: Rc<RefCell<UserCommandRegistry>>,
    pub(crate) const_vars: BTreeSet<String>,
    pub(crate) channel_ids: ChannelIds,
    pub(crate) jobs: Option<JobManager>,
    pub(crate) current_augroup: AugroupId,
    /// Whether the built-in `nvim.terminal` `TermClose` exit message is active.
    pub(crate) terminal_exit_message: bool,
    pub(crate) redirection: Option<Redirection>,
    /// `filetype_detect`/`filetype_plugin`/`filetype_indent`
    /// (`ex_docmd.c:7860-7884`): unset, enabled, or explicitly disabled.
    pub(crate) filetype: FiletypeState,
    /// `getout` (`main.c`:753) has begun, so `VimLeavePre`/`VimLeave` are done.
    pub(crate) exiting: bool,
    /// `do_one_cmd`'s view of the command it is running, which
    /// `do_errthrow`'s `cmdname` argument (`ex_docmd.c:2385-2387`) and
    /// `append_command` (`ex_docmd.c:2993`) both read.
    ///
    /// This is runtime state rather than a parameter threaded through the
    /// dispatcher for the same reason it is a global upstream: an error can be
    /// raised anywhere below the command — inside expression evaluation, a
    /// nested function, a buffer mutation — and every one of those has to
    /// produce the same `Vim({cmdname}):` prefix without knowing it exists.
    /// Threading it instead would mean touching several hundred `error_flow`
    /// call sites and would still miss the next one.
    pub(crate) executing: ExecutingCommand,
    /// Active `<amatch>`/`<afile>`/`<abuf>` binding for the current autocmd action.
    pub(crate) active_autocmd: ActiveAutocmdContext,
    /// `autocmd_busy` (`autocmd.c:1657`): how many [`FiringPlan`]s are
    /// currently executing. While it is nonzero, new events fire only when
    /// forced by a value change or raised through a `++nested` handler.
    pub(crate) autocmd_busy: usize,
    /// `ft_recursive` (`autocmd.c:2518`): `FileType` plans currently executing.
    /// A same-value `filetype` assignment inside one is suppressed even from
    /// a `++nested` handler, which is what stops endless recursion.
    pub(crate) filetype_autocmd_depth: usize,
    /// One entry per active user-function frame, holding the paths that frame's
    /// `writefile(..., 'D')` calls asked to have deleted when it returns.
    ///
    /// This is upstream's `funccall_T.fc_defer` (`eval/userfunc.c` 3469-3484)
    /// narrowed to the only deferred call this port can produce. An empty stack
    /// is `get_current_funccal() == NULL`, which is what `can_add_defer`
    /// (3457-3464) reports as `E193`.
    pub(crate) deferred_ops: Vec<Vec<DeferredOp>>,
    /// The `defer({fn}, ...)` half of the same per-frame list: the calls the
    /// frame's `defer()` registered, fired when the frame ends.

    /// `prevcmd` (`ex_cmds.c` static): the last `:!`/`:range!` command text,
    /// which a later `:!!` or an unescaped `!` in the argument splices back in
    /// (`do_bang`'s `ins_prevcmd` loop, `ex_cmds.c:1140-1175`).
    pub(crate) prev_bang_command: Option<String>,
    /// Preview-command tag stack (`ptag_entry` in `tag.c`).
    pub(crate) preview_tag: Option<crate::tags::TagStackItem>,
    /// Shared lambda registry so `function('<lambda>N')` resolves after the
    /// creating `Evaluator` is gone (`eval.c` `func_ref` / `get_lambda_name`).
    pub(crate) closures: ClosureRegistry,
    /// Edit mode requested by `:startinsert` or `:startreplace`; the host
    /// applies the last request after command execution releases its borrows.
    pub(crate) pending_edit_mode: Option<PendingEditMode>,
    /// Whether an uncaught error was displayed during this execution.
    pub(crate) did_emsg: bool,
    /// The host's live mode machine, installed so `:normal` feeds keys into the
    /// real mode state instead of a throwaway copy. `None` in unit tests that
    /// do not install one; those fall back to a temporary machine.
    pub(crate) mode_machine: Option<Rc<RefCell<ModeMachine>>>,
    /// Upstream `trylevel` (`ex_eval.c`): nonzero while a `:try` block is
    /// active. When zero, errors display but do not abort script execution
    /// (`cause_errthrow` returns false, `should_abort` returns false). When
    /// nonzero, errors become catchable exceptions.
    pub(crate) try_depth: usize,
}

impl<F: FileIO> ExRuntime<F> {
    fn new(io: F) -> Self {
        Self {
            scripts: ScriptCtx::new(io),
            functions: UserFunctions::new(),
            user_commands: Rc::new(RefCell::new(UserCommandRegistry::default())),
            const_vars: BTreeSet::new(),
            channel_ids: ChannelIds::new(),
            jobs: None,
            current_augroup: AugroupId::default(),
            terminal_exit_message: true,
            redirection: None,
            filetype: FiletypeState::default(),
            exiting: false,
            executing: ExecutingCommand::default(),
            active_autocmd: ActiveAutocmdContext::default(),
            autocmd_busy: 0,
            filetype_autocmd_depth: 0,
            deferred_ops: Vec::new(),
            prev_bang_command: None,
            preview_tag: None,
            closures: ClosureRegistry::new(),
            pending_edit_mode: None,
            did_emsg: false,
            try_depth: 0,
            mode_machine: None,
        }
    }

    /// `can_add_defer` (`eval/userfunc.c` 3457-3464): whether a deferred call
    /// has a frame to attach to.
    pub(crate) fn can_add_defer(&self) -> bool {
        !self.deferred_ops.is_empty()
    }

    /// `add_defer` for the `delete` this port can produce.
    /// `add_defer("delete", ...)` for the deletes this port can register.
    pub(crate) fn push_deferred_delete(
        &mut self,
        path: PathBuf,
        mode: crate::fs_builtins::DeleteMode,
    ) {
        if let Some(frame) = self.deferred_ops.last_mut() {
            frame.push(DeferredOp::Delete(path, mode));
        }
    }

    /// `add_defer` for an explicit `defer()` call registration.
    pub(crate) fn push_deferred_call(&mut self, name: String, args: Vec<Typval>) {
        if let Some(frame) = self.deferred_ops.last_mut() {
            frame.push(DeferredOp::Call(name, args));
        }
    }

    pub(crate) fn throwpoint(&self) -> String {
        let function = self.functions.throwpoint_prefix();
        let script = self.scripts.throwpoint_tail();
        if function.is_empty() {
            script
        } else if script == "command line" {
            function
        } else {
            format!("{function}..{script}")
        }
    }

    pub(crate) fn exception(&self, code: &'static str, message: impl Into<String>) -> VimException {
        let mut message = message.into();
        if reports_command_line(code) {
            self.append_command(&mut message);
        }
        self.error(code.to_owned(), message, self.executing.command.clone())
    }
    /// Parse failures report E492 from the raw command line. Other parse
    /// errors append that line to the parser-owned message.
    fn parse_exception(&self, error: &ParseError) -> VimException {
        let message = if error.code == ErrorCode::E492 {
            format!("Not an editor command: {}", self.executing.line)
        } else {
            let mut message = error.message.clone();
            self.append_command(&mut message);
            message
        };
        self.error(
            error.code.as_str().to_owned(),
            message,
            error.command.map(str::to_owned),
        )
    }

    /// A block whose closer never arrived. Upstream detects these after
    /// `do_cmdline`'s loop, where no command is current, so it reports
    /// `Vim:E171: Missing :endif` and not `Vim(if):`. `:function` is the
    /// exception and keeps its name, because it consumes the rest of the input
    /// itself; that one goes through [`Self::exception`].
    fn unterminated_block(&self, code: &'static str, message: impl Into<String>) -> VimException {
        self.error(code.to_owned(), message.into(), None)
    }

    /// `append_command` (`ex_docmd.c:2993-3019`): `": "` and then the command
    /// line, so an error about a range or an unreadable name shows what it was
    /// reading, whitespace and quoting included.
    fn append_command(&self, message: &mut String) {
        message.push_str(": ");
        message.push_str(&self.executing.line);
    }

    fn error(&self, code: String, message: String, command: Option<String>) -> VimException {
        VimException {
            kind: VimExceptionKind::Error(code),
            value: Box::new(Typval::String(OxStr(message.into_bytes()))),
            throwpoint: self.throwpoint(),
            command,
        }
    }
}

/// Stateful Ex execution host.
pub struct ExExecutor<F: FileIO = RealFileIO> {
    runtime: ExRuntime<F>,
    scope: Scope,
    lua: Option<Rc<RefCell<dyn LuaExec>>>,
    last_quit: Option<i64>,
}

impl ExExecutor<RealFileIO> {
    /// Creates an executor backed by the real filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self::with_io(RealFileIO)
    }
}

impl Default for ExExecutor<RealFileIO> {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps already-parsed commands as an executable program.
///
/// One owner, because both `:map` right-hand sides and `feedkeys()`'s
/// pending-mapping path have to build this the same way.
pub(crate) fn program_from_commands(commands: &[ExCommand], line: usize) -> Vec<Instruction> {
    commands
        .iter()
        .cloned()
        .map(|command| {
            let rendered = render_command(&command);
            Instruction {
                source: rendered.clone(),
                raw: rendered.clone(),
                typed: rendered,
                command: Some(command),
                line,
                retried: false,
            }
        })
        .collect()
}

/// `ins_typebuf`'s flags for keys a mapping or `:normal` produced
/// (`input.c:922-1027`).
///
/// `nottyped` is what matters: upstream reports only the bytes past
/// `typebuf.tb_maplen` through `gotchars` (`input.c:2495-2497`), so these keys
/// never reach `may_sync_undo` and everything they do stays in one undo block.
fn mapped_flags(buffer: Option<BufHandle>, remap: bool, modes: MapModes) -> TypeaheadFlags {
    TypeaheadFlags {
        remap: if remap { Remap::Yes } else { Remap::No },
        modes,
        buffer,
        mapped: true,
        silent: false,
    }
}

/// Drains queued input through `machine`, running whatever each consumed key
/// produces before the next one is read.
///
/// This is `exec_normal`'s loop (`ex_docmd.c:7274-7291`) and the main loop's
/// `state_enter` (`state.c:34-106`) in one place, because three callers need
/// it: `:normal`, `feedkeys()`, and the host's input loop. A mapping whose
/// right-hand side is keys is expanded inside [`ModeMachine::check`], but an
/// Ex-command or `<expr>` right-hand side can only leave a *pending action*
/// there — nothing below the host can run a command. Duplicating that
/// handling per caller is what made a mapping execute under `feedkeys()` and
/// do nothing at all under `:normal`.
pub(crate) fn drain_typeahead<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    machine: &Rc<RefCell<ModeMachine>>,
) -> Flow {
    while !access.with_ex_editor(|editor| editor.typeahead().is_empty()) {
        let result = {
            let mut null = crate::indent::NullExprEval;
            let mut eval = crate::indent::IgnoreExprEval::new(&mut null);
            access.with_ex_editor(|editor| machine.borrow_mut().run_once(editor, &mut eval))
        };
        match result {
            Ok(true) => {}
            // Nothing is ready but input is still queued: the front of the
            // queue is the prefix of a longer mapping, and `check` is waiting
            // for a key that cannot arrive here. Time it out.
            Ok(false) => match access
                .with_ex_editor(|editor| machine.borrow_mut().timeout_pending_mapping(editor))
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return error_flow(runtime, "E523", error.to_string()),
            },
            // `emsg(e_recursive_mapping)` then `flush_buffers(FLUSH_MINIMAL)`
            // and `return map_result_fail` (`vgetorpeek`, `input.c:2513-2518`):
            // a message and a discarded queue, not a thrown exception. The
            // oracle confirms it — `:try`/`:catch` around `:normal ,x` with
            // `nmap ,x ,x` catches nothing and the script continues.
            Err(crate::ModeError::RecursiveMapping) => {
                access.with_ex_editor(|editor| {
                    push_text_message(editor, "E223: recursive mapping".to_owned(), true, true);
                });
                access.with_ex_editor(|editor| editor.typeahead_mut().flush());
            }
            Err(crate::ModeError::Vim(code, message)) => {
                return error_flow(runtime, code, message);
            }
            Err(error) => return error_flow(runtime, "E523", error.to_string()),
        }
        if machine.borrow().has_pending_paste_repeat() {
            return Flow::Normal;
        }
        let command = machine.borrow_mut().take_ex_command();
        if let Some(command) = command {
            let logical = vec![LogicalLine {
                text: command,
                first_line: runtime.scripts.current_line().max(1),
            }];
            let program = parse_program(
                &runtime.user_commands,
                access.with_ex_editor(|editor| editor.current_buffer()),
                &logical,
            );
            let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
            if !matches!(flow, Flow::Normal) {
                return flow;
            }
        }
        let mapping = machine.borrow_mut().take_mapping_action();
        if let Some((action, options)) = mapping {
            let flow = run_mapping_action(runtime, access, scope, lua, action, &options);
            if !matches!(flow, Flow::Normal) {
                return flow;
            }
        }
    }
    Flow::Normal
}

/// Runs the mapping right-hand sides [`ModeMachine::check`] cannot.
fn run_mapping_action<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    action: MappingAction,
    options: &MappingOptions,
) -> Flow {
    match action {
        MappingAction::ExCommands { commands, .. } => {
            let program = program_from_commands(&commands, runtime.scripts.current_line().max(1));
            run_program(runtime, access, scope, lua, &program, 0, program.len())
        }
        // `<expr>` (`eval_map_expr`, `mapping.c`): the right-hand side is an
        // expression re-evaluated on every use, and its result *is* the key
        // sequence, inserted under the mapping's own remap flag.
        MappingAction::Expr(expression) => {
            let value = match eval_text(runtime, access, scope, lua, &expression) {
                Ok(value) => value,
                Err(flow) => return flow,
            };
            let keys = match &value {
                Typval::String(value) => Keys::escape_ks(value.as_bytes()),
                _ => Keys::from(typval_to_text(&value).as_str()),
            };
            let flags = mapped_flags(
                access.with_ex_editor(|editor| editor.current_buffer()),
                options.flags.contains(MapFlags::REMAP),
                options.modes,
            );
            match access.with_ex_editor(|editor| editor.typeahead_mut().push(&keys, 0, flags)) {
                Ok(()) => Flow::Normal,
                Err(error) => error_flow(runtime, "E523", error.to_string()),
            }
        }
        MappingAction::Callback(id) => {
            let Some(lua) = lua else {
                return Flow::NotImplemented(format!("mapping callback {id}"));
            };
            let Ok(reference) = usize::try_from(id) else {
                return Flow::NotImplemented(format!("mapping callback {id}"));
            };
            match lua.borrow_mut().invoke_callback(reference, Vec::new()) {
                Ok(_) => Flow::Normal,
                Err(error) => lua_error_flow(runtime, error, "E5107", "E5108"),
            }
        }
        MappingAction::Keys(_) | MappingAction::Nop => Flow::Normal,
    }
}

/// The result of [`ExExecutor::parse_cmdline`]: the first parsed command
/// with its addresses and count resolved exactly as an invocation resolves
/// them.
pub struct ParsedCommandLine {
    /// The first bar-separated command of the line.
    pub command: ExCommand,
    /// First resolved address (`<line1>`).
    pub line1: i64,
    /// Last resolved address (`<line2>`).
    pub line2: i64,
    /// Resolved count, including `-count` defaults.
    pub count: i64,
}

impl<F: FileIO> ExExecutor<F> {
    /// Creates an executor using an injected IO seam.
    #[must_use]
    pub fn with_io(io: F) -> Self {
        Self {
            runtime: ExRuntime::new(io),
            scope: Scope::new(),
            lua: None,
            last_quit: None,
        }
    }

    /// Installs the Lua host used by `:lua`, `:luafile`, and `:luado`.
    pub fn set_lua_exec(&mut self, lua: Rc<RefCell<dyn LuaExec>>) {
        self.lua = Some(lua);
    }

    /// Process exit requested since the last poll (`:cquit` / `:qall`).
    pub fn take_quit(&mut self) -> Option<i64> {
        self.last_quit.take()
    }
    /// Shares one user-command registry with `other`, so a nested executor
    /// sees every definition the primary one carries and vice versa.
    pub fn share_user_commands_from<G: FileIO>(&mut self, other: &ExExecutor<G>) {
        self.runtime.user_commands = Rc::clone(&other.runtime.user_commands);
    }

    /// Shares durable user-function definitions without sharing call frames.
    pub fn share_user_functions_from<G: FileIO>(&mut self, other: &ExExecutor<G>) {
        self.runtime
            .functions
            .share_definitions_from(&other.runtime.functions);
    }

    /// Copies runtime search roots from `other` without sharing mutable
    /// sourcing state.
    pub fn share_runtime_roots_from<G: FileIO>(&mut self, other: &ExExecutor<G>) {
        self.runtime
            .scripts
            .share_runtime_roots_from(&other.runtime.scripts);
    }

    /// Defines one user command in the global or a buffer-local table
    /// (`nvim_create_user_command` / `nvim_buf_create_user_command`).
    ///
    /// `force` replaces an existing definition, `:command!`'s semantics.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Editor`] for an invalid `-nargs`, an
    /// uncapitalized name, or an unknown buffer, and
    /// [`ExecError::DuplicateCommand`] when a different definition exists
    /// and `force` is false.
    pub fn define_user_command(
        &mut self,
        editor: &mut Editor,
        buffer: Option<BufHandle>,
        command: UserCommand,
        force: bool,
    ) -> Result<(), ExecError> {
        // `-nargs` characters plus the API's `_` form (`EXTRA|NOSPC|NEEDARG`
        // like `1`).
        if !matches!(command.nargs, '0' | '1' | '?' | '+' | '*' | '_') {
            return Err(ExecError::Editor(format!(
                "E176: Invalid number of arguments: {}",
                command.nargs
            )));
        }
        // An accepted range defaults its domain to lines, like `-range`.
        let mut command = command;
        if command.accepts_range && command.addr_type == AddrType::None {
            command.addr_type = AddrType::Lines;
        }
        if !valid_user_command_name(&command.name) {
            return Err(ExecError::Editor(
                "E183: User defined commands must be capitalized".to_owned(),
            ));
        }
        if let Some(buffer) = buffer
            && editor.buffer(buffer).is_err()
        {
            return Err(ExecError::Editor(format!(
                "Invalid buffer id: {}",
                i64::from(buffer)
            )));
        }
        let mut registry = self.runtime.user_commands.borrow_mut();
        let scope = registry.scope_mut(buffer);
        if let Some(existing) = scope.get(&command.name) {
            // The same silent-reload rule `:command` applies (`usercmd.c:940-948`).
            let same_script_reload = existing.script_context.sid == command.script_context.sid
                && existing.script_context.seq != command.script_context.seq;
            if !force && !same_script_reload {
                // Typed distinctly so the API maps only duplicates to
                // its "command already exists" error.
                return Err(ExecError::DuplicateCommand {
                    name: command.name.clone(),
                });
            }
        }
        scope.insert(command.name.clone(), command);
        Ok(())
    }

    /// Deletes one user command from the global or a buffer-local table
    /// (`nvim_del_user_command` / `nvim_buf_del_user_command`).
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Editor`] for an unknown buffer or when no
    /// user command with `name` exists in the target scope.
    pub fn delete_user_command(
        &mut self,
        editor: &mut Editor,
        buffer: Option<BufHandle>,
        name: &str,
    ) -> Result<(), ExecError> {
        if let Some(buffer) = buffer
            && editor.buffer(buffer).is_err()
        {
            return Err(ExecError::Editor(format!(
                "Invalid buffer id: {}",
                i64::from(buffer)
            )));
        }
        let removed = {
            let mut registry = self.runtime.user_commands.borrow_mut();
            registry.scope_mut(buffer).remove(name).is_some()
        };
        if !removed {
            return Err(ExecError::Editor(format!(
                "E184: No such user-defined command: {name}"
            )));
        }
        Ok(())
    }

    /// Lists one exact scope's user commands (`nvim_get_commands` with
    /// `builtin = false`): `None` is the global table, `Some(buffer)` that
    /// buffer's local table. The merged current-buffer-first *lookup* lives
    /// in [`UserCommandLookup`], not here.
    #[must_use]
    pub fn list_user_commands(&self, buffer: Option<BufHandle>) -> Vec<UserCommand> {
        let registry = self.runtime.user_commands.borrow();
        let scope = match buffer {
            Some(buffer) => registry.buffer_commands.get(&buffer),
            None => Some(&registry.commands),
        };
        scope
            .map(|table| table.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Parses all commands in one line with the current buffer's user-command
    /// table. This is the execution path used by `nvim_command` and
    /// `nvim_exec2`.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Parse`] when the command line does not parse.
    pub fn parse_commands(&self, editor: &Editor, line: &str) -> Result<Vec<ExCommand>, ExecError> {
        let registry = self.runtime.user_commands.borrow();
        let provider = UserCommandLookup {
            registry: &registry,
            buffer: editor.current_buffer(),
        };
        ExParser::with_user_commands(&provider)
            .parse(line)
            .map_err(ExecError::Parse)
    }

    /// Parses one command line without running it (`nvim_parse_cmd`),
    /// resolving its addresses and count exactly like an invocation would.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Parse`] for a parse failure and
    /// [`ExecError::Editor`] for an empty line or a user command that no
    /// longer resolves.
    pub fn parse_cmdline(
        &mut self,
        editor: &mut Editor,
        line: &str,
    ) -> Result<ParsedCommandLine, ExecError> {
        let buffer = editor.current_buffer();
        let Some(command) = self.parse_commands(editor, line)?.into_iter().next() else {
            return Err(ExecError::Editor("Parsing command-line".to_owned()));
        };
        let (line1, line2, count) = if let ResolvedCommand::User(info) = &command.command {
            let definition = {
                let registry = self.runtime.user_commands.borrow();
                let provider = UserCommandLookup {
                    registry: &registry,
                    buffer,
                };
                provider.get(&info.name).cloned()
            };
            let Some(definition) = definition else {
                return Err(ExecError::Editor(format!(
                    "E492: Not an editor command: {}",
                    info.name
                )));
            };
            user_command_addresses(editor, &command, &definition)
        } else {
            let (line1, line2) =
                resolve_range(editor, &command).unwrap_or_else(|_| current_line_pair(editor));
            let count = command
                .count
                .and_then(|value| i64::try_from(value).ok())
                .unwrap_or(0);
            (line1, line2, count)
        };
        Ok(ParsedCommandLine {
            line1: i64::try_from(line1).unwrap_or(i64::MAX),
            line2: i64::try_from(line2).unwrap_or(i64::MAX),
            count,
            command,
        })
    }

    /// Drops one buffer's local user commands — the wipe path. `:bunload`
    /// and `:bdelete` do not call this.
    pub fn remove_buffer(&mut self, buffer: BufHandle) {
        self.runtime
            .user_commands
            .borrow_mut()
            .remove_buffer(buffer);
    }

    /// Share the host editor's dynamic channel key space with jobs.
    pub fn set_channel_ids(&mut self, channel_ids: ChannelIds) {
        self.runtime.channel_ids = channel_ids;
    }

    /// Installs the host's live mode machine so `:normal` feeds keys into the
    /// real mode state and mode changes persist after the command returns.
    pub fn set_mode_machine(&mut self, machine: Rc<RefCell<ModeMachine>>) {
        self.runtime.mode_machine = Some(machine);
    }

    /// Write bytes to a job channel's standard input or PTY master.
    ///
    /// Used by the RPC host's job sink so `nvim_chan_send` can reach children
    /// spawned by `jobstart`.
    ///
    /// # Errors
    ///
    /// Returns the job manager's write failure text; `Ok(false)` means the
    /// channel does not exist.
    pub fn job_send(&mut self, channel: u64, data: &[u8]) -> Result<bool, String> {
        if self.runtime.jobs.is_none() {
            self.runtime.jobs = JobManager::new().ok();
        }
        let Some(manager) = self.runtime.jobs.as_mut() else {
            return Ok(false);
        };
        manager.send(channel, data.to_vec())
    }

    /// Poll after a channel write and return any PTY output ready for the terminal buffer.
    ///
    /// # Errors
    ///
    /// Returns the job manager's poll failure text.
    pub fn take_pty_output(&mut self, channel: u64) -> Result<Vec<u8>, String> {
        if self.runtime.jobs.is_none() {
            return Ok(Vec::new());
        }
        let Some(manager) = self.runtime.jobs.as_mut() else {
            return Ok(Vec::new());
        };
        let events = manager.poll()?;
        manager.defer_events(events);
        Ok(manager.take_pty_output(channel).unwrap_or_default())
    }

    /// Poll jobs and append ready PTY output to their terminal buffers.
    ///
    /// # Errors
    ///
    /// Returns the job manager's poll or editor update failure.
    pub fn flush_pty_output<E: ExEditorAccess>(&mut self, access: &E) -> Result<bool, String> {
        let Some(manager) = self.runtime.jobs.as_mut() else {
            return Ok(false);
        };
        let output = manager.take_all_pty_output()?;
        let changed = !output.is_empty();
        for (channel, bytes) in output {
            access
                .with_ex_editor(|editor| editor.append_terminal_buffer(channel, &bytes))
                .map_err(|error| error.to_string())?;
        }
        Ok(changed)
    }

    /// Script/SID/runtime-root state.
    #[must_use]
    pub fn scripts(&self) -> &ScriptCtx<F> {
        &self.runtime.scripts
    }

    /// Mutable script state, used to install runtime roots.
    pub fn scripts_mut(&mut self) -> &mut ScriptCtx<F> {
        &mut self.runtime.scripts
    }

    /// User-function table and call stack.
    #[must_use]
    pub fn functions(&self) -> &UserFunctions {
        &self.runtime.functions
    }

    /// Persistent Vimscript scope (notably `g:`); `$` reads go to the live
    /// process environment.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Call any builtin through this executor's persistent runtime, using the
    /// same dispatch a Vimscript expression gets.
    ///
    /// This is the entry point the Lua `vim.fn`/`vim.call` bridge comes in
    /// through, so it has to answer exactly what Vimscript answers: the
    /// editor-stateful families, user functions, and the regex-backed
    /// typval-only builtins. It used to forward straight into the job-builtin
    /// dispatcher, which served five names and panicked on everything else.
    ///
    /// # Errors
    ///
    /// Returns the builtin's evaluation error, or the scope sync failure as
    /// [`ExecError::Editor`]/[`ExecError::Eval`].
    #[expect(
        clippy::needless_pass_by_value,
        reason = "public host API: callers move their argument vector in, mirroring the evaluator's owned-builtin contract"
    )]
    pub fn call_builtin<E: ExEditorAccess>(
        &mut self,
        access: &E,
        name: &OxStr,
        args: Vec<Typval>,
    ) -> Result<Typval, ExecError> {
        access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut self.scope))?;
        self.runtime.try_depth += 1;
        let lua = self.lua.clone();
        let result = call_builtin_dispatch(
            &mut self.runtime,
            access,
            &mut self.scope,
            lua.as_ref(),
            name,
            &args,
        );
        self.runtime.try_depth -= 1;
        access.with_ex_editor(|editor| sync_scope_into_editor(editor, &self.scope))?;
        result
    }

    /// Evaluates one Vimscript expression in this executor's persistent scope.
    ///
    /// # Errors
    ///
    /// Returns the evaluation failure: [`ExecError::Vim`] for a raised
    /// exception, [`ExecError::NotImplemented`] for an unimplemented
    /// builtin, or the scope sync failure.
    pub fn evaluate_expression<E: ExEditorAccess>(
        &mut self,
        access: &E,
        expression: &str,
    ) -> Result<Typval, ExecError> {
        access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut self.scope))?;
        self.runtime.try_depth += 1;
        let lua = self.lua.clone();
        let result = eval_text(
            &mut self.runtime,
            access,
            &mut self.scope,
            lua.as_ref(),
            expression,
        );
        self.runtime.try_depth -= 1;
        access.with_ex_editor(|editor| sync_scope_into_editor(editor, &self.scope))?;
        match result {
            Ok(value) => Ok(value),
            Err(Flow::Exception(exception)) => Err(ExecError::Vim(exception)),
            Err(Flow::NotImplemented(name)) => Err(ExecError::NotImplemented(name)),
            // `eval_text` cannot produce the remaining flows here; report the
            // invariant break as E605 instead of panicking inside a host.
            Err(flow) => Err(ExecError::Editor(format!(
                "expression evaluation returned control flow: {flow:?}"
            ))),
        }
    }

    /// Changes the process working directory and retains it for `:cd -`.
    ///
    /// # Errors
    ///
    /// Returns the directory transition error.
    pub fn change_directory<E: ExEditorAccess>(
        &mut self,
        access: &E,
        path: &str,
    ) -> Result<(), ExecError> {
        access
            .with_ex_editor(|editor| {
                crate::excmd_exec::change_directory(editor, path, DirectoryScope::Global)
            })
            .map(|_| ())
            .map_err(ExecError::Eval)
    }

    /// Executes one command line, including bar-separated commands.
    ///
    /// Startup call sites (`-c`, `-S`) use [`Self::execute_line_core`] directly
    /// to keep `try_depth == 0` — upstream's `trylevel` stays 0 for `-c`/`-S`
    /// so depth-0 errors display and continue rather than unwinding the
    /// startup script. API callers use this wrapper which increments
    /// `try_depth` so errors propagate as exceptions.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Vim`] for an uncaught exception,
    /// [`ExecError::Editor`] for stray `:break`/`:continue`/`:return`, or
    /// the scope sync failure.
    pub fn execute_line<E: ExEditorAccess>(
        &mut self,
        access: &E,
        line: &str,
    ) -> Result<ExecOutcome, ExecError> {
        self.runtime.try_depth += 1;
        let result = self.execute_line_core(access, line);
        self.runtime.try_depth -= 1;
        result
    }

    /// Core runner without `try_depth` increment — for startup paths
    /// (`-c`, `-S`) where upstream keeps `trylevel == 0`.
    ///
    /// # Errors
    ///
    /// Returns the same failures [`ExExecutor::execute_line`] reports.
    pub fn execute_line_core<E: ExEditorAccess>(
        &mut self,
        access: &E,
        line: &str,
    ) -> Result<ExecOutcome, ExecError> {
        let logical = if line.contains('\n') {
            self.runtime
                .scripts
                .join_logical_lines(line)
                .map_err(|error| {
                    ExecError::Vim(self.runtime.exception(error.code, error.message))
                })?
        } else {
            vec![LogicalLine {
                text: line.to_owned(),
                first_line: 1,
            }]
        };
        let program = parse_program(
            &self.runtime.user_commands,
            access.with_ex_editor(|editor| editor.current_buffer()),
            &logical,
        );
        access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut self.scope))?;
        let flow = run_program(
            &mut self.runtime,
            access,
            &mut self.scope,
            self.lua.as_ref(),
            &program,
            0,
            program.len(),
        );
        self.finish_quit(access, &flow);
        access.with_ex_editor(|editor| sync_scope_into_editor(editor, &self.scope))?;
        flow_to_result(flow)
    }

    /// Executes one API-planned Ex autocmd with `<amatch>`, `<afile>`, and
    /// `<abuf>` bound for the duration of the command.
    ///
    /// # Errors
    ///
    /// Returns the same failures [`ExExecutor::execute_line`] reports.
    pub fn execute_autocmd_command<E: ExEditorAccess>(
        &mut self,
        access: &E,
        action: &crate::AutocmdAction,
        source: &str,
    ) -> Result<ExecOutcome, ExecError> {
        self.with_autocmd_context(action, |executor| executor.execute_line(access, source))
    }

    /// Invokes one API-planned named Vimscript autocmd callback with no arguments.
    ///
    /// # Errors
    ///
    /// Returns the callback's uncaught exception as [`ExecError::Vim`], or
    /// the scope sync failure.
    pub fn execute_autocmd_function<E: ExEditorAccess>(
        &mut self,
        access: &E,
        action: &crate::AutocmdAction,
        name: &str,
    ) -> Result<ExecOutcome, ExecError> {
        self.with_autocmd_context(action, |executor| {
            access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut executor.scope))?;
            let (first, last) = access.with_ex_editor(|editor| current_line_pair(editor));
            let flow = match call_user_function(
                &mut executor.runtime,
                access,
                &mut executor.scope,
                executor.lua.as_ref(),
                name,
                Vec::new(),
                first,
                last,
            ) {
                Ok(_) => Flow::Normal,
                Err(flow) => flow,
            };
            executor.finish_quit(access, &flow);
            access.with_ex_editor(|editor| sync_scope_into_editor(editor, &executor.scope))?;
            flow_to_result(flow)
        })
    }

    fn with_autocmd_context<R>(
        &mut self,
        action: &crate::AutocmdAction,
        execute: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let previous_context = std::mem::replace(
            &mut self.runtime.active_autocmd,
            ActiveAutocmdContext {
                matched: action.match_name.clone(),
                file: action.file_name.clone(),
                buffer: action.buffer,
                nested: action.nested,
            },
        );
        // The API host runs one action per call; count it as busy so a
        // `filetype` write inside the handler obeys the handler's `++nested`
        // flag, the same as a native plan.
        self.runtime.autocmd_busy += 1;
        let result = execute(self);
        self.runtime.autocmd_busy -= 1;
        self.runtime.active_autocmd = previous_context;
        result
    }

    /// Executes an already parsed command stream against `editor`.
    ///
    /// API callers use this wrapper which increments `try_depth` so errors
    /// propagate as exceptions. Startup paths that need `trylevel == 0`
    /// should call [`Self::execute_commands_core`] directly.
    ///
    /// # Errors
    ///
    /// Returns the same failures [`ExExecutor::execute_line`] reports.
    pub fn execute_commands<E: ExEditorAccess>(
        &mut self,
        access: &E,
        commands: &[ExCommand],
    ) -> Result<ExecOutcome, ExecError> {
        self.runtime.try_depth += 1;
        let result = self.execute_commands_core(access, commands);
        self.runtime.try_depth -= 1;
        result
    }

    /// Core runner without `try_depth` increment — for startup paths
    /// where upstream keeps `trylevel == 0`.
    ///
    /// # Errors
    ///
    /// Returns the same failures [`ExExecutor::execute_line`] reports.
    pub fn execute_commands_core<E: ExEditorAccess>(
        &mut self,
        access: &E,
        commands: &[ExCommand],
    ) -> Result<ExecOutcome, ExecError> {
        let program = program_from_commands(commands, self.runtime.scripts.current_line().max(1));
        access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut self.scope))?;
        let flow = run_program(
            &mut self.runtime,
            access,
            &mut self.scope,
            self.lua.as_ref(),
            &program,
            0,
            program.len(),
        );
        self.finish_quit(access, &flow);
        access.with_ex_editor(|editor| sync_scope_into_editor(editor, &self.scope))?;
        flow_to_result(flow)
    }

    /// Takes the last edit-mode transition requested by an Ex command.
    #[must_use]
    pub fn take_pending_edit_mode(&mut self) -> Option<PendingEditMode> {
        self.runtime.pending_edit_mode.take()
    }

    /// Reports whether execution displayed an uncaught error.
    #[must_use]
    pub const fn did_emsg(&self) -> bool {
        self.runtime.did_emsg
    }

    /// Consumes queued input through `machine` until nothing is left, running
    /// finished command lines and mapping right-hand sides as they appear.
    ///
    /// The host's input loop, `:normal` and `feedkeys()` all reach the same
    /// [`drain_typeahead`] through this, so a mapping behaves the same however
    /// its left-hand side arrived.
    ///
    /// # Errors
    ///
    /// Returns the same failures [`ExExecutor::execute_line`] reports.
    pub fn run_typeahead<E: ExEditorAccess>(
        &mut self,
        access: &E,
        machine: &Rc<RefCell<ModeMachine>>,
    ) -> Result<ExecOutcome, ExecError> {
        access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut self.scope))?;
        let before = access.with_ex_editor(|editor| {
            let buffer = editor.current_buffer();
            (
                buffer,
                buffer.is_some_and(|buffer| editor.is_terminal_buffer(buffer)),
                matches!(machine.borrow().mode(), Mode::Insert(_) | Mode::Replace(_)),
            )
        });
        self.runtime.try_depth += 1;
        let mut flow = drain_typeahead(
            &mut self.runtime,
            access,
            &mut self.scope,
            self.lua.as_ref(),
            machine,
        );
        let after = access.with_ex_editor(|editor| {
            let buffer = editor.current_buffer();
            (
                buffer,
                buffer.is_some_and(|buffer| editor.is_terminal_buffer(buffer)),
            )
        });
        if matches!(flow, Flow::Normal) && before.2 && !before.1 && after.1 && before.0 != after.0 {
            if let Some(buffer) = before.0 {
                flow = fire_buffer_lifecycle(
                    &mut self.runtime,
                    access,
                    &mut self.scope,
                    self.lua.as_ref(),
                    &[Event::InsertLeave],
                    buffer,
                );
            }
            if matches!(flow, Flow::Normal)
                && let Some(buffer) = after.0
            {
                flow = fire_buffer_lifecycle(
                    &mut self.runtime,
                    access,
                    &mut self.scope,
                    self.lua.as_ref(),
                    &[Event::TermEnter],
                    buffer,
                );
            }
        }
        self.runtime.try_depth -= 1;
        if let Some(pending) = self.runtime.pending_edit_mode.take() {
            match pending {
                PendingEditMode::Insert => machine.borrow_mut().enter_insert(),
                PendingEditMode::Append => access
                    .with_ex_editor(|editor| machine.borrow_mut().enter_append(editor))
                    .map_err(|error| ExecError::Editor(error.to_string()))?,
                PendingEditMode::Replace => machine.borrow_mut().enter_replace(),
                PendingEditMode::StopInsert => machine.borrow_mut().stop_insert(),
            }
        }
        self.finish_quit(access, &flow);
        access.with_ex_editor(|editor| sync_scope_into_editor(editor, &self.scope))?;
        flow_to_result(flow)
    }

    /// Executes source text with a fresh stable SID and isolated `s:` scope.
    ///
    /// API callers use this wrapper which increments `try_depth` so errors
    /// propagate as exceptions. Startup paths (`-S`) that need `trylevel == 0`
    /// should call [`Self::execute_script_core`] directly.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Vim`] for a script-line parse failure and the
    /// execution or scope sync failures [`ExExecutor::execute_line`]
    /// reports.
    pub fn execute_script<E: ExEditorAccess>(
        &mut self,
        access: &E,
        source_name: &str,
        text: &str,
    ) -> Result<ExecOutcome, ExecError> {
        self.runtime.try_depth += 1;
        let result = self.execute_script_core(access, source_name, text);
        self.runtime.try_depth -= 1;
        result
    }

    /// Core runner without `try_depth` increment — for startup paths
    /// (`-S`) where upstream keeps `trylevel == 0`.
    ///
    /// # Errors
    ///
    /// Returns the same failures [`ExExecutor::execute_script`] reports.
    pub fn execute_script_core<E: ExEditorAccess>(
        &mut self,
        access: &E,
        source_name: &str,
        text: &str,
    ) -> Result<ExecOutcome, ExecError> {
        let lines = self
            .runtime
            .scripts
            .join_logical_lines(text)
            .map_err(|error| ExecError::Vim(self.runtime.exception(error.code, error.message)))?;
        let caller_script = self.scope.script.clone();
        let caller_augroup = self.runtime.current_augroup;
        let sid = self.runtime.scripts.push_source(source_name.to_owned());
        let lines = expand_script_lines(&self.runtime.scripts, lines, sid);
        self.runtime.scripts.load_script_scope(sid, &mut self.scope);
        let program = parse_program(
            &self.runtime.user_commands,
            access.with_ex_editor(|editor| editor.current_buffer()),
            &lines,
        );
        let result =
            match access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut self.scope)) {
                Ok(()) => {
                    let flow = run_program(
                        &mut self.runtime,
                        access,
                        &mut self.scope,
                        self.lua.as_ref(),
                        &program,
                        0,
                        program.len(),
                    );
                    self.finish_quit(access, &flow);
                    access
                        .with_ex_editor(|editor| sync_scope_into_editor(editor, &self.scope))
                        .and_then(|()| flow_to_result(flow))
                }
                Err(error) => Err(error),
            };
        self.runtime.scripts.store_script_scope(sid, &self.scope);
        self.scope.script = caller_script;
        self.runtime.current_augroup = caller_augroup;
        result
    }

    /// Sources a file through [`FileIO`]. Plain `:source` executes each time.
    ///
    /// Sourcing preserves the caller's current `try_depth`.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Io`] when the file cannot be read,
    /// [`ExecError::Vim`] for a script-line parse failure, and the
    /// execution failures [`ExExecutor::execute_line`] reports.
    pub fn source_file<E: ExEditorAccess>(
        &mut self,
        access: &E,
        path: &Path,
    ) -> Result<ExecOutcome, ExecError> {
        let flow = source_path(
            &mut self.runtime,
            access,
            &mut self.scope,
            self.lua.as_ref(),
            path,
            false,
        )?;
        self.finish_quit(access, &flow);
        flow_to_result(flow)
    }

    /// `getout` (`main.c`:753): the exit sequence, run when a flow ends the
    /// process. It happens before the scope is synced back, so a `VimLeave`
    /// handler sees the state the quitting command left behind.
    fn finish_quit<E: ExEditorAccess>(&mut self, access: &E, flow: &Flow) {
        if !matches!(flow, Flow::Quit(_)) {
            return;
        }
        if let Flow::Quit(code) = *flow {
            self.last_quit = Some(code);
        }
        let lua = self.lua.clone();
        fire_exit_autocmds(&mut self.runtime, access, &mut self.scope, lua.as_ref());
    }

    /// Runs `getout`'s autocommands for an exit the host decided on rather
    /// than a command: the Ex loop reaching the end of its input, which
    /// `main.c` also finishes through `getout(0)`. Idempotent, so a host that
    /// calls it after a `:quit` has already exited fires nothing twice.
    ///
    /// # Errors
    ///
    /// Returns the scope sync failure as [`ExecError::Editor`].
    pub fn run_exit_sequence<E: ExEditorAccess>(&mut self, access: &E) -> Result<(), ExecError> {
        access.with_ex_editor(|editor| sync_editor_into_scope(editor, &mut self.scope))?;
        let lua = self.lua.clone();
        fire_exit_autocmds(&mut self.runtime, access, &mut self.scope, lua.as_ref());
        access.with_ex_editor(|editor| sync_scope_into_editor(editor, &self.scope))
    }
}

#[derive(Clone)]
pub(crate) struct Instruction {
    /// `None` marks a slot stored deferred: it re-parses against the live
    /// user-command view when executed, and a slot `run_deferred_line` has
    /// already deferred once surfaces its re-parse failure instead
    /// (`retried`).
    command: Option<ExCommand>,
    source: String,
    /// The command exactly as typed, from where `do_one_cmd` started reading
    /// it to its span end — modifiers and range included. A user command
    /// re-parses this when its live metadata diverges from the metadata this
    /// program was parsed with.
    typed: String,
    /// The command line as written, from where `do_one_cmd` started reading it
    /// to the end of the line — upstream's `*ea.cmdlinep`, which
    /// `append_command` echoes verbatim, leading whitespace included.
    ///
    /// Distinct from `source`, which is a re-render used to rebuild a
    /// `:function` body and therefore normalizes whitespace away.
    raw: String,
    line: usize,
    /// Set on a slot `run_deferred_line` already deferred once: its next
    /// parse failure is final instead of deferring again, so a line that
    /// never resolves raises instead of recursing.
    retried: bool,
}

impl Instruction {
    /// The parsed command, when this slot carries one; `None` routes the
    /// slot through the deferred re-parse path.
    fn command(&self) -> Option<&ExCommand> {
        self.command.as_ref()
    }

    fn name(&self) -> &str {
        self.command
            .as_ref()
            .map_or("", |command| command.command.name())
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Flow {
    Normal,
    Break,
    Continue,
    Return(Typval),
    Finish,
    Quit(i64),
    Exception(VimException),
    NotImplemented(String),
}

fn flow_to_result(flow: Flow) -> Result<ExecOutcome, ExecError> {
    match flow {
        Flow::Normal => Ok(ExecOutcome::Completed),
        Flow::Finish => Ok(ExecOutcome::Finished),

        Flow::Quit(code) => Ok(ExecOutcome::Quit(code)),
        Flow::Exception(exception) => Err(ExecError::Vim(exception)),
        Flow::NotImplemented(name) => Err(ExecError::NotImplemented(name)),
        Flow::Break => Err(ExecError::Editor(
            "E587: :break without :while or :for".to_owned(),
        )),
        Flow::Continue => Err(ExecError::Editor(
            "E586: :continue without :while or :for".to_owned(),
        )),
        Flow::Return(_) => Err(ExecError::Editor(
            "E133: :return not inside a function".to_owned(),
        )),
    }
}

/// Upstream `emsg()` displays the error before `cause_errthrow` decides
/// whether to throw. At `trylevel == 0` (no active `:try` block) the error
/// is displayed via `emsg` and `did_emsg` is set; the API's `TRY_WRAP`
/// (`helpers.c:159-165`) then captures it via `msg_list` in `try_leave`.
/// This mirrors that display: when the flow escapes all try blocks
/// (`try_depth == 0`), push the error message to the editor's message list
/// so `v:errmsg` and `last_error_message` reflect it, then the caller
/// receives the flow as `Err`.
fn display_error_message<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    flow: &Flow,
) -> bool {
    let Some(message) = swallow_error_message(runtime, flow) else {
        return false;
    };
    runtime.did_emsg = true;
    access.with_ex_editor(|editor| {
        push_text_message(editor, message, true, true);
    });
    true
}
fn expand_script_lines<F: FileIO>(
    scripts: &ScriptCtx<F>,
    lines: Vec<LogicalLine>,
    sid: Sid,
) -> Vec<LogicalLine> {
    lines
        .into_iter()
        .map(|line| LogicalLine {
            text: scripts.expand_snr(&line.text, sid),
            first_line: line.first_line,
        })
        .collect()
}

/// Parses a program with the user-command view of `buffer` (the current
/// buffer's local table first, then the global one).
pub(crate) fn parse_program(
    users: &Rc<RefCell<UserCommandRegistry>>,
    buffer: Option<BufHandle>,
    logical: &[LogicalLine],
) -> Vec<Instruction> {
    let registry = users.borrow();
    let provider = UserCommandLookup {
        registry: &registry,
        buffer,
    };
    let parser = ExParser::with_user_commands(&provider);
    let mut program = Vec::new();
    for line in logical {
        let (command_text, heredoc_body) = line
            .text
            .split_once('\n')
            .map_or((line.text.as_str(), None), |(command, body)| {
                (command, Some(body))
            });
        let commands = match parser.parse(command_text) {
            Ok(commands) => commands,
            // A line that does not parse is stored deferred: the slot keeps
            // no command and `run_instructions` re-resolves it against the
            // live user-command view when its turn comes.
            Err(_) => {
                if let Some(commands) = parse_put_expression(&parser, command_text) {
                    commands
                } else {
                    program.push(Instruction {
                        command: None,
                        source: command_text.to_owned(),
                        typed: command_text.to_owned(),
                        raw: command_text.to_owned(),
                        line: line.first_line,
                        retried: false,
                    });
                    continue;
                }
            }
        };
        // `*ea.cmdlinep` is where `do_one_cmd` started reading: the whole line
        // for the first command, and the text just past the `|` for each one
        // after it. `append_command` echoes from there to the end of the line,
        // so the indentation of `  99print` inside a `:try` shows up in the
        // message exactly as written.
        let mut read_from = 0;
        for mut command in commands {
            let raw = command_text[read_from..].to_owned();
            read_from = command.span.end.min(command_text.len());
            if command_text.as_bytes().get(read_from) == Some(&b'|') {
                read_from += 1;
            }
            let typed = command_text[command.span.start..command.span.end.min(command_text.len())]
                .to_owned();
            if let Some(body) = heredoc_body {
                command.args.push('\n');
                command.args.push_str(body);
            }
            program.push(Instruction {
                source: render_command(&command),
                command: Some(command),
                typed,
                raw,
                line: line.first_line,
                retried: false,
            });
        }
    }
    program
}

fn parse_put_expression<P: UserCommandProvider + ?Sized>(
    parser: &ExParser<'_, P>,
    line: &str,
) -> Option<Vec<ExCommand>> {
    for (offset, _) in line.match_indices('=') {
        let Ok(mut commands) = parser.parse(&line[..=offset]) else {
            continue;
        };
        if commands.len() != 1
            || commands[0].command.name() != "put"
            || commands[0].register != Some('=')
        {
            continue;
        }
        let expression = line[offset + 1..].trim();
        if expression.is_empty() {
            return None;
        }
        expression.clone_into(&mut commands[0].args);
        commands[0].span.end = line.len();
        return Some(commands);
    }
    None
}

/// Upstream `cause_errthrow` (`ex_eval.c:189`): when `trylevel == 0` (no
/// active `:try` block) and the error is not an explicit `:throw`, the error
/// displays via `emsg` but does not abort script execution. Returns the
/// error message to display if the flow should be swallowed, or `None` if it
/// should propagate.
fn swallow_error_message(runtime: &ExRuntime<impl FileIO>, flow: &Flow) -> Option<String> {
    if runtime.try_depth != 0 {
        return None;
    }
    match flow {
        Flow::Exception(exception) => {
            if matches!(exception.kind, VimExceptionKind::Error(_)) {
                Some(exception.message())
            } else {
                None
            }
        }
        Flow::NotImplemented(name) => Some(format!("E117: Unknown function: {name}")),
        _ => None,
    }
}

pub(crate) fn run_program<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    program: &[Instruction],
    start: usize,
    end: usize,
) -> Flow {
    let outer = std::mem::take(&mut runtime.executing);
    let flow = run_instructions(runtime, access, scope, lua, program, start, end);
    runtime.executing = outer;
    flow
}

/// Upstream `BREAKCHECK_SKIP` (`vim.h`): loop back edges between break
/// checks. One Ex loop back edge in this many services one non-blocking
/// event turn through [`LuaExec::run_event_turn`].
const BREAKCHECK_SKIP: usize = 1000;

/// `do_cmdline`'s loop over one program (`ex_docmd.c:321-...`).
///
/// Split from [`run_program`] only so the `executing` command state, which
/// every `error_flow` below reads to build `Vim({cmdname}):`, is saved and
/// restored on *every* exit path rather than at two dozen `return`s.
#[expect(
    clippy::too_many_lines,
    reason = "the instruction interpreter preserves Vim's ordered control-flow and exception state"
)]
fn run_instructions<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    program: &[Instruction],
    start: usize,
    end: usize,
) -> Flow {
    let mut pc = start;
    while pc < end {
        let instruction = &program[pc];
        runtime.scripts.set_current_line(instruction.line);
        runtime.functions.set_current_line(instruction.line);
        // `do_errthrow(cstack, cmdidx != CMD_SIZE && !IS_USER_CMDIDX ? name : NULL)`
        // (`ex_docmd.c:2385-2387`): the canonical builtin name, or nothing for
        // a user command and for a line that did not parse.
        runtime.executing = ExecutingCommand {
            command: instruction
                .command
                .as_ref()
                .and_then(|command| match &command.command {
                    ResolvedCommand::Builtin(spec) => Some(spec.name.to_owned()),
                    ResolvedCommand::User(_) | ResolvedCommand::RangeOnly => None,
                }),
            line: instruction.raw.clone(),
        };
        let Some(command) = instruction.command() else {
            if instruction.retried {
                // This slot already deferred once and still does not resolve:
                // one final attempt, then the failure surfaces instead of
                // deferring forever (`do_one_cmd` raises what it cannot parse).
                let attempt = {
                    let registry = runtime.user_commands.borrow();
                    let provider = UserCommandLookup {
                        registry: &registry,
                        buffer: access.with_ex_editor(|editor| editor.current_buffer()),
                    };
                    ExParser::with_user_commands(&provider).parse_first(&instruction.source)
                };
                match attempt {
                    Ok(Some(_)) => {
                        let flow = run_deferred_line(runtime, access, scope, lua, instruction);
                        if !matches!(flow, Flow::Normal)
                            && !display_error_message(runtime, access, &flow)
                        {
                            return flow;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let flow = exec_error_flow(runtime, ExecError::Parse(error));
                        if !display_error_message(runtime, access, &flow) {
                            return flow;
                        }
                    }
                }
            } else {
                // `find_ex_command` runs when a line is *executed*, not when the
                // enclosing script or function body was read (`do_one_cmd`): the
                // line re-parses against the live user-command view, and because
                // `do_one_cmd` runs one command at a time, an earlier command on
                // the line — `:buffer` — may change what the later ones resolve
                // to. `run_deferred_line` reproduces that per-command cadence
                // while keeping bar-split structural blocks on one program.
                let flow = run_deferred_line(runtime, access, scope, lua, instruction);
                if !matches!(flow, Flow::Normal) && !display_error_message(runtime, access, &flow) {
                    return flow;
                }
            }
            pc += 1;
            continue;
        };
        // A user command resolves at execution too: if the live view — the
        // current buffer's local table first, then the global one — now
        // disagrees with the metadata this program was parsed with, the
        // command re-parses as typed before it runs.
        let command = match &command.command {
            ResolvedCommand::User(info) => match access
                .with_ex_editor(|editor| revalidated_user_command(runtime, editor, info))
            {
                UserRevalidation::Current => command,
                UserRevalidation::Vanished => {
                    return error_flow(
                        runtime,
                        "E492",
                        format!("Not an editor command: {}", runtime.executing.line),
                    );
                }
                UserRevalidation::Changed => {
                    let probe = Instruction {
                        command: None,
                        source: instruction.typed.clone(),
                        typed: instruction.typed.clone(),
                        raw: instruction.raw.clone(),
                        line: instruction.line,
                        retried: false,
                    };
                    let flow = run_deferred_line(runtime, access, scope, lua, &probe);
                    if !matches!(flow, Flow::Normal)
                        && !display_error_message(runtime, access, &flow)
                    {
                        return flow;
                    }
                    pc += 1;
                    continue;
                }
            },
            _ => command,
        };
        let name = command.command.name();
        match name {
            "if" => {
                // Upstream runs the `:if` line itself: `ex_if` evaluates the
                // condition when the line comes up, and `do_cmdline` only
                // reports the missing closer after its loop. A condition that
                // fails to evaluate therefore raises its own error — E1169
                // for a too-recursive expression — before E171 can.
                let first = match eval_condition(
                    runtime,
                    access,
                    scope,
                    lua,
                    skipwhite_trim(&command.args),
                ) {
                    Ok(value) => value,
                    Err(flow) => return flow,
                };
                let Some(block) = find_if(program, pc, end) else {
                    return Flow::Exception(runtime.unterminated_block("E171", "Missing :endif"));
                };
                let mut chosen = None;
                for (index, branch) in block.branches.iter().enumerate() {
                    let take = if index == 0 {
                        first
                    } else {
                        match branch.condition.as_deref() {
                            Some(condition) => {
                                match eval_condition(runtime, access, scope, lua, condition) {
                                    Ok(value) => value,
                                    Err(flow) => return flow,
                                }
                            }
                            None => true,
                        }
                    };
                    if take {
                        chosen = Some((branch.start, branch.end));
                        break;
                    }
                }
                if let Some((branch_start, branch_end)) = chosen {
                    let flow = run_program(
                        runtime,
                        access,
                        scope,
                        lua,
                        program,
                        branch_start,
                        branch_end,
                    );
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                }
                pc = block.end + 1;
                continue;
            }
            "while" => {
                let Some(block_end) = find_matching(program, pc, end, "while", "endwhile") else {
                    return Flow::Exception(
                        runtime.unterminated_block("E170", "Missing :endwhile"),
                    );
                };
                loop {
                    match eval_condition(runtime, access, scope, lua, skipwhite_trim(&command.args))
                    {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(flow) => return flow,
                    }
                    match run_program(runtime, access, scope, lua, program, pc + 1, block_end) {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        flow => return flow,
                    }
                }
                pc = block_end + 1;
                continue;
            }
            "for" => {
                let Some((target, expression)) = split_for(&command.args) else {
                    return error_flow(runtime, "E690", "Missing \"in\" after :for");
                };
                let value = match eval_text(runtime, access, scope, lua, expression) {
                    Ok(value) => value,
                    Err(flow) => return flow,
                };
                let values = match iterable_values(value) {
                    Ok(values) => values,
                    // `eval_for_line` (eval.c:1528-1531): anything that is
                    // not a String, List, or Blob is E1098; the E714 tuple
                    // keeps the pre-existing locked-list signal.
                    Err((code, message)) => return error_flow(runtime, code, message),
                };
                let Some(block_end) = find_matching(program, pc, end, "for", "endfor") else {
                    return Flow::Exception(runtime.unterminated_block("E170", "Missing :endfor"));
                };
                let mut back_edges: usize = 0;
                for value in values {
                    if let Err(flow) = assign_target(runtime, access, scope, target, value, false) {
                        return flow;
                    }
                    match run_program(runtime, access, scope, lua, program, pc + 1, block_end) {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        flow => return flow,
                    }
                    // Upstream `line_breakcheck()` runs on every loop back
                    // edge; one turn every `BREAKCHECK_SKIP` back edges keeps
                    // the same cadence without polling per iteration.
                    back_edges += 1;
                    if back_edges.is_multiple_of(BREAKCHECK_SKIP)
                        && let Some(lua) = lua
                        // A re-entrant loop inside a Lua host callback finds
                        // the host already mutably borrowed; upstream pumps no
                        // events from inside Lua either, so skip that turn.
                        && let Ok(mut host) = lua.try_borrow_mut()
                        && let Err(error) = host.run_event_turn()
                    {
                        return lua_error_flow(runtime, error, "E5107", "E5108");
                    }
                }
                pc = block_end + 1;
                continue;
            }
            "try" => {
                let Some(block) = find_try(program, pc, end) else {
                    return Flow::Exception(runtime.unterminated_block("E600", "Missing :endtry"));
                };
                runtime.try_depth += 1;
                let mut pending =
                    run_program(runtime, access, scope, lua, program, pc + 1, block.try_end);
                if let Flow::Exception(exception) = &pending {
                    let message = exception.message();
                    let throwpoint = exception.throwpoint.clone();
                    for catch in &block.catches {
                        let matched = match catch.pattern.as_deref() {
                            None => true,
                            Some(pattern) => match regex_matches_catch_pattern(pattern, &message) {
                                Ok(value) => value,
                                Err(detail) => {
                                    runtime.try_depth -= 1;
                                    return error_flow(runtime, "E54", detail);
                                }
                            },
                        };
                        if matched {
                            let saved_exception = scope.replace_pair(
                                ScopeKind::Vim,
                                "exception",
                                Typval::String(OxStr::from(message.as_str())),
                            );
                            let saved_throwpoint = scope.replace_pair(
                                ScopeKind::Vim,
                                "throwpoint",
                                Typval::String(OxStr::from(throwpoint.as_str())),
                            );
                            pending = run_program(
                                runtime,
                                access,
                                scope,
                                lua,
                                program,
                                catch.start,
                                catch.end,
                            );
                            scope.restore_pair(ScopeKind::Vim, "exception", saved_exception);
                            scope.restore_pair(ScopeKind::Vim, "throwpoint", saved_throwpoint);
                            break;
                        }
                    }
                }
                if let Some((finally_start, finally_end)) = block.finally {
                    let final_flow = run_program(
                        runtime,
                        access,
                        scope,
                        lua,
                        program,
                        finally_start,
                        finally_end,
                    );
                    if !matches!(final_flow, Flow::Normal) {
                        pending = final_flow;
                    }
                }
                runtime.try_depth -= 1;
                if !matches!(pending, Flow::Normal) {
                    return pending;
                }
                pc = block.end + 1;
                continue;
            }
            "function" => {
                let listed =
                    command.args.trim().is_empty() || command.args.trim_start().starts_with('/');
                if listed {
                    let message_start = access.with_ex_editor(|editor| editor.messages().len());
                    let flow = access
                        .with_ex_editor(|editor| command_function_list(runtime, editor, command));
                    if let Err(capture_flow) = access.with_ex_editor(|editor| {
                        capture_command_messages(runtime, editor, scope, command, message_start)
                    }) {
                        return capture_flow;
                    }
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                    pc += 1;
                    continue;
                }
                let Some(block_end) = find_matching(program, pc, end, "function", "endfunction")
                else {
                    return error_flow(runtime, "E126", "Missing :endfunction");
                };
                let signature = match UserFunctions::parse_signature(&command.args) {
                    Ok(signature) => signature,
                    Err(error) => return userfunc_error_flow(runtime, error),
                };
                let dictionary_target = match dictionary_function_target(scope, &signature.name) {
                    Ok(target) => target,
                    Err((code, message)) => return error_flow(runtime, code, message),
                };
                let body = program[pc + 1..block_end]
                    .iter()
                    .map(|item| item.source.clone())
                    .collect::<Vec<_>>();
                let context = SourceContext {
                    sid: runtime.scripts.current_sid().unwrap_or(0),
                    seq: runtime.scripts.current_seq(),
                    lnum: program[pc].line,
                };
                // "Function can be replaced with function! and when sourcing
                // the same script again, but only once"
                // (`eval/userfunc.c:2856-2863`).
                let same_script_reload = runtime
                    .functions
                    .get(&signature.name, context.sid)
                    .is_some_and(|existing| {
                        existing.context.sid == context.sid && existing.context.seq != context.seq
                    });
                let canonical = match runtime.functions.define(
                    signature,
                    body,
                    context,
                    command.bang || same_script_reload,
                    scope,
                ) {
                    Ok(name) => name,
                    Err(error) => return userfunc_error_flow(runtime, error),
                };
                if let Some((dictionary, key)) = dictionary_target {
                    let mut data = match dictionary.try_borrow_mut() {
                        Ok(data) if !data.lock.locked => data,
                        Ok(_) => return error_flow(runtime, "E741", "Value is locked"),
                        Err(_) => return error_flow(runtime, "E742", "Cannot change value"),
                    };
                    let value = Typval::Funcref(Funcref {
                        name: OxStr::from(canonical.as_str()),
                        args: Vec::new(),
                        dict: None,
                        registry: None,
                    });
                    if let Some(entry) = data.entries.iter_mut().find(|entry| entry.key == key) {
                        entry.value = value;
                    } else {
                        data.entries.push(DictEntry::new(key, value));
                    }
                }
                pc = block_end + 1;
                continue;
            }
            "append" | "insert" => {
                let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
                    return error_flow(runtime, "E749", "Empty buffer");
                };
                let current = access.with_ex_editor(|editor| {
                    editor
                        .current_window()
                        .and_then(|window| editor.window(window).ok())
                        .map_or(1, |window| window.cursor.lnum)
                });
                let after = if name == "insert" {
                    current.saturating_sub(1)
                } else {
                    let empty = access
                        .with_ex_editor(|editor| buffer_lines(editor, buffer))
                        .ok()
                        .and_then(|lines| lines.get(current.saturating_sub(1)).cloned())
                        .is_some_and(|line| line.iter().all(u8::is_ascii_whitespace));
                    if empty {
                        current.saturating_sub(1)
                    } else {
                        current
                    }
                };
                let mut lines = Vec::new();
                pc += 1;
                while pc < end {
                    let raw = program[pc].raw.clone();
                    pc += 1;
                    if raw.trim() == "." {
                        break;
                    }
                    lines.push(raw.into_bytes());
                }
                let cursor = access.with_ex_editor(|editor| {
                    editor
                        .current_window()
                        .and_then(|window| editor.window(window).ok())
                        .map_or(
                            Position {
                                lnum: after.max(1),
                                col: 0,
                            },
                            |window| window.cursor,
                        )
                });
                let existing = access
                    .with_ex_editor(|editor| buffer_lines(editor, buffer))
                    .unwrap_or_default();
                let replace_empty = existing.len() == 1
                    && existing[0].iter().all(u8::is_ascii_whitespace)
                    && name == "append";
                let result = if replace_empty {
                    access.with_ex_editor(|editor| {
                        editor.replace_buffer_lines(crate::LineReplaceRequest {
                            buffer,
                            start: 1,
                            end: 1,
                            lines: &lines,
                            cursor_before: cursor,
                            cursor_after: Position { lnum: 1, col: 0 },
                            timestamp: 0,
                        })
                    })
                } else {
                    access.with_ex_editor(|editor| {
                        editor.append_buffer_lines(buffer, after, &lines, cursor, 0)
                    })
                };
                if let Err(error) = result {
                    return error_flow(runtime, "E16", error.to_string());
                }
                continue;
            }
            "elseif" | "else" | "endif" | "endwhile" | "endfor" | "catch" | "finally"
            | "endtry" | "endfunction" => {
                return error_flow(runtime, "E580", format!(":{name} without matching opener"));
            }
            _ => {}
        }

        let message_start = access.with_ex_editor(|editor| editor.messages().len());
        let flow = dispatch(runtime, access, scope, lua, command);
        if let Err(capture_flow) = access.with_ex_editor(|editor| {
            capture_command_messages(runtime, editor, scope, command, message_start)
        }) {
            return capture_flow;
        }
        access.with_ex_editor(|editor| refresh_special_registers(editor, scope));
        access.with_ex_editor(|editor| refresh_local_options(editor, scope));

        if !matches!(flow, Flow::Normal) {
            let silent_bang = command
                .modifiers
                .iter()
                .any(|modifier| modifier.kind == ModifierKind::Silent && modifier.bang);
            if silent_bang && matches!(flow, Flow::Exception(_) | Flow::NotImplemented(_)) {
                pc += 1;
                continue;
            }
            if display_error_message(runtime, access, &flow) {
                pc += 1;
                continue;
            }
            return flow;
        }
        pc += 1;
    }
    Flow::Normal
}

/// Outcome of checking one pre-parsed user command against the live view.
enum UserRevalidation {
    /// The live lookup still carries the same flags and address domain: the
    /// stored command runs, and its body resolves at invocation.
    Current,
    /// The command no longer resolves in the live view.
    Vanished,
    /// The live definition has different parser flags or address domain, so
    /// the pre-parsed argument shape may be wrong and the command re-parses
    /// exactly as typed.
    Changed,
}

/// Re-resolves one stored user command against the live view (current
/// buffer's local table first, then the global one).
fn revalidated_user_command<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &Editor,
    info: &UserCommandInfo,
) -> UserRevalidation {
    let fresh = {
        let registry = runtime.user_commands.borrow();
        let provider = UserCommandLookup {
            registry: &registry,
            buffer: editor.current_buffer(),
        };
        resolve_command(&info.name, &provider).ok()
    };
    match fresh {
        Some(ResolvedCommand::User(live))
            if live.flags == info.flags && live.addr_type == info.addr_type =>
        {
            UserRevalidation::Current
        }
        Some(ResolvedCommand::User(_)) => UserRevalidation::Changed,
        _ => UserRevalidation::Vanished,
    }
}

/// Re-parses and runs one stored line against the live user-command view.
///
/// Upstream parses and executes one command at a time (`do_one_cmd`'s loop),
/// so a command earlier in the line can change the tables the later ones
/// resolve against — `buffer B | Local` switches to `B` before `Local` is
/// parsed. The stored line is therefore walked with
/// [`Parser::parse_first`], each resolved command going onto one line-local
/// program, so bar-split structural blocks (`if … | … | endif`) keep their
/// program-level discovery; a command that does not resolve *yet* is stored
/// deferred and re-parses when its turn comes, by then against the buffer
/// the earlier commands switched to.
fn run_deferred_line<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    instruction: &Instruction,
) -> Flow {
    let mut program: Vec<Instruction> = Vec::new();
    let mut remaining = instruction.source.clone();
    loop {
        if remaining.trim().is_empty() || remaining.trim_start().starts_with('"') {
            break;
        }
        let parse = {
            let registry = runtime.user_commands.borrow();
            let provider = UserCommandLookup {
                registry: &registry,
                buffer: access.with_ex_editor(|editor| editor.current_buffer()),
            };
            ExParser::with_user_commands(&provider).parse_first(&remaining)
        };
        match parse {
            Ok(Some((command, end))) => {
                let typed = remaining[..command.span.end.min(remaining.len())].to_owned();
                let source = render_command(&command);
                let raw = std::mem::take(&mut remaining);
                raw[end.min(raw.len())..].clone_into(&mut remaining);
                if remaining.starts_with('|') {
                    remaining = remaining[1..].to_owned();
                }
                program.push(Instruction {
                    source,
                    command: Some(command),
                    typed,
                    raw,
                    line: instruction.line,
                    retried: false,
                });
            }
            Ok(None) => break,
            Err(_) => {
                // Deferred like parse_program defers a whole line: this slot
                // re-parses when executed, against the then-current view.
                // The raw passthrough keeps `append_command`'s error echo
                // showing the line as written.
                let raw = if program.is_empty() {
                    instruction.raw.clone()
                } else {
                    remaining.clone()
                };
                program.push(Instruction {
                    command: None,
                    source: remaining.clone(),
                    typed: remaining.clone(),
                    raw,
                    line: instruction.line,
                    retried: true,
                });
                break;
            }
        }
    }
    if program.is_empty() {
        return Flow::Normal;
    }
    let end = program.len();
    run_program(runtime, access, scope, lua, &program, 0, end)
}

#[expect(
    clippy::too_many_lines,
    reason = "the Ex dispatcher keeps command routing and address validation in one ordered match"
)]
fn dispatch<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let name = command.command.name();
    if name == "windo"
        && command.range.is_some()
        && access
            .with_ex_editor(|editor| resolve_range_raw(editor, command))
            .is_err_and(|message| message == "Invalid range")
    {
        return error_flow(runtime, "E493", "Backwards range given");
    }
    // invalid_range runs in do_one_cmd before the command function, so every
    // EX_RANGE command is bounded whether or not it goes on to resolve its
    // addresses (ex_docmd.c:2209).
    if let Err(message) = access.with_ex_editor(|editor| check_address_domain(editor, command)) {
        return error_flow(runtime, "E16", message);
    }
    match name {
        "lua" => command_lua(runtime, access, scope, lua, command),
        "luado" => command_luado(runtime, access, scope, lua, command),
        "luafile" => command_luafile(runtime, access, scope, lua, command),
        "let" => command_let(runtime, access, scope, lua, &command.args, false),
        "const" => command_let(runtime, access, scope, lua, &command.args, true),
        "unlet" => command_unlet(runtime, access, scope, &command.args, command.bang),
        "delfunction" => command_delfunction(runtime, command),
        "set" => command_set(
            runtime,
            access,
            scope,
            lua,
            &command.args,
            SetLayer::Effective,
        ),
        "setlocal" => command_set(runtime, access, scope, lua, &command.args, SetLayer::Local),
        "setglobal" => command_set(runtime, access, scope, lua, &command.args, SetLayer::Global),
        "syntax" if matches!(command.args.trim(), "on" | "off") => Flow::Normal,
        "filetype" => command_filetype(runtime, access, scope, lua, command),
        "insert" => Flow::Normal,
        "startinsert" => {
            runtime.pending_edit_mode = Some(if command.bang {
                PendingEditMode::Append
            } else {
                PendingEditMode::Insert
            });
            Flow::Normal
        }
        "startreplace" => {
            runtime.pending_edit_mode = Some(PendingEditMode::Replace);
            Flow::Normal
        }
        "aunmenu" | "tlunmenu" if command.args.trim() == "*" => Flow::Normal,
        "echo" | "echomsg" | "echon" | "echoerr" => {
            command_echo(runtime, access, scope, lua, name, &command.args)
        }
        "messages" => access.with_ex_editor(command_messages),
        "help" => access.with_ex_editor(|editor| command_help(runtime, editor, command)),
        "eval" => match eval_text(runtime, access, scope, lua, skipwhite_trim(&command.args)) {
            Ok(_) => Flow::Normal,
            Err(flow) => flow,
        },
        "redir" => command_redir(runtime, access, scope, command),
        "break" => Flow::Break,
        "continue" => Flow::Continue,
        "throw" => match eval_text(runtime, access, scope, lua, skipwhite_trim(&command.args)) {
            Ok(value) => Flow::Exception(VimException {
                kind: VimExceptionKind::Throw,
                value: Box::new(value),
                throwpoint: runtime.throwpoint(),
                // `:throw` produces the value verbatim, with no `Vim(...)`
                // prefix (`get_exception_string`'s ET_USER branch).
                command: None,
            }),
            Err(flow) => flow,
        },
        "call" => command_call(runtime, access, scope, lua, command),
        "defer" => command_defer(runtime, access, scope, lua, command),
        "return" => {
            if skipwhite_trim(&command.args).is_empty() {
                Flow::Return(Typval::Number(0))
            } else {
                match eval_text(runtime, access, scope, lua, skipwhite_trim(&command.args)) {
                    Ok(value) => Flow::Return(value),
                    Err(flow) => flow,
                }
            }
        }
        "execute" => command_execute(runtime, access, scope, lua, &command.args),
        "cd" => access.with_ex_editor(|editor| {
            command_cd(runtime, editor, &command.args, DirectoryScope::Global)
        }),
        // `:tcd`/`:tchdir` is tabpage-scoped upstream; this port models one
        // local-directory scope (see `getcwd`'s "tabpage" mapping).
        "lcd" | "bcd" | "tcd" | "tchdir" => access.with_ex_editor(|editor| {
            command_cd(runtime, editor, &command.args, DirectoryScope::Window)
        }),
        "swapname" => {
            access.with_ex_editor(|editor| {
                push_text_message(editor, "No swap file".to_owned(), false, false);
            });
            Flow::Normal
        }
        "source" => {
            let path = access.with_ex_editor(|editor| argument_path(editor, &command.args));
            match source_path(runtime, access, scope, lua, &path, false) {
                Ok(Flow::Finish) => Flow::Normal,
                Ok(flow) => flow,
                Err(error) => exec_error_flow(runtime, error),
            }
        }
        "finish" if runtime.scripts.current_sid().is_some() => Flow::Finish,
        "finish" => error_flow(runtime, "E168", ":finish used outside of a sourced file"),
        "normal" => command_normal(runtime, access, scope, lua, command),
        "terminal" => command_terminal(runtime, access, scope, lua, command),
        "packadd" => command_packadd(runtime, access, scope, lua, command),
        "runtime" => command_runtime(runtime, access, scope, lua, command),
        "preserve" => {
            // `:preserve` writes the swap file (`ex_preserve`); this port
            // has no swap subsystem, so the command's whole observable
            // contract here is "succeeds without output".
            Flow::Normal
        }
        "iabbrev" => command_iabbrev(runtime, access, scope, command),
        "abclear" => {
            // `:abclear` removes abbreviations in both scopes (ex_cmds.lua
            // `abclear`, `map_clear` over abbrs).
            access.with_ex_editor(|editor| {
                editor.mappings_mut().abbrevclear(MapScope::Global);
                editor
                    .mappings_mut()
                    .abbrevclear(MapScope::Buffer(BufHandle::CURRENT));
            });
            Flow::Normal
        }
        "clearjumps" => access.with_ex_editor(command_clearjumps),
        "argadd" => access.with_ex_editor(|editor| command_argadd(runtime, editor, command)),
        "stopinsert" => command_stopinsert(runtime, access),
        "global" => command_global(runtime, access, scope, lua, command, false),
        "vglobal" => command_global(runtime, access, scope, lua, command, true),
        "substitute" => command_substitute(runtime, access, scope, command),
        "edit" | "ex" | "visual" | "view" | "drop" => {
            command_edit(runtime, access, scope, lua, command)
        }
        "find" => command_find(runtime, access, scope, lua, command),
        "read" => command_read(runtime, access, scope, lua, command),
        "enew" => command_enew(runtime, access, scope, lua, command),
        "file" => command_file(runtime, access, scope, lua, command),
        "update" => {
            let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
                return Flow::Normal;
            };
            let should_write = match access.with_ex_editor(|editor| match editor.buffer(buffer) {
                Ok(state) if state.flags.contains(crate::BufferFlags::MODIFIED) => Ok(true),
                Ok(state) => {
                    let name = state.name().to_string_lossy();
                    let ordinary = matches!(
                        editor.options().get_buffer(buffer, "buftype").cloned(),
                        Ok(OptionValue::String(value)) if value.is_empty()
                    ) && !editor.is_terminal_buffer(buffer);
                    if name.is_empty() || !ordinary {
                        Ok(false)
                    } else {
                        let path = Path::new(name.as_ref());
                        // Neovim treats every failed stat as "missing"; the
                        // `exists` fallback supports FileIO adapters without metadata.
                        Ok(runtime.scripts.io().metadata(path, true).is_err()
                            && !runtime.scripts.io().exists(path))
                    }
                }
                Err(error) => Err(error),
            }) {
                Ok(value) => value,
                Err(error) => return error_flow(runtime, "E749", error.to_string()),
            };
            if should_write {
                command_write(runtime, access, scope, lua, command)
            } else {
                Flow::Normal
            }
        }
        "write" | "wq" | "xit" => {
            let flow = command_write(runtime, access, scope, lua, command);
            if matches!(flow, Flow::Normal) && matches!(name, "wq" | "xit") {
                access.with_ex_editor(|editor| command_close(runtime, editor, command, true))
            } else {
                flow
            }
        }
        "split" | "new" => command_split(runtime, access, scope, lua, command, false),
        "vsplit" | "vnew" => command_split(runtime, access, scope, lua, command, true),
        "tabnew" | "tabedit" => {
            access.with_ex_editor(|editor| command_tabnew(runtime, editor, command))
        }
        "tabnext" | "tabn" => {
            access.with_ex_editor(|editor| command_tabnext(runtime, editor, command))
        }
        "tabonly" => access.with_ex_editor(|editor| command_tabonly(runtime, editor, command)),
        "tabclose" | "tabc" => {
            access.with_ex_editor(|editor| command_tabclose(runtime, editor, command))
        }

        "undo" => access.with_ex_editor(|editor| command_undo(runtime, editor, command)),
        "redo" => access.with_ex_editor(|editor| command_redo(runtime, editor)),
        "undojoin" => access.with_ex_editor(|editor| command_undojoin(runtime, editor)),
        "retab" => access.with_ex_editor(|editor| command_retab(runtime, editor, scope, command)),
        "hide" => access.with_ex_editor(|editor| command_hide(runtime, editor, command)),
        "sleep" => access.with_ex_editor(|editor| command_sleep(runtime, editor, command)),
        "scriptencoding" => command_scriptencoding(runtime, command),
        "argdelete" => access.with_ex_editor(|editor| command_argdelete(runtime, editor, command)),
        "z" => access.with_ex_editor(|editor| command_z(runtime, editor, command)),
        "lockvar" => command_lockvar(runtime, access, scope, command, true),
        "unlockvar" => command_lockvar(runtime, access, scope, command, false),
        "fold" => access.with_ex_editor(|editor| command_fold(runtime, editor, command)),
        "foldopen" | "foldclose" => {
            access.with_ex_editor(|editor| command_foldopen(runtime, editor, command))
        }
        "diffthis" => access.with_ex_editor(|editor| command_diffthis(runtime, editor)),
        "diffoff" => access.with_ex_editor(|editor| command_diffoff(runtime, editor, command)),
        "diffupdate" => access.with_ex_editor(|editor| command_diffupdate(runtime, editor)),
        "resize" => access.with_ex_editor(|editor| command_resize(runtime, editor, command)),
        "wincmd" => access.with_ex_editor(|editor| command_wincmd(runtime, editor, command)),
        "echohl" => access.with_ex_editor(|editor| command_echohl(runtime, editor, command)),
        "redraw" | "redrawstatus" | "redrawtabline" => command_redraw(runtime, access, scope, lua),
        "close" => access.with_ex_editor(|editor| command_close(runtime, editor, command, false)),
        "pclose" => access.with_ex_editor(|editor| command_pclose(runtime, editor)),
        "only" => access.with_ex_editor(|editor| command_only(runtime, editor)),
        "quit" => access.with_ex_editor(|editor| command_close(runtime, editor, command, true)),
        "qall" => access.with_ex_editor(|editor| command_qall(runtime, editor, command)),
        "cquit" => access.with_ex_editor(|editor| command_cquit(runtime, editor, command)),
        "bnext" => access.with_ex_editor(|editor| command_buffer_step(runtime, editor, command, 1)),
        "bprevious" | "bprev" => {
            access.with_ex_editor(|editor| command_buffer_step(runtime, editor, command, -1))
        }
        "bfirst" | "brewind" => {
            access.with_ex_editor(|editor| command_buffer_absolute(runtime, editor, command, 0))
        }
        "blast" => access
            .with_ex_editor(|editor| command_buffer_absolute(runtime, editor, command, isize::MAX)),
        "buffer" | "b" => access.with_ex_editor(|editor| command_buffer(runtime, editor, command)),
        "ls" | "buffers" | "files" => {
            access.with_ex_editor(|editor| command_buffer_list(runtime, editor, command))
        }
        "bwipeout" | "bwipe" => {
            access.with_ex_editor(|editor| command_buffer_remove(runtime, editor, command, true))
        }
        "bdelete" | "bdel" | "bunload" | "bun" => {
            access.with_ex_editor(|editor| command_buffer_remove(runtime, editor, command, false))
        }
        "args" => access.with_ex_editor(|editor| command_args(runtime, editor, command)),
        "next" => access.with_ex_editor(|editor| command_next(runtime, editor, command)),
        "first" | "rewind" => {
            access.with_ex_editor(|editor| command_argument_absolute(runtime, editor, command, 0))
        }
        "last" => access
            .with_ex_editor(|editor| command_argument_absolute(runtime, editor, command, i64::MAX)),
        "argument" => access.with_ex_editor(|editor| command_argument(runtime, editor, command)),
        "previous" | "Next" => {
            access.with_ex_editor(|editor| command_previous(runtime, editor, command))
        }
        "wnext" => {
            let flow = command_write(runtime, access, scope, lua, command);
            if matches!(flow, Flow::Normal) {
                access.with_ex_editor(|editor| command_next(runtime, editor, command))
            } else {
                flow
            }
        }
        "wprevious" => {
            let flow = command_write(runtime, access, scope, lua, command);
            if matches!(flow, Flow::Normal) {
                access.with_ex_editor(|editor| command_previous(runtime, editor, command))
            } else {
                flow
            }
        }
        "!" => command_bang(runtime, access, scope, lua, command),
        "argdo" => command_argdo(runtime, access, scope, lua, command),
        "windo" => command_windo(runtime, access, scope, lua, command),
        "put" => command_put(runtime, access, scope, lua, command),
        "print" => access.with_ex_editor(|editor| command_print(runtime, editor, command)),
        "delete" => access.with_ex_editor(|editor| command_delete(runtime, editor, command)),
        "yank" => access.with_ex_editor(|editor| command_yank(runtime, editor, command)),
        "mark" | "k" => access.with_ex_editor(|editor| command_mark(runtime, editor, command)),
        "marks" => access.with_ex_editor(|editor| command_marks(runtime, editor)),
        "jumps" => access.with_ex_editor(command_jumps),
        "delmarks" => access.with_ex_editor(|editor| command_delmarks(runtime, editor, command)),
        "cc" | "ll" => {
            access.with_ex_editor(|editor| command_quickfix_jump(runtime, editor, command, false))
        }
        "cnext" | "lnext" | "cnfile" | "lnfile" | "cbelow" | "lbelow" | "cafter" | "lafter" => {
            access.with_ex_editor(|editor| command_quickfix_next(runtime, editor, command, 1))
        }
        "cprevious" | "cprev" | "lprevious" | "lprev" | "cNext" | "cNfile" | "lNext" | "lNfile"
        | "cabove" | "labove" | "cbefore" | "lbefore" => {
            access.with_ex_editor(|editor| command_quickfix_next(runtime, editor, command, -1))
        }
        "cfirst" | "lfirst" | "crewind" | "lrewind" => {
            access.with_ex_editor(|editor| command_quickfix_next(runtime, editor, command, 0))
        }
        "clast" | "llast" => {
            access.with_ex_editor(|editor| command_quickfix_last(runtime, editor, command))
        }
        "colder" | "lolder" => {
            access.with_ex_editor(|editor| command_quickfix_age(runtime, editor, -1))
        }
        "cnewer" | "lnewer" => {
            access.with_ex_editor(|editor| command_quickfix_age(runtime, editor, 1))
        }
        "clist" | "llist" => access.with_ex_editor(|editor| command_quickfix_list(runtime, editor)),
        "copen" | "lopen" => {
            access.with_ex_editor(|editor| command_quickfix_open(runtime, editor, command))
        }
        "cclose" | "lclose" => {
            access.with_ex_editor(|editor| command_quickfix_close(runtime, editor))
        }
        "cwindow" | "lwindow" => {
            access.with_ex_editor(|editor| command_quickfix_window(runtime, editor))
        }
        "cexpr" | "cgetexpr" | "laddexpr" | "caddexpr" | "lexpr" | "lgetexpr" | "lcexpr" => {
            command_quickfix_expr(runtime, access, scope, lua, command)
        }
        "cbuffer" | "cgetbuffer" | "lbuffer" | "lgetbuffer" | "caddbuffer" | "laddbuffer" => access
            .with_ex_editor(|editor| command_quickfix_buffer(runtime, editor, scope, lua, command)),
        "cfile" | "cgetfile" | "lfile" | "lgetfile" | "caddfile" | "laddfile" => {
            access.with_ex_editor(|editor| command_quickfix_file(runtime, editor, command))
        }
        "registers" | "display" => {
            access.with_ex_editor(|editor| command_registers(runtime, editor, &command.args))
        }
        "colorscheme" => command_colorscheme(runtime, access, scope, lua, command),
        "language" => {
            access.with_ex_editor(|editor| command_language(runtime, editor, scope, command))
        }
        "highlight" => access.with_ex_editor(|editor| command_highlight(runtime, editor, command)),
        "sign" => access.with_ex_editor(|editor| command_sign(runtime, editor, command)),
        "augroup" => access.with_ex_editor(|editor| command_augroup(runtime, editor, command)),
        "autocmd" => command_autocmd(runtime, access, lua, command),
        "command" => access.with_ex_editor(|editor| command_user_command(runtime, editor, command)),
        "comclear" => {
            runtime.user_commands.borrow_mut().commands.clear();
            runtime.user_commands.borrow_mut().buffer_commands.clear();
            Flow::Normal
        }
        "delcommand" => {
            access.with_ex_editor(|editor| command_delcommand(runtime, editor, command))
        }
        "map" | "nmap" | "vmap" | "xmap" | "smap" | "omap" | "imap" | "cmap" | "lmap" | "tmap"
        | "noremap" | "nnoremap" | "vnoremap" | "xnoremap" | "snoremap" | "onoremap"
        | "inoremap" | "cnoremap" | "lnoremap" | "tnoremap" | "unmap" | "nunmap" | "vunmap"
        | "xunmap" | "sunmap" | "ounmap" | "iunmap" | "cunmap" | "lunmap" | "tunmap"
        | "mapclear" | "nmapclear" | "vmapclear" | "xmapclear" | "smapclear" | "omapclear"
        | "imapclear" | "cmapclear" | "lmapclear" | "tmapclear" => {
            access.with_ex_editor(|editor| command_map(runtime, editor, scope, command))
        }
        "tag" | "tjump" | "tselect" | "tnext" | "tprevious" | "tNext" | "tfirst" | "trewind"
        | "tlast" | "pop" | "ltag" | "ptag" | "ptjump" | "ptnext" | "ptprevious" | "ptNext"
        | "ptfirst" | "ptrewind" | "ptlast" | "ptselect" | "stag" => {
            command_tag(runtime, access, scope, lua, command)
        }

        "isearch" | "ilist" | "ijump" | "isplit" | "dsearch" | "dlist" | "djump" | "dsplit" => {
            access.with_ex_editor(|editor| command_findpat(runtime, editor, command))
        }

        _ => match &command.command {
            ResolvedCommand::RangeOnly => {
                access.with_ex_editor(|editor| command_range_only(runtime, editor, command))
            }
            ResolvedCommand::User(user_info) => {
                command_invoke_user(runtime, access, scope, lua, &user_info.name, command)
            }
            ResolvedCommand::Builtin(spec) => Flow::NotImplemented(spec.name.to_owned()),
        },
    }
}

pub(crate) fn eval_text<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    text: &str,
) -> Result<Typval, Flow> {
    let expression = ExprParser::new(text.as_bytes())
        .parse()
        .map_err(|error| eval_error_flow(runtime, error))?;
    let regex = VimRegex;
    let ambiguous_wide = access.with_ex_editor(|editor| {
        matches!(editor.options().get_global("ambiwidth"), Ok(OptionValue::String(value)) if value == "double")
    });
    let mut host = EvalHost {
        runtime,
        access,
        lua,
        builtins: Builtins::new(&regex).with_ambiguous_width(ambiguous_wide),
        submatches: None,
        escaped_exception: None,
    };
    let result = Evaluator::new(&mut host, &regex).eval(&expression, scope);
    if let Some(exception) = host.escaped_exception {
        return Err(Flow::Exception(exception));
    }
    result.map_err(|error| eval_error_flow(host.runtime, error))
}

/// Serve one builtin call through the same [`EvalHost`] a Vimscript
/// expression is evaluated against.
fn call_builtin_dispatch<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &OxStr,
    args: &[Typval],
) -> Result<Typval, ExecError> {
    let regex = VimRegex;
    let ambiguous_wide = access.with_ex_editor(|editor| {
        matches!(editor.options().get_global("ambiwidth"), Ok(OptionValue::String(value)) if value == "double")
    });
    let mut host = EvalHost {
        runtime,
        access,
        lua,
        builtins: Builtins::new(&regex).with_ambiguous_width(ambiguous_wide),
        submatches: None,
        escaped_exception: None,
    };
    let name_text = name.to_string_lossy();
    let result = if let Some(family) = crate::builtins::route(&name_text) {
        crate::builtins::call(&mut host, family, &name_text, args, scope)
    } else {
        BuiltinHost::call(&mut host, name, args.to_vec(), scope)
    };
    if let Some(exception) = host.escaped_exception {
        return Err(ExecError::Vim(exception));
    }
    result.map_err(ExecError::Eval)
}

fn eval_condition<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    text: &str,
) -> Result<bool, Flow> {
    let value = eval_text(runtime, access, scope, lua, text)?;
    match value {
        Typval::Number(number) => Ok(number != 0),
        Typval::Bool(value) => Ok(value),
        Typval::String(value) => Ok(parse_number_prefix(&value.to_string_lossy()) != 0),
        Typval::Float(value) => Ok(value != 0.0),
        Typval::Channel(id) | Typval::Job(id) => Ok(id != 0),
        _ => Err(error_flow(runtime, "E745", "Using a List as a Number")),
    }
}

pub(crate) struct EvalHost<'a, F: FileIO, E: ExEditorAccess> {
    pub(crate) runtime: &'a mut ExRuntime<F>,
    pub(crate) access: &'a E,
    pub(crate) lua: Option<&'a Rc<RefCell<dyn LuaExec>>>,
    pub(crate) builtins: Builtins<'a>,
    pub(crate) submatches: Option<Vec<String>>,
    pub(crate) escaped_exception: Option<VimException>,
}

impl<F: FileIO, E: ExEditorAccess> BuiltinHost for EvalHost<'_, F, E> {
    fn call(
        &mut self,
        name: &OxStr,
        args: Vec<Typval>,
        scope: &mut Scope,
    ) -> ox_eval::Result<Typval> {
        let name_text = name.to_string_lossy();
        if let Some(reference) = name_text
            .strip_prefix(LUA_REF_FUNCTION_PREFIX)
            .and_then(|reference| reference.parse::<u64>().ok())
        {
            let Some(lua) = self.lua else {
                return Err(EvalError::new(
                    "E117",
                    0,
                    format!("Unknown function: {name_text}"),
                ));
            };
            self.access
                .with_ex_editor(|editor| sync_scope_into_editor(editor, scope))
                .map_err(|error| EvalError::new("E5108", 0, error.to_string()))?;
            let callback_args: Vec<Object> = args.iter().map(typval_to_object).collect();
            let result = lua.borrow_mut().invoke_callback(
                usize::try_from(reference).unwrap_or(usize::MAX),
                callback_args,
            );
            let sync = self
                .access
                .with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
            return match (result, sync) {
                (Err(error), _) => Err(EvalError::new("E5108", 0, error.to_string())),
                (Ok(result), Err(error)) => {
                    lua.borrow_mut().discard_result(result);
                    Err(EvalError::new("E5108", 0, error.to_string()))
                }
                (Ok(result), Ok(())) => Ok(object_to_typval(&result)),
            };
        }
        if name_text.starts_with("nvim_") {
            return self.api_call(&name_text, &args, scope);
        }
        if let Some(family) = crate::builtins::route(&name_text) {
            return crate::builtins::call(self, family, &name_text, &args, scope);
        }
        let sid = self.runtime.scripts.current_sid().unwrap_or(0);
        if self.runtime.functions.contains(&name_text, sid) || name_text.contains('#') {
            let (first, last) = self
                .access
                .with_ex_editor(|editor| current_line_pair(editor));
            return match call_user_function(
                self.runtime,
                self.access,
                scope,
                self.lua,
                &name.to_string_lossy(),
                args,
                first,
                last,
            ) {
                Ok(value) => Ok(value),
                Err(Flow::Exception(exception)) => {
                    let message = exception.message();
                    self.escaped_exception = Some(exception);
                    Err(EvalError::new("E605", 0, message))
                }
                Err(flow) => Err(flow_to_eval_error(flow, &name_text)),
            };
        }
        if ox_eval::builtin_spec(&name_text).is_none() {
            return Err(EvalError::new(
                "E117",
                0,
                format!("Unknown function: {name_text}"),
            ));
        }
        if ox_eval::is_higher_order_builtin(&name_text) {
            let regex = VimRegex;
            return ox_eval::call_higher_order_builtin(self, &regex, &name_text, args, scope);
        }
        self.builtins.call(name, args, scope)
    }

    fn closure_registry(&self) -> Option<ox_eval::eval::ClosureRegistry> {
        Some(self.runtime.closures.clone())
    }
}

impl<F: FileIO, E: ExEditorAccess> EvalHost<'_, F, E> {
    /// Calls one `nvim_*` API function through the installed Lua host, the
    /// same way a `function('nvim_…')` reaches upstream's C dispatch.
    fn api_call(
        &mut self,
        name: &str,
        args: &[Typval],
        scope: &mut Scope,
    ) -> ox_eval::Result<Typval> {
        let Some(lua) = self.lua else {
            return Err(EvalError::new(
                "E117",
                0,
                format!("Unknown function: {name}"),
            ));
        };
        let mut call = Vec::with_capacity(args.len() + 1);
        call.push(Object::String(OxStr::from(name)));
        call.extend(args.iter().map(typval_to_object));
        self.access
            .with_ex_editor(|editor| sync_scope_into_editor(editor, scope))
            .map_err(|error| EvalError::new("E5108", 0, error.to_string()))?;
        let result = lua
            .borrow_mut()
            .execute_chunk("return vim.api[select(1, ...)](select(2, ...))", call);
        let sync = self
            .access
            .with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
        match (result, sync) {
            (Err(error), _) => Err(EvalError::new("E5108", 0, error.to_string())),
            (Ok(result), Err(error)) => {
                lua.borrow_mut().discard_result(result);
                Err(EvalError::new("E5108", 0, error.to_string()))
            }
            (Ok(result), Ok(())) => Ok(object_to_typval(&result)),
        }
    }
}

fn command_resize<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    // `ex_resize` (ex_docmd.c:5911-5936): an address selects the window,
    // `+N`/`-N` are relative to the current height, and a bare `:resize`
    // means Rows - 1 ("as high as possible").
    let args = command.args.trim();
    let signed = args
        .parse::<isize>()
        .ok()
        .unwrap_or_else(|| match args.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => 0,
        });
    let window = if command.range.is_some() {
        // `:Nresize` selects the Nth window (ex_docmd.c:5915-5918).
        let target = match resolve_range_raw(editor, command) {
            Ok((_, end)) => end.max(1),
            Err(message) => return error_flow(runtime, "E16", message),
        };
        editor.windows().into_iter().nth(target - 1)
    } else {
        editor.current_window()
    };
    let Some(window) = window else {
        return error_flow(
            runtime,
            "E443",
            "Cannot rotate when another window is split",
        );
    };
    // The relative base is the window's layout height (upstream
    // `wp->w_height`), not its text height minus the status line.
    let current_height = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage(tab).ok())
        .and_then(|tabpage| tabpage.layout().window_geometry(window).ok())
        .map_or(1, |geometry| geometry.height);
    let height = if args.starts_with(['+', '-']) {
        signed + current_height.cast_signed()
    } else if args.is_empty() {
        editor
            .current_tabpage()
            .and_then(|tab| editor.tabpage(tab).ok())
            .map_or(24, |tabpage| tabpage.layout().size().height)
            .saturating_sub(1)
            .max(1)
            .cast_signed()
    } else {
        signed.max(1)
    }
    .max(1)
    .cast_unsigned();
    match editor.set_window_height(window, height) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E36", error.to_string()),
    }
}

fn command_wincmd<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    // `ex_wincmd` (ex_docmd.c:6522-6551): one window-command key, or a
    // second key for the `g`/Ctrl-G forms. The parser already split any
    // real tail at the bar (`check_nextcmd`, ex_docmd.c:4630-4637), so past
    // the key form only spaces or tabs, a `"` comment, or the end may
    // remain — anything else is E474 rather than silently dropped.
    let mut keys = command.args.chars();
    let Some(key) = keys.next() else {
        return error_flow(runtime, "E474", "Invalid argument");
    };
    if key == 'g' || key == '\u{7}' {
        // ex_docmd.c:6527-6532: the `g`/Ctrl-G forms consume a second
        // command character; a missing one is E474 before any window work.
        if keys.next().is_none() {
            return error_flow(runtime, "E474", "Invalid argument");
        }
    }
    // ex_docmd.c:6540-6542: `p = skipwhite(p)`; then only a `"` comment or
    // the end may follow the key form.
    if keys
        .find(|character| !matches!(character, ' ' | '\t'))
        .is_some_and(|character| character != '"')
    {
        return error_flow(runtime, "E474", "Invalid argument");
    }
    if matches!(key, 'i' | 'd') {
        return wincmd_ident_search(runtime, editor, command, key);
    }
    let Some(tab) = editor.current_tabpage() else {
        return Flow::Normal;
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let Some(current) = editor.current_window() else {
        return Flow::Normal;
    };
    let next = match key {
        'w' => windows
            .iter()
            .position(|window| *window == current)
            .and_then(|index| windows.get((index + 1) % windows.len()))
            .copied(),
        'W' => windows
            .iter()
            .position(|window| *window == current)
            .and_then(|index| windows.get((index + windows.len() - 1) % windows.len()))
            .copied(),
        'p' => editor
            .previous_window()
            .filter(|window| windows.contains(window)),
        'P' => match windows.iter().copied().find(|window| {
            matches!(
                editor.options().get_window(*window, "previewwindow"),
                Ok(OptionValue::Boolean(true))
            )
        }) {
            Some(window) => Some(window),
            None => return error_flow(runtime, "E441", "There is no preview window"),
        },
        'h' | 'j' | 'k' | 'l' => directional_window(editor, current, &windows, key),
        _ => return error_flow(runtime, "E474", format!("Invalid argument: {key}")),
    };
    match next.map(|window| editor.set_current_window(window)) {
        None | Some(Ok(())) => Flow::Normal,
        Some(Err(error)) => error_flow(runtime, "E957", error.to_string()),
    }
}

pub(crate) fn directional_window(
    editor: &Editor,
    current: WinHandle,
    windows: &[WinHandle],
    key: char,
) -> Option<WinHandle> {
    let origin = editor.window_geometry(current).ok()?;
    windows
        .iter()
        .copied()
        .filter(|window| *window != current)
        .filter_map(|window| {
            let geometry = editor.window_geometry(window).ok()?;
            let distance = match key {
                'h' if geometry.col < origin.col => origin.col - geometry.col,
                'l' if geometry.col > origin.col => geometry.col - origin.col,
                'k' if geometry.row < origin.row => origin.row - geometry.row,
                'j' if geometry.row > origin.row => geometry.row - origin.row,
                _ => return None,
            };
            Some((distance, window))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, window)| window)
}

fn command_echohl<F: FileIO>(
    _runtime: &ExRuntime<F>,
    _editor: &mut Editor,
    _command: &ExCommand,
) -> Flow {
    Flow::Normal
}

/// `:redraw[!]`, `:redrawstatus[!]`, and `:redrawtabline` (`ex_docmd.c`
/// `ex_redraw`/`ex_redrawstatus`/`ex_redrawtabline`).
///
/// The screen update these commands force has no model here: this editor
/// owns no grid, status line, or tabline, and the message-area resets
/// (`msg_didout`, `msg_col`, `need_wait_return`) and `maketitle` have no
/// counterpart either. What is modeled is `ex_redraw`'s `validate_cursor`
/// call, whose `check_cursor_lnum` (`cursor.c:310-323`) clamps the current
/// window's cursor onto a real buffer line. Topline is deliberately left
/// alone: `update_topline` (`move.c:270-485`) is the scroll subsystem this
/// port does not have, and guessing at it would move the viewport a real
/// `:redraw` never moves.
fn command_redraw<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
) -> Flow {
    let Some((window, cursor, last)) = access.with_ex_editor(|editor| {
        let window = editor.current_window()?;
        let state = editor.window(window).ok()?;
        let cursor = state.cursor;
        let last = editor
            .buffer(state.buffer)
            .ok()
            .and_then(|buffer| buffer.text().ok())
            .map_or(1, Buffer::line_count);
        Some((window, cursor, last))
    }) else {
        return Flow::Normal;
    };
    let clamped = cursor.lnum.clamp(1, last.max(1));
    if clamped != cursor.lnum
        && let Err(error) = access.with_ex_editor(|editor| {
            editor.set_window_cursor(
                window,
                Position {
                    lnum: clamped,
                    col: cursor.col,
                },
            )
        })
    {
        return error_flow(runtime, "E948", error.to_string());
    }
    // ex_redraw's forced screen update reaches decoration providers here.
    // The outermost redraw wraps on_start/on_end around the cursor work; a
    // callback-triggered :redraw observes an already-active transaction and
    // performs no provider work (decoration_provider.c:108-284).
    match access.with_ex_editor(|editor| editor.decorations_mut().enter_redraw(0)) {
        Ok(RedrawEntry::Nested) => Flow::Normal,
        Ok(RedrawEntry::Outermost(id)) => {
            let tick = access
                .with_ex_editor(|editor| editor.decorations().active_display_tick(id).unwrap_or(0));
            let (start_flow, end_flow) =
                match access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
                    Ok(()) => (
                        run_provider_phase(
                            runtime,
                            access,
                            lua,
                            CallbackPhase::Start,
                            &callback_args_start(tick),
                        ),
                        run_provider_phase(
                            runtime,
                            access,
                            lua,
                            CallbackPhase::End,
                            &callback_args_end(tick),
                        ),
                    ),
                    Err(error) => (exec_error_flow(runtime, error), Flow::Normal),
                };
            let finish = access.with_ex_editor(|editor| editor.decorations_mut().finish_redraw(id));
            if access.with_ex_editor(|editor| editor.current_window()) != Some(window)
                && access.with_ex_editor(|editor| editor.window(window).is_ok())
            {
                let _ = access.with_ex_editor(|editor| editor.set_current_window(window));
            }
            let sync = access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
            if !matches!(start_flow, Flow::Normal) {
                return start_flow;
            }
            if !matches!(end_flow, Flow::Normal) {
                return end_flow;
            }
            if let Err(error) = finish {
                return error_flow(runtime, "E570", error.to_string());
            }
            match sync {
                Ok(()) => Flow::Normal,
                Err(error) => exec_error_flow(runtime, error),
            }
        }
        Err(error) => error_flow(runtime, "E570", error.to_string()),
    }
}

fn callback_args_start(tick: u64) -> Vec<Object> {
    vec![
        Object::String(OxStr::from("start")),
        Object::Integer(i64::try_from(tick).unwrap_or(0)),
    ]
}

fn callback_args_end(tick: u64) -> Vec<Object> {
    vec![
        Object::String(OxStr::from("end")),
        Object::Integer(i64::try_from(tick).unwrap_or(0)),
    ]
}

/// Invokes every provider's current callback for `phase`, in registration
/// order. The callback handle is re-read immediately before each invocation so
/// a callback that removes or replaces another provider is honored later in
/// the same frame; a removed or replaced provider stops receiving calls.
/// Errors surface as E5108 without aborting the remaining providers.
pub(crate) fn run_provider_phase<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    phase: CallbackPhase,
    args: &[Object],
) -> Flow {
    let ids = access.with_ex_editor(|editor| editor.decorations().phase_provider_ids(phase));
    for id in ids {
        let Some(reference) =
            access.with_ex_editor(|editor| editor.decorations().phase_callback(id, phase))
        else {
            continue;
        };
        let Some(lua) = lua else { continue };
        let Ok(mut host) = lua.try_borrow_mut() else {
            continue;
        };
        let Ok(reference) = usize::try_from(reference) else {
            continue;
        };
        if let Err(error) = host.invoke_callback(reference, args.to_vec()) {
            drop(host);
            return lua_error_flow(runtime, error, "E5107", "E5108");
        }
    }
    Flow::Normal
}

/// Resolves a command's file argument, expanding bare `%` and `#` names.
fn argument_path(editor: &Editor, argument: &str) -> PathBuf {
    let argument = argument.trim();
    let buffer = match argument {
        "%" => editor.current_buffer(),
        "#" => editor
            .current_window()
            .and_then(|window| editor.window(window).ok())
            .and_then(|window| window.alternate_buffer),
        _ => return PathBuf::from(argument),
    };
    buffer
        .and_then(|buffer| editor.buffer(buffer).ok())
        .map_or_else(
            || PathBuf::from(argument),
            |buffer| PathBuf::from(buffer.name().to_string_lossy().into_owned()),
        )
}

pub(crate) fn resolve_buffer_argument(
    editor: &Editor,
    argument: Option<&Typval>,
) -> Option<BufHandle> {
    match argument {
        None | Some(Typval::Number(0)) => editor.current_buffer(),
        Some(Typval::Number(number)) => BufHandle::try_from(*number)
            .ok()
            .filter(|buffer| editor.buffer(*buffer).is_ok()),
        Some(Typval::String(name)) if name.as_bytes().is_empty() || name.as_bytes() == b"%" => {
            editor.current_buffer()
        }
        Some(Typval::String(name)) => editor.buffers().into_iter().find(|buffer| {
            editor
                .buffer(*buffer)
                .is_ok_and(|state| state.name() == name)
        }),
        _ => None,
    }
}

/// [`BufferHost`] adapter over the editor's current buffer, mapping the
/// evaluator's line-addressed builtins onto the single-writer line
/// mutations `Editor::replace_buffer_lines`/`append_buffer_lines`. Undo
/// timestamps match the other ex mutations here (0); the recorded cursor is
/// the window cursor, like `:substitute`.
pub(crate) struct CurrentBuffer<'a>(pub(crate) &'a mut Editor);

impl BufferHost for CurrentBuffer<'_> {
    fn line_count(&self) -> ox_eval::Result<usize> {
        let Some(buffer) = self.0.current_buffer() else {
            return Ok(0);
        };
        Ok(self
            .0
            .buffer(buffer)
            .ok()
            .and_then(|state| state.text().ok())
            .map_or(0, Buffer::line_count))
    }

    fn get_line(&self, lnum: usize) -> ox_eval::Result<Option<OxStr>> {
        let Some(buffer) = self.0.current_buffer() else {
            return Ok(None);
        };
        let line = self
            .0
            .buffer(buffer)
            .ok()
            .and_then(|state| state.text().ok())
            .and_then(|text| text.line(lnum).ok());
        Ok(line.map(OxStr))
    }

    fn replace_line(&mut self, lnum: usize, text: &OxStr) -> ox_eval::Result<()> {
        let buffer = self
            .0
            .current_buffer()
            .ok_or_else(|| EvalError::new("E749", 0, "Empty buffer"))?;
        let cursor = self.cursor_or(Position { lnum, col: 0 });
        self.0
            .replace_buffer_lines(crate::LineReplaceRequest {
                buffer,
                start: lnum,
                end: lnum,
                lines: &[text.as_bytes().to_vec()],
                cursor_before: cursor,
                cursor_after: cursor,
                timestamp: 0,
            })
            .map(|_| ())
            .map_err(|error| EvalError::new("E16", 0, error.to_string()))
    }

    fn append_line(&mut self, text: &OxStr) -> ox_eval::Result<()> {
        let buffer = self
            .0
            .current_buffer()
            .ok_or_else(|| EvalError::new("E749", 0, "Empty buffer"))?;
        let after = {
            let state = self
                .0
                .buffer(buffer)
                .map_err(|error| EvalError::new("E749", 0, error.to_string()))?;
            let text_state = state
                .text()
                .map_err(|error| EvalError::new("E749", 0, error.to_string()))?;
            text_state.line_count()
        };
        let cursor = self.cursor_or(Position {
            lnum: after + 1,
            col: 0,
        });
        self.0
            .append_buffer_lines(buffer, after, &[text.as_bytes().to_vec()], cursor, 0)
            .map(|_| ())
            .map_err(|error| EvalError::new("E16", 0, error.to_string()))
    }

    /// `var2fpos` for string lnum arguments: `"."` is the current window's
    /// cursor line, `"'x"` thek position (buffer-local first, then the
    /// uppercase/numbered globalks, like `getmark`).
    fn address_line(&self, address: &str) -> ox_eval::Result<Option<i64>> {
        let mut chars = address.chars();
        match chars.next() {
            Some('.') if chars.next().is_none() => {
                let lnum = self.cursor_or(Position { lnum: 1, col: 0 }).lnum;
                Ok(Some(i64::try_from(lnum).map_err(|_| {
                    EvalError::new("E475", 0, "Invalid argument")
                })?))
            }
            Some('\'') => {
                let Some(name) = chars.next() else {
                    return Ok(None);
                };
                let Some(buffer) = self.0.current_buffer() else {
                    return Ok(None);
                };
                let local = self
                    .0
                    .local_mark(buffer, name)
                    .map_err(|error| EvalError::new("E749", 0, error.to_string()))?
                    .map(|position| position.lnum);
                if let Some(line) = local {
                    return Ok(Some(
                        i64::try_from(line)
                            .map_err(|_| EvalError::new("E475", 0, "Invalid argument"))?,
                    ));
                }
                let global = if name.is_ascii_uppercase() || name.is_ascii_digit() {
                    self.0
                        .global_marks()
                        .get(name)
                        .map_err(|error| EvalError::new("E749", 0, error.to_string()))?
                        .and_then(|location| {
                            (location.buffer() == Some(buffer)).then_some(location.position.lnum)
                        })
                } else {
                    None
                };
                Ok(global
                    .map(|line| {
                        i64::try_from(line)
                            .map_err(|_| EvalError::new("E475", 0, "Invalid argument"))
                    })
                    .transpose()?)
            }
            _ => Ok(None),
        }
    }
}

impl CurrentBuffer<'_> {
    fn cursor_or(&self, fallback: Position) -> Position {
        self.0
            .current_window()
            .and_then(|window| self.0.window(window).ok())
            .map_or(fallback, |window| window.cursor)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct VimRegex;

impl RegexEngine for VimRegex {
    fn is_match(&self, text: &OxStr, pattern: &OxStr, ignore_case: bool) -> ox_eval::Result<bool> {
        let source = if ignore_case {
            format!("\\c{}", pattern.to_string_lossy())
        } else {
            pattern.to_string_lossy().into_owned()
        };
        let program = compile_regex(&source, Magic::Magic)
            .map_err(|error| EvalError::new("E54", 0, error.to_string()))?;
        Ok(ox_regex::exec(
            &program,
            &RegexText::new(text.to_string_lossy().into_owned()),
        )
        .is_some())
    }

    fn find(
        &self,
        text: &OxStr,
        pattern: &OxStr,
        start: usize,
    ) -> ox_eval::Result<Option<(usize, usize)>> {
        let source = text.to_string_lossy().into_owned();
        let program = compile_regex(&pattern.to_string_lossy(), Magic::Magic)
            .map_err(|error| EvalError::new("E54", 0, error.to_string()))?;
        let text = RegexText::new(source);
        let Some(position) = text.position(start) else {
            return Ok(None);
        };
        let found = regex_exec_at(&program, &text, position);
        Ok(found.map(|matched| (matched.start.byte, matched.end.byte)))
    }

    fn find_captures(
        &self,
        text: &OxStr,
        pattern: &OxStr,
        start: usize,
    ) -> ox_eval::Result<Option<ox_eval::RegexMatch>> {
        let source = text.to_string_lossy().into_owned();
        let program =
            compile_regex(&pattern.to_string_lossy(), Magic::Magic).map_err(
                |error| match error {
                    RegexCompileError::Syntax {
                        message: "lookaround suffix follows nothing",
                        ..
                    } => EvalError::new("E866", 0, "(NFA regexp) Misplaced @"),
                    other => EvalError::new("E54", 0, other.to_string()),
                },
            )?;
        let text = RegexText::new(source);
        let Some(position) = text.position(start) else {
            return Ok(None);
        };
        Ok(
            regex_exec_at(&program, &text, position).map(|matched| ox_eval::RegexMatch {
                start: matched.start.byte,
                end: matched.end.byte,
                captures: matched
                    .captures
                    .into_iter()
                    .map(|capture| capture.map(|capture| (capture.start.byte, capture.end.byte)))
                    .collect(),
            }),
        )
    }

    fn split(
        &self,
        text: &OxStr,
        pattern: &OxStr,
        keep_empty: bool,
    ) -> ox_eval::Result<Vec<OxStr>> {
        let source = text.to_string_lossy().into_owned();
        let program = compile_regex(&pattern.to_string_lossy(), Magic::Magic)
            .map_err(|error| EvalError::new("E54", 0, error.to_string()))?;
        let regex_text = RegexText::new(source.clone());
        let mut result = Vec::new();
        let mut previous = 0;
        let mut cursor = 0;
        while cursor <= source.len() {
            let Some(position) = regex_text.position(cursor) else {
                break;
            };
            let Some(matched) = regex_exec_at(&program, &regex_text, position) else {
                break;
            };
            let item = &source[previous..matched.start.byte];
            if keep_empty || !item.is_empty() {
                result.push(OxStr::from(item));
            }
            previous = matched.end.byte;
            cursor = if matched.end.byte == matched.start.byte {
                next_boundary(&source, matched.end.byte)
            } else {
                matched.end.byte
            };
        }
        let tail = &source[previous..];
        if keep_empty || !tail.is_empty() {
            result.push(OxStr::from(tail));
        }
        Ok(result)
    }

    fn substitute(
        &self,
        text: &OxStr,
        pattern: &OxStr,
        replacement: &OxStr,
        flags: &OxStr,
    ) -> ox_eval::Result<OxStr> {
        substitute_plain(
            &text.to_string_lossy(),
            &pattern.to_string_lossy(),
            &replacement.to_string_lossy(),
            flags.to_string_lossy().contains('g'),
        )
        .map(|value| OxStr(value.into_bytes()))
        .map_err(|error| EvalError::new("E54", 0, error))
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the function call boundary mirrors the evaluator's complete Vim call frame"
)]
fn call_user_function<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    args: Vec<Typval>,
    first_line: usize,
    last_line: usize,
) -> Result<Typval, Flow> {
    call_user_function_with_self(
        runtime, access, scope, lua, name, args, first_line, last_line, None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the function call boundary mirrors the evaluator's complete Vim call frame"
)]
pub(crate) fn call_user_function_with_self<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    mut args: Vec<Typval>,
    first_line: usize,
    last_line: usize,
    receiver: Option<DictRef>,
) -> Result<Typval, Flow> {
    let mut sid = runtime.scripts.current_sid().unwrap_or(0);
    if !runtime.functions.contains(name, sid) && name.contains('#') {
        let path = runtime
            .scripts
            .resolve_autoload(name)
            .ok_or_else(|| error_flow(runtime, "E117", format!("Unknown function: {name}")))?;
        if !runtime.scripts.is_sourced_once(&path) {
            let flow = source_path(runtime, access, scope, lua, &path, true)
                .map_err(|error| exec_error_flow(runtime, error))?;
            if !matches!(flow, Flow::Normal | Flow::Finish) {
                return Err(flow);
            }
        }
        sid = runtime.scripts.current_sid().unwrap_or(0);
    }
    let preview = runtime
        .functions
        .resolve(name, sid)
        .ok_or_else(|| error_flow(runtime, "E117", format!("Unknown function: {name}")))?;
    // call_user_func: defaulted parameters omitted by the caller are filled
    // by evaluating their expressions in the *caller's* scope, before the
    // frame's l:/a: maps replace it. Arity errors stay in begin_call, so the
    // defaults are only evaluated when the call is otherwise well-formed.
    let required = preview.args.len() - preview.default_args.len();
    if args.len() >= required && args.len() < preview.args.len() {
        for expression in &preview.default_args[args.len() - required..] {
            {
                let value = eval_text(runtime, access, scope, lua, expression)?;
                args.push(value);
            }
        }
    }
    let logical = preview
        .body
        .iter()
        .enumerate()
        .map(|(index, text)| LogicalLine {
            text: text.clone(),
            first_line: index + 1,
        })
        .collect::<Vec<_>>();
    let parsed = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    let function = runtime
        .functions
        .begin_call(name, sid, args, first_line, last_line, scope)
        .map_err(|error| userfunc_error_flow(runtime, error))?;
    if let Some(receiver) = receiver {
        scope
            .local
            .push((OxStr::from("self"), Typval::Dict(receiver)));
    }
    // `call_user_func` gives every frame its own `fc_defer` list, and pops it
    // in the cleanup after the body regardless of how the body ended. The
    // `defer()`-registered calls and `writefile(..., 'D')` deletes share the
    // frame's lifetime.
    runtime.deferred_ops.push(Vec::new());
    let sid = function.context.sid;
    let switched_script = sid != 0 && runtime.scripts.current_sid() != Some(sid);
    let caller_script = scope.script.clone();
    if sid != 0 {
        let name = runtime
            .scripts
            .script_name(sid)
            .map_or_else(|| format!("<SNR>{sid}"), std::borrow::ToOwned::to_owned);
        runtime
            .scripts
            .push_alias_source(sid, function.context.seq, function.context.lnum, name);
    }
    if switched_script {
        runtime.scripts.load_script_scope(sid, scope);
    }
    let flow = run_program(runtime, access, scope, lua, &parsed, 0, parsed.len());
    let deferred_flow = drain_frame_defers(runtime, access, scope, lua, first_line, last_line);
    if switched_script {
        runtime.scripts.store_script_scope(sid, scope);
        scope.script = caller_script;
    }
    let flow = match flow {
        Flow::NotImplemented(name) => {
            error_flow(runtime, "E117", format!("not implemented: {name}"))
        }
        flow => flow,
    };
    if sid != 0 {
        runtime.scripts.pop_source();
    }
    runtime.functions.end_call(scope);
    // A deferred call's error surfaces after the primary return, and never
    // replaces the body error.
    let primary = match flow {
        Flow::Normal => Ok(Typval::Number(0)),
        Flow::Return(value) => Ok(value),
        flow => Err(flow),
    };
    match (primary, deferred_flow) {
        (Ok(_), Some(flow)) => Err(flow),
        (primary, _) => primary,
    }
}

fn source_path<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    path: &Path,
    load_once: bool,
) -> Result<Flow, ExecError> {
    if load_once && runtime.scripts.is_sourced_once(path) {
        return Ok(Flow::Normal);
    }
    let text = runtime
        .scripts
        .read_script(path)
        .map_err(|error| ExecError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    let lines = runtime
        .scripts
        .join_logical_lines(&text)
        .map_err(|error| ExecError::Vim(runtime.exception(error.code, error.message)))?;
    let name = runtime
        .scripts
        .io()
        .canonicalize(path)
        .display()
        .to_string();
    let caller_script = scope.script.clone();
    let caller_augroup = runtime.current_augroup;
    let sid = runtime.scripts.push_source(name);
    let lines = expand_script_lines(&runtime.scripts, lines, sid);
    runtime.scripts.load_script_scope(sid, scope);
    if load_once {
        runtime.scripts.mark_sourced_once(path);
    }
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &lines,
    );
    let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
    runtime.scripts.store_script_scope(sid, scope);
    runtime.scripts.pop_source();
    scope.script = caller_script;
    runtime.current_augroup = caller_augroup;
    Ok(flow)
}
/// `call_user_func` cleanup (userfunc.c:1272, 3487-3524): pops the frame's
/// one `fc_defer` list and runs it last-registered-first while the callee
/// frame — including its script scope — is still live. Deletes and `defer()`
/// calls share that list upstream, so they interleave by registration order
/// here rather than draining as two separate lists. Returns the first
/// deferred-call failure, which surfaces only when the body itself succeeded.
fn drain_frame_defers<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    first_line: usize,
    last_line: usize,
) -> Option<Flow> {
    // Keep an empty frame installed while the captured operations run:
    // `handle_defer_one` (userfunc.c:3487-3524) drains with the frame still
    // current, so a deferred builtin that itself defers (`writefile` 'D')
    // sees `can_add_defer()` true and registers on the live frame.
    let mut deferred_flow = None;
    loop {
        let frame = runtime
            .deferred_ops
            .last_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        if frame.is_empty() {
            break;
        }
        for operation in frame.into_iter().rev() {
            match operation {
                DeferredOp::Call(name, args) => {
                    // `call_func` (userfunc.c:3512) resolves the deferred name
                    // generically: a user function when one exists, a builtin
                    // otherwise (`defer('delete', ...)`).
                    let result = if runtime.functions.contains(&name, 0) {
                        call_user_function(
                            runtime, access, scope, lua, &name, args, first_line, last_line,
                        )
                        .map(|_| Typval::Number(0))
                    } else {
                        call_builtin_dispatch(
                            runtime,
                            access,
                            scope,
                            lua,
                            &OxStr::from(name.as_str()),
                            &args,
                        )
                        .map_err(|error| exec_error_flow(runtime, error))
                    };
                    if let Err(flow) = result {
                        deferred_flow.get_or_insert(flow);
                    }
                }
                DeferredOp::Delete(path, mode) => {
                    // `delete(name, flags)` reports failure through its return
                    // value, which `add_defer`'s deferred call discards.
                    let _ignored = mode.remove(runtime.scripts.io(), &path);
                }
            }
        }
    }
    runtime.deferred_ops.pop();
    deferred_flow
}

fn list_scoped_variables(editor: &mut Editor, scope: &Scope, args: &str) -> bool {
    let bare = args.trim();
    let [prefix, b':'] = bare.as_bytes() else {
        return false;
    };
    let Some(kind) = ScopeKind::from_byte(*prefix) else {
        return false;
    };
    let entries = match kind {
        ScopeKind::Global => &scope.global,
        ScopeKind::Buffer => &scope.buffer,
        ScopeKind::Window => &scope.window,
        ScopeKind::Tab => &scope.tab,
        ScopeKind::Script => &scope.script,
        ScopeKind::Local => &scope.local,
        ScopeKind::Argument => &scope.argument,
        ScopeKind::Vim => &scope.vim,
    };
    let rows = entries
        .iter()
        .map(|(name, value)| {
            let qualified = format!("{bare}{}", name.to_string_lossy());
            let display = match value {
                Typval::Number(number) => format!("#{number}"),
                value => typval_to_display(value, true),
            };
            format!("{qualified:<22}{display}")
        })
        .collect::<Vec<_>>();
    for row in rows {
        push_info_text_message(editor, row);
    }
    true
}

fn command_let<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: &str,
    constant: bool,
) -> Flow {
    if !constant && access.with_ex_editor(|editor| list_scoped_variables(editor, scope, args)) {
        return Flow::Normal;
    }
    let Some((target, operator, expression)) = split_assignment(args) else {
        return error_flow(
            runtime,
            "E121",
            format!("Undefined variable: {}", args.trim()),
        );
    };
    let value = if let Some((header, body)) = expression.split_once('\n') {
        if !header.trim_start().starts_with("<<") {
            return error_flow(runtime, "E15", "Invalid expression");
        }
        let items = if body.is_empty() {
            Vec::new()
        } else {
            body.strip_suffix('\n')
                .unwrap_or(body)
                .split('\n')
                .map(|line| Typval::String(OxStr::from(line.as_bytes())))
                .collect()
        };
        Typval::list(items)
    } else {
        match eval_text(
            runtime,
            access,
            scope,
            lua,
            strip_expression_comment(expression),
        ) {
            Ok(value) => value,
            Err(flow) => return flow,
        }
    };
    // `ex_let_env` (eval/vars.c 1323-1330) parses the name with `get_env_len`
    // and reports `E475` naming the whole remaining argument when it is empty.
    // The value is evaluated first upstream (`ex_let` fills `tv` before
    // `ex_let_one`), so `let $ = g:nope` is `E121`, not `E475`; the guard sits
    // after the evaluation to keep that order.
    if let Some(name) = target.trim_start().strip_prefix('$')
        && env_name_len(name) == 0
    {
        return error_flow(
            runtime,
            "E475",
            format!("Invalid argument: {}", args.trim()),
        );
    }
    // `ex_let_one` resolves the target through `get_lval`, whose
    // `make_expanded_name` (eval.c:5769) evaluates curly-braces pieces and
    // re-joins the name before any variable lookup. The value is evaluated
    // first upstream, so this sits behind the expression above; the same
    // expansion serves the lvalue route below, so plain and subscripted
    // targets share one implementation.
    let curly_target = match expand_curly_target(runtime, access, scope, lua, target) {
        Ok(target) => target,
        Err(flow) => return flow,
    };
    let target = curly_target.as_str();
    let key = canonical_target(target);
    if runtime.const_vars.contains(&key) {
        return error_flow(
            runtime,
            "E46",
            format!("Cannot change read-only variable \"{target}\""),
        );
    }
    // Subscripted targets resolve through the lvalue path (`get_lval`),
    // where a reached entry's read-only flag refuses with E46; root names
    // keep the plain assignment routes.
    let bound = if has_subscript(target) {
        match parse_and_bind_lvalue(runtime, access, scope, lua, target) {
            Ok(bound) => Some(bound),
            Err(flow) => return flow,
        }
    } else {
        None
    };
    let assigned = if operator == "=" {
        value
    } else {
        let previous = match &bound {
            Some(lvalue) => match read_lvalue(runtime, access, scope, lvalue) {
                Ok(value) => value,
                Err(flow) => return flow,
            },
            None => match read_target(runtime, access, scope, target) {
                Ok(value) => value,
                Err(flow) => return flow,
            },
        };
        let combined = if target.trim_start().starts_with('&') {
            apply_option_assignment_operator(runtime, previous, &value, operator)
        } else {
            apply_assignment_operator(runtime, previous, &value, operator)
        };
        match combined {
            Ok(value) => value,
            Err(flow) => return flow,
        }
    };
    let assigned_flow = match &bound {
        Some(lvalue) => assign_lvalue(runtime, access, scope, lvalue, assigned, constant),
        None => assign_target(runtime, access, scope, target, assigned, constant),
    };
    if let Err(flow) = assigned_flow {
        return flow;
    }
    if constant {
        runtime.const_vars.insert(key);
    }
    Flow::Normal
}

fn command_unlet<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    args: &str,
    bang: bool,
) -> Flow {
    // `ex_unletlock` (eval/vars.c 1587-1600) parses a `$` target with
    // `get_env_len` before it ever reaches the unset: an empty name is `E475`
    // naming the whole remaining argument, and a name followed by anything
    // other than white space or a command end is `E488`. Without this the
    // empty name reached `std::env::remove_var("")` and killed the process.
    let mut rest = args;
    while let Some(start) = rest.find(|character: char| !matches!(character, ' ' | '\t')) {
        rest = &rest[start..];
        let end = rest.find([' ', '\t']).unwrap_or(rest.len());
        let (target, tail) = rest.split_at(end);
        if let Some(name) = target.strip_prefix('$') {
            let length = env_name_len(name);
            if length == 0 {
                return error_flow(runtime, "E475", format!("Invalid argument: {rest}"));
            }
            if length < name.len() {
                return error_flow(
                    runtime,
                    "E488",
                    format!("Trailing characters: {}", &name[length..]),
                );
            }
        } else if let Some(garbage) = unlet_name_garbage(target) {
            return error_flow(runtime, "E488", format!("Trailing characters: {garbage}"));
        }
        let key = canonical_target(target);
        if runtime.const_vars.contains(&key) {
            return error_flow(
                runtime,
                "E46",
                format!("Cannot change read-only variable \"{target}\""),
            );
        }
        let (kind, name) = parse_scope_name(target);
        if let Some(kind) = kind
            && let Some((flags, subscripted)) = scoped_target_flags(kind, name.as_bytes())
        {
            if subscripted {
                // A subscript spelling resolves as a dict item
                // (`do_unlet_var`), whose read-only flag refuses with E46;
                // the fixed bit is not consulted on that path.
                if flags.intersects(DictEntryFlags::READ_ONLY) {
                    return error_flow(
                        runtime,
                        "E46",
                        format!("Cannot change read-only variable \"{target}\""),
                    );
                }
            } else if flags.contains(DictEntryFlags::FIXED) {
                // `do_unlet` (vars.c:1759): the fixed check comes first and
                // names the variable unquoted (E795); read-only follows.
                return error_flow(runtime, "E795", format!("Cannot delete variable {target}"));
            } else if flags.intersects(DictEntryFlags::READ_ONLY) {
                return error_flow(
                    runtime,
                    "E46",
                    format!("Cannot change read-only variable \"{target}\""),
                );
            }
        }
        let removed = if has_subscript(target) {
            // Subscripted targets unlet through the lvalue path, whose
            // dict-item resolution reports E46 for read-only entries.
            match parse_and_bind_lvalue(runtime, access, scope, None, target) {
                Ok(lvalue) => match remove_lvalue(runtime, access, scope, &lvalue, bang) {
                    Ok(removed) => removed,
                    Err(flow) => return flow,
                },
                Err(flow) => return flow,
            }
        } else {
            remove_target(scope, target)
        };
        if !removed && !bang {
            return error_flow(runtime, "E108", format!("No such variable: \"{target}\""));
        }
        rest = tail;
    }
    Flow::Normal
}

fn command_delfunction<F: FileIO>(runtime: &mut ExRuntime<F>, command: &ExCommand) -> Flow {
    let name = command.args.trim();
    if name.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    if name.split_whitespace().count() != 1 {
        return error_flow(runtime, "E488", "Trailing characters");
    }
    let sid = runtime.scripts.current_sid().unwrap_or(0);
    if runtime.functions.is_active(name, sid) {
        return error_flow(
            runtime,
            "E131",
            format!("Cannot delete function {name}: It is in use"),
        );
    }
    if runtime.functions.remove(name, sid) || command.bang {
        Flow::Normal
    } else {
        error_flow(runtime, "E130", format!("Unknown function: {name}"))
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SetLayer {
    Effective,
    Local,
    Global,
}

/// Assignment metadata from one successful `:set` write (`option.c`
/// `did_set_option`'s outcome narrowed to what the Ex layer needs to fire
/// `FileType` after the fact). `OptionStore` stays callback-free: the
/// command layer consumes this instead of a storage-side hook.
struct OptionAssignment {
    /// Canonical option name, including when the user typed an alias (`ft`).
    name: &'static str,
    /// Buffer the write landed on, when the option is buffer-scoped.
    buffer: Option<BufHandle>,
    /// The value committed to the store.
    value: OptionValue,
    /// The effective value before the write differed from `value`
    /// (upstream `os_value_changed`).
    changed: bool,
}

fn command_set<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: &str,
    layer: SetLayer,
) -> Flow {
    if args.trim().is_empty() || args.trim() == "all" {
        let all = args.trim() == "all";
        for metadata in OPTION_METADATA {
            if !all && access.with_ex_editor(|editor| option_is_default(editor, metadata.name)) {
                continue;
            }
            if let Some(text) =
                access.with_ex_editor(|editor| display_option(editor, metadata.name, layer))
            {
                access.with_ex_editor(|editor| push_info_text_message(editor, text));
            }
        }
        return Flow::Normal;
    }
    let mut touched_runtimepath = false;
    for raw in split_set_args(args) {
        let assignment = match set_one(access, scope, &raw, layer) {
            Ok(assignment) => assignment,
            Err((code, message)) => return error_flow(runtime, code, message),
        };
        touched_runtimepath |= set_arg_targets(&raw);
        // Upstream fires FileType after each committed 'filetype' write
        // (`option.c:4150-4156`), before the next argument is processed.
        // The assignment stays committed when a handler fails, and later
        // arguments of this `:set` are abandoned with its flow.
        if let Some(assignment) = assignment
            && assignment.name == "filetype"
        {
            let flow = fire_filetype_autocmd(runtime, access, scope, lua, &assignment);
            if !matches!(flow, Flow::Normal) {
                return flow;
            }
        }
    }
    if touched_runtimepath {
        access.with_ex_editor(|editor| sync_runtime_roots(runtime, editor));
    }
    Flow::Normal
}

/// Whether one `:set` argument names the 'runtimepath' option, so runtime
/// searches must be re-derived from its value.
fn set_arg_targets(raw: &str) -> bool {
    let name = raw
        .trim_end_matches(['?', '!'])
        .split(['=', '+', '-', '^'])
        .next()
        .unwrap_or_default();
    crate::option_metadata(name).is_some_and(|metadata| metadata.name == "runtimepath")
}

/// Re-derives the runtime search roots from the current 'runtimepath'
/// value (runtime.c `did_set_runtimepackpath` keeps searches glued to
/// `p_rtp`). An unset or empty value keeps the existing roots so
/// embedders that inject roots without seeding the option keep working.
fn sync_runtime_roots<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &Editor) {
    if let Ok(OptionValue::String(rtp)) = editor.options().get_global("runtimepath")
        && !rtp.is_empty()
    {
        runtime.scripts.set_runtime_roots_from_rtp(rtp);
    }
}

fn command_echo<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    args: &str,
) -> Flow {
    if let Ok(value) = eval_text(runtime, access, scope, lua, args) {
        access.with_ex_editor(|editor| {
            push_text_message(
                editor,
                typval_to_display(&value, false),
                name == "echoerr",
                name == "echomsg",
            );
        });
        return Flow::Normal;
    }
    let expressions = match ExprParser::new(args.as_bytes()).parse_many() {
        Ok(expressions) => expressions,
        Err(error) => return eval_error_flow(runtime, error),
    };
    let mut pieces = Vec::with_capacity(expressions.len());
    for expression in expressions {
        let value = match eval_text(
            runtime,
            access,
            scope,
            lua,
            &args[expression.span.start..expression.span.end],
        ) {
            Ok(value) => value,
            Err(flow) => return flow,
        };
        pieces.push(typval_to_display(&value, false));
    }
    let separator = if name == "echon" { "" } else { " " };
    let text = pieces.join(separator);
    access.with_ex_editor(|editor| {
        push_text_message(editor, text, name == "echoerr", name == "echomsg");
    });
    Flow::Normal
}
fn command_messages(editor: &mut Editor) -> Flow {
    let history = editor
        .messages()
        .iter()
        .filter(|message| message.history)
        .filter_map(|message| match &message.content {
            Object::String(text) => Some(text.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let output = if history.contains('\n') {
        format!("{history}\nPress ENTER or type command to continue")
    } else {
        history
    };
    push_info_text_message(editor, output);
    Flow::Normal
}

fn command_help<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let requested = command.args.trim();
    let topic = if requested.is_empty() {
        "help"
    } else {
        requested
    };
    let roots: Vec<PathBuf> = runtime
        .scripts
        .runtime_roots()
        .iter()
        .map(|root| root.path().to_path_buf())
        .collect();
    for root in roots {
        let doc = root.join("doc");
        let fallback = doc.join("help.txt");
        let (path, command) = if requested.is_empty() && runtime.scripts.io().exists(&fallback) {
            (fallback, String::new())
        } else {
            let tags = doc.join("tags");
            if !runtime.scripts.io().exists(&tags) {
                continue;
            }
            let Some(tags_name) = tags.to_str() else {
                continue;
            };
            let tag_matches = match crate::tags::lookup_search(
                runtime.scripts.io(),
                tags_name,
                topic,
                0,
                false,
                true,
            ) {
                Ok(matches) => matches,
                Err(("E426" | "E433", _)) => continue,
                Err((code, message)) => return error_flow(runtime, code, message),
            };
            let Some(matched) = tag_matches.first() else {
                continue;
            };
            let path = if matched.filename.is_absolute() {
                matched.filename.clone()
            } else {
                doc.join(&matched.filename)
            };
            (path, matched.cmd.clone())
        };
        let handle = match buffer_from_file(runtime, editor, &path) {
            Ok((handle, _)) => handle,
            Err(flow) => return flow,
        };
        let lines = match buffer_lines(editor, handle) {
            Ok(lines) => lines,
            Err(message) => return error_flow(runtime, "E149", message),
        };
        let target =
            crate::tags::cmd_target_from(&lines, &command, 0).map(|(position, _)| position);
        if let Err(flow) = open_tag_buffer(runtime, editor, handle, true, false, None, false) {
            return flow;
        }
        let _ = editor.options_mut().set_buffer(
            handle,
            "buftype",
            OptionValue::String("help".to_owned()),
        );
        let _ = editor
            .options_mut()
            .set_buffer(handle, "modifiable", OptionValue::Boolean(false));
        if let Some(target) = target
            && let Some(window) = editor.current_window()
            && let Err(error) = editor.set_window_cursor(window, target)
        {
            return error_flow(runtime, "E16", error.to_string());
        }
        return Flow::Normal;
    }
    error_flow(runtime, "E149", format!("Sorry, no help for {topic}"))
}

fn command_function_list<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let argument = command.args.trim();
    let pattern = if argument.is_empty() {
        None
    } else {
        let source = &argument[1..];
        let (pattern, trailing) = match take_delimited(argument, '/') {
            Some((pattern, trailing)) => (pattern, trailing.trim()),
            None => (source.to_owned(), ""),
        };
        if !trailing.is_empty() {
            return error_flow(runtime, "E488", "Trailing characters");
        }
        let compiled = match compile_regex(&pattern, Magic::Magic) {
            Ok(compiled) => compiled,
            Err(error) => return error_flow(runtime, "E54", error.to_string()),
        };
        Some(compiled)
    };

    for (name, function) in runtime.functions.iter() {
        if name.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            continue;
        }
        if pattern.as_ref().is_some_and(|compiled| {
            ox_regex::exec(compiled, &RegexText::new(name.clone())).is_none()
        }) {
            continue;
        }
        let required = function
            .args
            .len()
            .saturating_sub(function.default_args.len());
        let mut arguments = Vec::with_capacity(function.args.len() + usize::from(function.varargs));
        for (index, argument) in function.args.iter().enumerate() {
            if index < required {
                arguments.push(argument.clone());
            } else {
                arguments.push(format!(
                    "{argument} = {}",
                    function.default_args[index - required]
                ));
            }
        }
        if function.varargs {
            arguments.push("...".to_owned());
        }
        let mut signature = format!("function {name}({})", arguments.join(", "));
        if function.flags.contains(crate::UserFuncFlags::ABORT) {
            signature.push_str(" abort");
        }
        if function.flags.contains(crate::UserFuncFlags::RANGE) {
            signature.push_str(" range");
        }
        if function.flags.contains(crate::UserFuncFlags::DICT) {
            signature.push_str(" dict");
        }
        if function.flags.contains(crate::UserFuncFlags::CLOSURE) {
            signature.push_str(" closure");
        }
        push_text_message(editor, signature, false, false);
    }
    Flow::Normal
}

fn command_redir<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    command: &ExCommand,
) -> Flow {
    let argument = command.args.trim();
    if argument.eq_ignore_ascii_case("END") {
        let Some(redirection) = runtime.redirection.take() else {
            return Flow::Normal;
        };
        return finish_redirection(runtime, access, scope, redirection);
    }
    if runtime.redirection.is_some() {
        return error_flow(
            runtime,
            "E930",
            "Cannot use :redir while redirection is active",
        );
    }

    let target = if let Some(register) = argument.strip_prefix('@') {
        let mut chars = register.chars();
        let Some(written_name) = chars.next() else {
            return error_flow(runtime, "E474", "Invalid argument");
        };
        let suffix = chars.as_str();
        if !matches!(suffix, "" | ">" | ">>") || written_name == '_' {
            return error_flow(runtime, "E474", format!("Invalid argument: {argument}"));
        }
        let append = written_name.is_ascii_uppercase() || suffix == ">>";
        let name = written_name.to_ascii_lowercase();
        if access.with_ex_editor(|editor| editor.registers().get(name).is_err()) {
            return error_flow(runtime, "E474", format!("Invalid argument: {argument}"));
        }
        if !append {
            let empty = match RegisterContent::characterwise(&[]) {
                Ok(content) => content,
                Err(error) => return error_flow(runtime, "E354", error.to_string()),
            };
            if let Err(error) =
                access.with_ex_editor(|editor| editor.registers_mut().set(name, empty))
            {
                return error_flow(runtime, "E354", error.to_string());
            }
            scope.set_register(&[name as u8], Typval::String(OxStr::from("")));
        }
        RedirTarget::Register { name }
    } else if let Some(variable) = argument.strip_prefix("=>>").map(str::trim) {
        if variable.is_empty() || variable.starts_with(['@', '$', '&']) {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        match read_target(runtime, access, scope, variable) {
            Ok(Typval::String(_)) => {}
            Ok(_) => return error_flow(runtime, "E734", "Wrong variable type for .="),
            Err(flow) => return flow,
        }
        RedirTarget::Variable {
            name: variable.to_owned(),
            append: true,
        }
    } else if let Some(variable) = argument.strip_prefix("=>").map(str::trim) {
        if variable.is_empty() || variable.starts_with(['@', '$', '&']) {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        if let Err(flow) = assign_target(
            runtime,
            access,
            scope,
            variable,
            Typval::String(OxStr::from("")),
            false,
        ) {
            return flow;
        }
        RedirTarget::Variable {
            name: variable.to_owned(),
            append: false,
        }
    } else if let Some(file) = argument.strip_prefix(">>").map(str::trim) {
        if file.is_empty() {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        let path = PathBuf::from(expand_env_esc(file));
        if let Err(error) = runtime.scripts.io().write_bytes(&path, &[], true) {
            return error_flow(runtime, "E484", format!("{}: {error}", path.display()));
        }
        RedirTarget::File { path }
    } else if let Some(file) = argument.strip_prefix('>').map(str::trim) {
        if file.is_empty() {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        let path = PathBuf::from(expand_env_esc(file));
        if let Err(error) = runtime.scripts.io().write_bytes(&path, &[], false) {
            return error_flow(runtime, "E484", format!("{}: {error}", path.display()));
        }
        RedirTarget::File { path }
    } else {
        return error_flow(runtime, "E474", format!("Invalid argument: {argument}"));
    };

    runtime.redirection = Some(Redirection {
        target,
        output: String::new(),
        seen_messages: access.with_ex_editor(|editor| editor.messages().len()),
    });
    Flow::Normal
}

fn finish_redirection<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    redirection: Redirection,
) -> Flow {
    match redirection.target {
        RedirTarget::Register { .. } | RedirTarget::File { .. } => Flow::Normal,
        RedirTarget::Variable { name, append } => {
            let output = if append {
                match read_target(runtime, access, scope, &name) {
                    Ok(Typval::String(current)) => {
                        let mut value = current.to_string_lossy().into_owned();
                        value.push_str(&redirection.output);
                        value
                    }
                    Ok(_) => return error_flow(runtime, "E734", "Wrong variable type for .="),
                    Err(flow) => return flow,
                }
            } else {
                redirection.output
            };
            match assign_target(
                runtime,
                access,
                scope,
                &name,
                Typval::String(OxStr::from(output.as_str())),
                false,
            ) {
                Ok(()) => Flow::Normal,
                Err(flow) => flow,
            }
        }
    }
}

fn capture_command_messages<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    command: &ExCommand,
    command_start: usize,
) -> Result<(), Flow> {
    let silent = command
        .modifiers
        .iter()
        .any(|modifier| modifier.kind == ModifierKind::Silent);
    let mut write = None;
    if let Some(redirection) = runtime.redirection.as_mut() {
        let start = redirection
            .seen_messages
            .max(command_start)
            .min(editor.messages().len());
        let mut chunk = String::new();
        for (index, message) in editor.messages()[start..].iter().enumerate() {
            let Object::String(text) = &message.content else {
                continue;
            };
            if (!redirection.output.is_empty() || !chunk.is_empty())
                && (command.command.name() != "echon" || index > 0)
            {
                chunk.push('\n');
            }
            chunk.push_str(&text.to_string_lossy());
        }
        redirection.output.push_str(&chunk);
        redirection.seen_messages = editor.messages().len();
        if !chunk.is_empty() {
            write = Some((redirection.target.clone(), chunk));
        }
    }

    if let Some((target, chunk)) = write {
        match target {
            RedirTarget::Register { name } => {
                let mut bytes = editor
                    .registers()
                    .get(name)
                    .ok()
                    .flatten()
                    .map_or_else(Vec::new, RegisterContent::to_bytes);
                bytes.extend_from_slice(chunk.as_bytes());
                let content = RegisterContent::characterwise(&bytes)
                    .map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
                editor
                    .registers_mut()
                    .set(name, content)
                    .map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
                scope.set_register(&[name as u8], Typval::String(OxStr(bytes)));
            }
            RedirTarget::File { path } => runtime
                .scripts
                .io()
                .write_bytes(&path, chunk.as_bytes(), true)
                .map_err(|error| {
                    error_flow(runtime, "E484", format!("{}: {error}", path.display()))
                })?,
            RedirTarget::Variable { .. } => {}
        }
    }

    if silent {
        editor.truncate_messages(command_start);
        if let Some(redirection) = runtime.redirection.as_mut() {
            redirection.seen_messages = editor.messages().len();
        }
    }
    Ok(())
}

fn command_defer<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    if !runtime.can_add_defer() {
        return error_flow(runtime, "E193", "defer not inside a function");
    }
    let text = command.args.trim();
    let Some(arguments) = text
        .strip_prefix("delete(")
        .and_then(|arguments| arguments.strip_suffix(')'))
    else {
        return Flow::NotImplemented(format!("defer {text}"));
    };
    let parts = split_comma_args(arguments);
    if let Err(error) = crate::fs_builtins::check_arity("delete", parts.len()) {
        return error_flow(runtime, error.code, error.message);
    }
    let mut parts = parts.into_iter();
    let path = match eval_text(runtime, access, scope, lua, parts.next().unwrap_or("")) {
        Ok(value) => PathBuf::from(typval_to_text(&value)),
        Err(flow) => return flow,
    };
    let flags = match parts.next() {
        Some(expression) => match eval_text(runtime, access, scope, lua, expression) {
            Ok(value) => typval_to_text(&value),
            Err(flow) => return flow,
        },
        None => String::new(),
    };
    // Upstream `add_defer` stores the raw call and evaluates it (including
    // flag validation) at deferred execution time.  This port validates the
    // `delete()` flags here at registration, which changes the abort
    // semantics for invalid flags; the `E193` message has no leading colon.
    let mode = match crate::fs_builtins::DeleteMode::parse(&flags) {
        Ok(mode) => mode,
        Err(error) => return error_flow(runtime, error.code, error.message),
    };
    runtime.push_deferred_delete(path, mode);
    Flow::Normal
}
fn command_call<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let text = skipwhite_trim(&command.args);
    let Some(open) = text.find('(') else {
        return error_flow(runtime, "E107", "Missing parentheses: :call");
    };
    let Some(close) = text.rfind(')') else {
        return error_flow(runtime, "E107", "Missing parentheses: :call");
    };
    // ex_call: only text that `ends_excmd` rejects is trailing — a `"`
    // comment (with or without leading whitespace) ends the command.
    let trailing = text[close + 1..].trim_start_matches([' ', '\t']);
    if !trailing.is_empty() && !trailing.starts_with('"') {
        return error_flow(runtime, "E488", format!("Trailing characters: {trailing}"));
    }
    let name = text[..open].trim();
    let sid = runtime.scripts.current_sid().unwrap_or(0);
    // `:call` reaches builtins, funcref variables (`g:Xsetlist`), and
    // lambdas through the same evaluator `:let` uses (`call_func`). User
    // functions stay on the registry path below so `-range` still iterates.
    if !runtime.functions.contains(name, sid) && !name.contains('#') {
        let (first, last) = access
            .with_ex_editor(|editor| resolve_range(editor, command))
            .unwrap_or_else(|_| access.with_ex_editor(|editor| current_line_pair(editor)));
        let addressed = if command.range.is_none() {
            first..=first
        } else {
            first..=last
        };
        for lnum in addressed {
            if command.range.is_some()
                && let Some(window) = access.with_ex_editor(|editor| editor.current_window())
                && let Err(error) = access.with_ex_editor(|editor| {
                    editor.set_window_cursor(window, Position { lnum, col: 0 })
                })
            {
                return error_flow(runtime, "E16", error.to_string());
            }
            if let Err(flow) = eval_text(runtime, access, scope, lua, &text[..=close]) {
                return flow;
            }
        }
        return Flow::Normal;
    }
    let mut values = Vec::new();
    for arg in split_comma_args(&text[open + 1..close]) {
        if arg.trim().is_empty() {
            continue;
        }
        match eval_text(runtime, access, scope, lua, arg) {
            Ok(value) => values.push(value),
            Err(flow) => return flow,
        }
    }
    let (first, last) = access
        .with_ex_editor(|editor| resolve_range(editor, command))
        .unwrap_or_else(|_| access.with_ex_editor(|editor| current_line_pair(editor)));
    let accepts_range = runtime
        .functions
        .get(name, sid)
        .is_some_and(|function| function.flags.contains(crate::UserFuncFlags::RANGE));
    if command.range.is_none() || accepts_range {
        return match call_user_function(runtime, access, scope, lua, name, values, first, last) {
            Ok(_) => Flow::Normal,
            Err(flow) => flow,
        };
    }

    for lnum in first..=last {
        if let Some(window) = access.with_ex_editor(|editor| editor.current_window())
            && let Err(error) = access.with_ex_editor(|editor| {
                editor.set_window_cursor(window, Position { lnum, col: 0 })
            })
        {
            return error_flow(runtime, "E16", error.to_string());
        }
        if let Err(flow) = call_user_function(
            runtime,
            access,
            scope,
            lua,
            name,
            values.clone(),
            lnum,
            lnum,
        ) {
            return flow;
        }
    }
    Flow::Normal
}

fn command_execute<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: &str,
) -> Flow {
    let expressions = match ExprParser::new(args.as_bytes()).parse_many() {
        Ok(expressions) => expressions,
        Err(error) => return eval_error_flow(runtime, error),
    };
    let mut pieces = Vec::with_capacity(expressions.len());
    for expression in expressions {
        match eval_text(
            runtime,
            access,
            scope,
            lua,
            &args[expression.span.start..expression.span.end],
        ) {
            Ok(value) => match value {
                Typval::List(_) => return error_flow(runtime, "E730", "Using a List as a String"),
                Typval::Dict(_) => {
                    return error_flow(runtime, "E731", "Using a Dictionary as a String");
                }
                Typval::Blob(_) => return error_flow(runtime, "E976", "Using a Blob as a String"),
                Typval::Funcref(_) | Typval::Partial(_) => {
                    return error_flow(runtime, "E729", "Using a Funcref as a String");
                }
                _ => pieces.push(typval_to_text(&value)),
            },
            Err(flow) => return flow,
        }
    }
    let line = pieces.join(" ");
    let logical = vec![LogicalLine {
        text: line,
        first_line: runtime.scripts.current_line(),
    }];
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    run_program(runtime, access, scope, lua, &program, 0, program.len())
}

fn command_cd<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &mut Editor,
    args: &str,
    scope: DirectoryScope,
) -> Flow {
    let path = args.trim();
    if path.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    match change_directory(editor, path, scope) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

fn directory_target(
    editor: &Editor,
    path: &str,
    scope: DirectoryScope,
) -> ox_eval::Result<PathBuf> {
    if path == "-" {
        return editor
            .previous_directory(scope)
            .ok_or_else(|| EvalError::new("E186", 0, "No previous directory"));
    }
    let direct = PathBuf::from(path);
    if direct.is_absolute() || direct.is_dir() {
        return Ok(direct);
    }
    if let Ok(OptionValue::String(cdpath)) = editor.options().get_global("cdpath") {
        for entry in cdpath.split(',') {
            let base = if entry.is_empty() {
                Path::new(".")
            } else {
                Path::new(entry)
            };
            let candidate = base.join(path);
            if candidate.is_dir() {
                return Ok(candidate);
            }
        }
    }
    Ok(direct)
}

/// `:cd`/`:lcd` and `chdir()`/`haslocaldir()`'s shared move
/// (`changedir_func`, `ex_docmd.c`:6290-6340).
///
/// Upstream remembers the directory it is leaving with `os_dirname` and, when
/// that fails, records no previous directory at all: `dir_differs` treats a
/// missing one as "differs" and the move still happens. Reading the old
/// directory must therefore never be able to refuse the move -- otherwise a
/// script that deleted its own working directory can no longer `:cd` back out
/// of it, and every later relative path resolves against a directory that is
/// gone. That is how `test_alot.vim` and `test_expand.vim` lost every result
/// they had collected: `runtest.vim`'s `exe 'cd ' . save_cwd` was refused, and
/// `FinishTesting`'s write of the relative `test.log` died with E212.
///
/// Returns the directory left behind, empty when it could not be read, which
/// is what `f_chdir` returns in that case (`eval/funcs.c`).
pub(crate) fn change_directory(
    editor: &mut Editor,
    path: &str,
    scope: DirectoryScope,
) -> ox_eval::Result<PathBuf> {
    let target = directory_target(editor, path, scope)?;
    editor
        .change_directory(&target, scope)
        .map_err(|error| match error {
            DirectoryError::NoCurrentWindow => EvalError::new("E16", 0, "No current window"),
            DirectoryError::ChangeFailed { error, .. } => {
                EvalError::new("E344", 0, format!("Can't find directory {path}: {error}"))
            }
        })
}

/// `:normal[!] {commands}` (`ex_normal`, `ex_docmd.c:7133-7210`).
///
/// The argument is *stuffed into the typeahead buffer* and then consumed by
/// the normal-mode loop (`exec_normal_cmd`, `ex_docmd.c:7263-7268`); it is not
/// fed straight to the mode machine. That is the whole reason `:normal` obeys
/// mappings while `:normal!` does not — `ins_typebuf`'s `remap` argument is
/// `REMAP_NONE` for the bang and `REMAP_YES` otherwise, and nothing else in
/// the two paths differs. Feeding the keys past typeahead, as this used to,
/// skips mapping lookup entirely: `nnoremap ,x :cmd<CR>` then `:normal ,x` ran
/// `,` and `x` as literal normal-mode keys.
///
/// The keys are inserted as *not typed*, so they do not close an undo block
/// (`may_sync_undo`); one `:normal` is one undo step.
fn command_normal<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let keys = Keys::from(command.args.trim_start());
    if keys.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let range = match &command.range {
        None => None,
        Some(_) => match access.with_ex_editor(|editor| resolve_range(editor, command)) {
            Ok(range) => Some(range),
            Err(message) => return error_flow(runtime, "E16", message),
        },
    };
    // `save_typeahead`/`restore_typeahead` (`ex_docmd.c:7096,7103`): whatever
    // was already queued must not be consumed by this command, and must still
    // be there afterwards.
    let saved = access.with_ex_editor(|editor| std::mem::take(editor.typeahead_mut()));
    let flow = run_normal_keys(runtime, access, scope, lua, &keys, command.bang, range);
    access.with_ex_editor(|editor| *editor.typeahead_mut() = saved);
    flow
}

/// `ex_normal`'s per-line loop: with a range the argument runs once for each
/// addressed line, from column zero (`ex_docmd.c:7189-7198`).
fn run_normal_keys<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    keys: &Keys,
    bang: bool,
    range: Option<(usize, usize)>,
) -> Flow {
    // Use the host's live mode machine when installed so mode changes persist
    // (e.g. `:normal! v` leaves Visual mode active). Fall back to a temporary
    // machine for unit tests that do not install one.
    let machine = runtime
        .mode_machine
        .clone()
        .unwrap_or_else(|| Rc::new(RefCell::new(ModeMachine::default())));
    // `:normal` is a closed drain: no further keys can arrive, so an
    // incomplete mapping must resolve like a timeout. Save and restore the
    // flag so a `:normal` nested inside the interactive loop does not clobber
    // the host's `no_more_input = false`.
    let saved_no_more_input = machine.borrow().no_more_input();
    machine.borrow_mut().set_no_more_input(true);
    let flow = {
        let (first, last) = range.unwrap_or((0, 0));
        let mut lnum = first;
        loop {
            if range.is_some()
                && let Some(window) = access.with_ex_editor(|editor| editor.current_window())
                && let Err(error) = access.with_ex_editor(|editor| {
                    editor.set_window_cursor(window, Position { lnum, col: 0 })
                })
            {
                break error_flow(runtime, "E16", error.to_string());
            }
            let flags = mapped_flags(
                access.with_ex_editor(|editor| editor.current_buffer()),
                !bang,
                MapModes::ALL,
            );
            if let Err(error) =
                access.with_ex_editor(|editor| editor.typeahead_mut().push(keys, 0, flags))
            {
                break error_flow(runtime, "E523", error.to_string());
            }
            let flow = drain_typeahead(runtime, access, scope, lua, &machine);
            if !matches!(flow, Flow::Normal) {
                break flow;
            }
            // `vgetorpeek`'s `ex_normal_busy` escape (`getchar.c`): a mode that is
            // still asking for a character with the typeahead empty gets ESC, so
            // an argument ending half-way through an insert or a command line
            // cannot hang. Only Insert and Cmdline ask; `exec_normal`'s loop just
            // stops for the others, which is why `:normal v` leaves a selection
            // active. The ESC is returned directly there, never remapped.
            if matches!(machine.borrow().mode(), Mode::Insert(_) | Mode::Cmdline(_)) {
                let escape = Keys::from("\u{1b}");
                let flags = mapped_flags(
                    access.with_ex_editor(|editor| editor.current_buffer()),
                    false,
                    MapModes::ALL,
                );
                if let Err(error) =
                    access.with_ex_editor(|editor| editor.typeahead_mut().push(&escape, 0, flags))
                {
                    break error_flow(runtime, "E523", error.to_string());
                }
                let flow = drain_typeahead(runtime, access, scope, lua, &machine);
                if !matches!(flow, Flow::Normal) {
                    break flow;
                }
            }
            if range.is_none() || lnum >= last {
                break Flow::Normal;
            }
            lnum += 1;
        }
    };
    machine.borrow_mut().set_no_more_input(saved_no_more_input);
    flow
}

#[expect(
    clippy::too_many_lines,
    reason = "global execution must keep mark lifetime and nested command ordering in one transaction"
)]
fn command_global<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
    invert: bool,
) -> Flow {
    let args = command.args.trim_start();
    let Some(delimiter) = args.chars().next() else {
        return error_flow(runtime, "E148", "Regular expression missing from global");
    };
    let Some((pattern, rest)) = take_delimited(args, delimiter) else {
        return error_flow(runtime, "E682", "Invalid search pattern or delimiter");
    };
    let nested = if rest.trim().is_empty() {
        "print"
    } else {
        rest.trim()
    };
    let (start, end) = match access.with_ex_editor(|editor| resolve_range(editor, command)) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let program_regex = match compile_regex(&pattern, Magic::Magic) {
        Ok(program) => program,
        Err(error) => return error_flow(runtime, "E54", error.to_string()),
    };
    let lines = match access.with_ex_editor(|editor| buffer_lines(editor, buffer)) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let mut marked = Vec::new();
    let namespace = match access.with_ex_editor(|editor| {
        editor.buffer_mut(buffer).and_then(|state| {
            state
                .extmarks
                .create_namespace("")
                .map_err(|error| crate::EditorError::Buffer(error.into()))
        })
    }) {
        Ok(namespace) => namespace,
        Err(error) => return error_flow(runtime, "E16", error.to_string()),
    };
    for lnum in start..=end.min(lines.len()) {
        let text = String::from_utf8_lossy(&lines[lnum - 1]).into_owned();
        let matched = ox_regex::exec(&program_regex, &RegexText::new(text)).is_some();
        if matched != invert {
            let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(lnum - 1, 0))
                .with_end(ExtmarkPosition::new(lnum, 0));
            placement
                .attributes
                .flags
                .set(crate::ExtmarkFlags::INVALIDATE, true);
            let id = match access.with_ex_editor(|editor| {
                editor.buffer_mut(buffer).and_then(|state| {
                    state
                        .extmarks
                        .set(namespace, None, placement)
                        .map_err(|error| crate::EditorError::Buffer(error.into()))
                })
            }) {
                Ok(id) => id,
                Err(error) => {
                    access.with_ex_editor(|editor| {
                        cleanup_global_marks(editor, buffer, namespace, &marked);
                    });
                    return error_flow(runtime, "E16", error.to_string());
                }
            };
            marked.push(id);
        }
    }
    for id in marked.iter().copied() {
        let target = match access.with_ex_editor(|editor| {
            editor.buffer(buffer).and_then(|state| {
                state
                    .extmarks
                    .get(namespace, id)
                    .map(|mark| {
                        mark.filter(|mark| !mark.invalid)
                            .map(|mark| mark.placement.position.row + 1)
                    })
                    .map_err(|error| crate::EditorError::Buffer(error.into()))
            })
        }) {
            Ok(target) => target,
            Err(error) => {
                access.with_ex_editor(|editor| {
                    cleanup_global_marks(editor, buffer, namespace, &marked);
                });
                return error_flow(runtime, "E16", error.to_string());
            }
        };
        access.with_ex_editor(|editor| {
            if let Ok(state) = editor.buffer_mut(buffer) {
                let _ = state.extmarks.delete(namespace, id);
            }
        });
        let Some(lnum) = target else { continue };
        if let Some(window) = access.with_ex_editor(|editor| editor.current_window())
            && let Err(error) = access.with_ex_editor(|editor| {
                editor.set_window_cursor(window, Position { lnum, col: 0 })
            })
        {
            access
                .with_ex_editor(|editor| cleanup_global_marks(editor, buffer, namespace, &marked));
            return error_flow(runtime, "E16", error.to_string());
        }
        let logical = vec![LogicalLine {
            text: nested.to_owned(),
            first_line: runtime.scripts.current_line(),
        }];
        let program = parse_program(
            &runtime.user_commands,
            access.with_ex_editor(|editor| editor.current_buffer()),
            &logical,
        );
        let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
        if !matches!(flow, Flow::Normal) {
            access
                .with_ex_editor(|editor| cleanup_global_marks(editor, buffer, namespace, &marked));
            return flow;
        }
    }
    access.with_ex_editor(|editor| cleanup_global_marks(editor, buffer, namespace, &marked));
    Flow::Normal
}

fn cleanup_global_marks(
    editor: &mut Editor,
    buffer: BufHandle,
    namespace: NamespaceId,
    marked: &[ExtmarkId],
) {
    if let Ok(state) = editor.buffer_mut(buffer) {
        for id in marked {
            let _ = state.extmarks.delete(namespace, *id);
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "substitution keeps regex, expression, and buffer-edit error ordering in one transaction"
)]
fn command_substitute<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    command: &ExCommand,
) -> Flow {
    let args = command.args.trim_start();
    let Some(delimiter) = args.chars().next() else {
        return error_flow(runtime, "E33", "No previous substitute regular expression");
    };
    let Some((pattern, tail)) = take_delimited(args, delimiter) else {
        return error_flow(runtime, "E488", "Trailing characters");
    };
    let replacement_input = format!("{delimiter}{tail}");
    // A missing closing delimiter is valid: the replacement is the rest of
    // the line (`ex_cmds.c` do_sub parsing). The check_col functional case
    // is `:1,5s:5\n:5 ` with no third delimiter.
    let (replacement, flags) =
        take_delimited(&replacement_input, delimiter).unwrap_or_else(|| (tail.to_owned(), ""));
    let flags = flags.trim();
    if flags.contains('c') {
        return error_flow(
            runtime,
            "E999",
            "Substitution confirmation is not supported without an interactive UI",
        );
    }
    let global = flags.contains('g');
    let suppress_nomatch = flags.contains('e');
    let mut ignore_case = false;
    let mut match_case = false;
    for flag in flags.chars() {
        if flag == 'i' {
            ignore_case = true;
        } else if flag == 'I' {
            match_case = true;
        }
    }
    let expression = replacement.strip_prefix("\\=");
    let compiled_pattern = if match_case {
        format!("\\C{pattern}")
    } else if ignore_case {
        format!("\\c{pattern}")
    } else {
        pattern.clone()
    };
    let (start, end) = match access.with_ex_editor(|editor| resolve_range(editor, command)) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let original = match access.with_ex_editor(|editor| buffer_lines(editor, buffer)) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let program_regex = match compile_regex(&compiled_pattern, Magic::Magic) {
        Ok(program) => program,
        Err(error) => return error_flow(runtime, "E54", error.to_string()),
    };
    let mut substitutions = Vec::new();
    let mut changed = false;
    if pattern.contains("\\n") {
        // `\n` in the pattern matches across line boundaries, so the range is
        // joined into one source (`do_sub`, ex_cmds.c) and every match is
        // translated back to byte-precise row/col coordinates.
        let last = end.min(original.len());
        let source = original[start - 1..last]
            .iter()
            .map(|line| String::from_utf8_lossy(line))
            .collect::<Vec<_>>()
            .join("\n");
        match substitute_line(
            runtime,
            access,
            scope,
            &program_regex,
            &source,
            &replacement,
            expression,
            global,
        ) {
            Ok(edits) => {
                changed |= !edits.is_empty();
                for (from, to, rendered) in edits {
                    let (start_offset, start_col) = byte_row_col(&source, from);
                    let (end_offset, end_col) = byte_row_col(&source, to);
                    substitutions.push((
                        start - 1 + start_offset,
                        start_col,
                        start - 1 + end_offset,
                        end_col,
                        rendered,
                    ));
                }
            }
            Err(flow) => return flow,
        }
    } else {
        for lnum in start..=end.min(original.len()) {
            let source = String::from_utf8_lossy(&original[lnum - 1]).into_owned();
            match substitute_line(
                runtime,
                access,
                scope,
                &program_regex,
                &source,
                &replacement,
                expression,
                global,
            ) {
                Ok(edits) => {
                    changed |= !edits.is_empty();
                    substitutions.extend(
                        edits
                            .into_iter()
                            .map(|(from, to, rendered)| (lnum - 1, from, lnum - 1, to, rendered)),
                    );
                }
                Err(flow) => return flow,
            }
        }
    }
    if !changed && !suppress_nomatch {
        return error_flow(runtime, "E486", format!("Pattern not found: {pattern}"));
    }
    if changed {
        let cursor = access.with_ex_editor(|editor| {
            editor
                .current_window()
                .and_then(|window| editor.window(window).ok())
                .map_or(
                    Position {
                        lnum: start,
                        col: 0,
                    },
                    |window| window.cursor,
                )
        });
        // Apply bottom-up so earlier (row, col) coordinates stay in pre-edit
        // space; each splice records exact byte extents so extmark endpoints
        // and undo replay the same geometry.
        for (start_row, from, end_row, to, rendered) in substitutions.into_iter().rev() {
            let parts = rendered
                .as_bytes()
                .split(|byte| matches!(*byte, b'\n' | b'\r'))
                .map(<[u8]>::to_vec)
                .collect::<Vec<_>>();
            let request = BufferTextEditRequest {
                start: ExtmarkPosition::new(start_row, from),
                end: ExtmarkPosition::new(end_row, to),
                replacement: parts,
            };
            if let Err(error) = access.with_ex_editor(|editor| {
                editor.replace_buffer_text(buffer, &request, cursor, cursor, 0)
            }) {
                return error_flow(runtime, "E16", error.to_string());
            }
        }
    }
    Flow::Normal
}

/// Translates a byte offset in a `\n`-joined substitute source into a
/// zero-based (row, column) pair.
fn byte_row_col(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source.as_bytes()[..byte.min(source.len())];
    let row = prefix.split(|byte| *byte == b'\n').count() - 1;
    let col = prefix
        .iter()
        .rposition(|value| *value == b'\n')
        .map_or(prefix.len(), |newline| prefix.len() - newline - 1);
    (row, col)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one substitution pass requires the complete match and evaluation context"
)]
fn substitute_line<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    program: &ox_regex::Prog,
    source: &str,
    replacement: &str,
    expression: Option<&str>,
    global: bool,
) -> Result<Vec<(usize, usize, String)>, Flow> {
    let text = RegexText::new(source.to_owned());
    let mut edits = Vec::new();
    let mut cursor = 0;
    while cursor <= source.len() {
        let Some(position) = text.position(cursor) else {
            break;
        };
        let Some(matched) = regex_exec_at(program, &text, position) else {
            break;
        };
        let mut groups = vec![source[matched.start.byte..matched.end.byte].to_owned()];
        for capture in &matched.captures {
            groups.push(capture.as_ref().map_or_else(String::new, |capture| {
                source[capture.start.byte..capture.end.byte].to_owned()
            }));
        }
        let rendered = if let Some(expression) = expression {
            eval_substitute_expression(runtime, access, scope, expression, groups)?
        } else {
            expand_replacement(replacement, &groups)
        };
        edits.push((matched.start.byte, matched.end.byte, rendered));
        cursor = if !global {
            // One substitution per line: skip to the line after the match.
            source[matched.end.byte..]
                .find('\n')
                .map_or(source.len().saturating_add(1), |newline| {
                    matched.end.byte.saturating_add(newline).saturating_add(1)
                })
        } else if matched.start.byte == matched.end.byte {
            next_boundary(source, matched.end.byte)
        } else {
            matched.end.byte
        };
        if cursor > source.len() {
            break;
        }
    }
    Ok(edits)
}

fn eval_substitute_expression<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    expression: &str,
    groups: Vec<String>,
) -> Result<String, Flow> {
    let parsed = ExprParser::new(expression.as_bytes())
        .parse()
        .map_err(|error| eval_error_flow(runtime, error))?;
    let regex = VimRegex;
    let mut host = EvalHost {
        runtime,
        access,
        lua: None,
        builtins: Builtins::new(&regex),
        submatches: Some(groups),
        escaped_exception: None,
    };
    Evaluator::new(&mut host, &regex)
        .eval(&parsed, scope)
        .map(|value| typval_to_text(&value))
        .map_err(|error| eval_error_flow(host.runtime, error))
}

/// The tabpage geometry every command that has to create one uses.
///
/// This port has no screen model to ask, so the size is fixed. It is one
/// constant rather than a literal repeated per command arm.
const DEFAULT_TABPAGE_GEOMETRY: crate::Geometry = crate::Geometry {
    row: 0,
    col: 0,
    width: 80,
    height: 24,
};

/// Loads `path` into a fresh listed buffer named after it, saved-clean.
///
/// A missing file is not an error: upstream's `:edit`/`:split`/`:tabedit` open
/// an empty buffer for a name that does not exist yet. Shared by every command
/// that opens a file into a new buffer so the read, the name, and the
/// saved-stateking have one owner.
/// Returns the buffer's handle and whether this call created it (rather than
/// reusing the buffer already named for the path).
fn buffer_from_file<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    path: &std::path::Path,
) -> Result<(BufHandle, bool), Flow> {
    if let Some(existing) = existing_buffer_for_path(editor, path) {
        return Ok((existing, false));
    }

    let text = match runtime.scripts.io().read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error_flow(
                runtime,
                "E484",
                format!("Can't open file {}: {error}", path.display()),
            ));
        }
    };
    let buffer_text = match Buffer::from_bytes(text.as_bytes()) {
        Ok(buffer) => buffer,
        Err(error) => return Err(error_flow(runtime, "E474", error.to_string())),
    };
    let handle = match editor.create_buffer_with(buffer_text, true) {
        Ok(handle) => handle,
        Err(error) => return Err(error_flow(runtime, "E948", error.to_string())),
    };
    if let Ok(buffer) = editor.buffer_mut(handle) {
        buffer.set_name(OxStr::from(path.to_string_lossy().as_ref()));
        buffer.mark_saved();
    }
    Ok((handle, true))
}
fn existing_buffer_for_path(editor: &Editor, path: &std::path::Path) -> Option<BufHandle> {
    let wanted = path.to_string_lossy();
    let wanted_canon = std::fs::canonicalize(path).ok();
    editor.buffers().into_iter().find(|handle| {
        editor.buffer(*handle).is_ok_and(|state| {
            let name = state.name().to_string_lossy();
            if name == wanted {
                return true;
            }

            let named = std::path::Path::new(name.as_ref());
            if named.file_name() == path.file_name()
                && named.file_name().is_some()
                && let (Some(left), Some(right)) =
                    (std::fs::canonicalize(named).ok(), wanted_canon.as_ref())
                && &left == right
            {
                return true;
            }
            false
        })
    })
}

/// `:runtime[!] {pat}…` (`runtime.c` `ex_runtime`): source files matching
/// each pattern under 'runtimepath'. Without the bang only the first match
/// per pattern is sourced, with it every match, per 'runtimepath' entry in
/// order. `where` (`n`/`r`/`nr`) is not honored: no caller in the shipped
/// runtime uses it.
fn command_runtime<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let raw = command.args.trim();
    if raw.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let rtp = access.with_ex_editor(|editor| match editor.options().get_global("runtimepath") {
        Ok(OptionValue::String(text)) => text.clone(),
        _ => String::new(),
    });
    let roots: Vec<PathBuf> = rtp
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect();
    for pattern in raw.split_whitespace() {
        let alternatives =
            crate::autocmd::expand_braces(pattern).unwrap_or_else(|| vec![pattern.to_owned()]);
        let mut matches: Vec<PathBuf> = Vec::new();
        for root in &roots {
            for alternative in &alternatives {
                let joined = root.join(alternative).to_string_lossy().into_owned();
                matches.extend(
                    crate::fs_builtins::expand_glob(runtime.scripts.io(), &joined, false)
                        .into_iter()
                        .map(PathBuf::from),
                );
            }
            if !command.bang && !matches.is_empty() {
                break;
            }
        }
        for file in matches {
            match source_path(runtime, access, scope, lua, &file, true) {
                Ok(Flow::Finish) => return Flow::Normal,
                Ok(flow) if !matches!(flow, Flow::Normal) => return flow,
                Ok(_) => {}
                Err(error) => return exec_error_flow(runtime, error),
            }
        }
    }
    Flow::Normal
}
/// `:iabbrev {lhs} {rhs}` (`ex_cmds.lua` `abbreviate`, insert-mode): define
/// one insert-mode abbreviation. The other `:abbreviate` mode prefixes and
/// `:unabbreviate` remain unrouted until the abbreviation command family
/// gets its full parser.
fn command_iabbrev<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    command: &ExCommand,
) -> Flow {
    let args = command.args.trim();
    let Some((lhs, rhs)) = args.split_once(char::is_whitespace) else {
        return error_flow(runtime, "E471", "Argument required");
    };
    let (lhs, rhs) = (lhs.trim(), rhs.trim());
    let action = match MappingAction::parse_rhs(
        rhs,
        &map_leader(scope, "mapleader"),
        &map_leader(scope, "maplocalleader"),
    ) {
        Ok(action) => action,
        Err(error) => return error_flow(runtime, "E224", error.to_string()),
    };
    let defined = access.with_ex_editor(|editor| {
        editor
            .mappings_mut()
            .abbreviate(lhs, action, MapScope::Global, !command.bang)
            .is_ok()
    });
    if defined {
        Flow::Normal
    } else {
        error_flow(runtime, "E736", "Invalid abbreviation")
    }
}
/// `:packadd[!] {name}` (`runtime.c` `ex_packadd`): search `'packpath'` for
/// `pack/*/{start,opt}/{name}`, insert each found directory into
/// `'runtimepath'` before its first `after` entry (skipped when already
/// present), and — unless the bang says insert-only — source
/// `plugin/**/*.vim` and `plugin/**/*.lua` recursively, then the pack's
/// `ftdetect/` files. `start` is only searched while no startup package load
/// has happened (this port never does one). Nothing found in either tree is
/// `E919` naming the `opt` pattern, upstream's `DIP_ERR` shape.
fn command_packadd<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let name = command.args.trim();
    if name.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let packpath = access.with_ex_editor(|editor| match editor.options().get_global("packpath") {
        Ok(OptionValue::String(text)) => text.clone(),
        _ => "./".to_owned(),
    });
    let io = runtime.scripts.io();
    let search = |kind: &str| -> Vec<PathBuf> {
        let mut found = Vec::new();
        for base in packpath.split(',') {
            let base = base.trim();
            if base.is_empty() {
                continue;
            }
            let pattern = format!("{base}/pack/*/{kind}/{name}");
            found.extend(
                crate::fs_builtins::expand_glob(io, &pattern, false)
                    .into_iter()
                    .map(PathBuf::from)
                    .filter(|path| path.is_dir()),
            );
        }
        found
    };
    // Start packs load only before any startup package load (ex_packadd's
    // `did_source_packages` guard); an opt hit after a start hit suppresses
    // the E919 that a lone failed opt search raises.
    let start = search("start");
    let opt = search("opt");
    if start.is_empty() && opt.is_empty() {
        return error_flow(
            runtime,
            "E919",
            format!("Directory not found in 'packpath': \"pack/*/opt/{name}\""),
        );
    }
    for directory in start.iter().chain(opt.iter()) {
        let inserted = access.with_ex_editor(|editor| {
            let current = match editor.options().get_global("runtimepath") {
                Ok(OptionValue::String(text)) => text.clone(),
                _ => String::new(),
            };
            let entry = directory.to_string_lossy().into_owned();
            if current.split(',').any(|existing| existing == entry) {
                return false;
            }
            // `add_pack_dir_to_rtp` (runtime.c:1032-1195): the pack dir goes
            // before the first `after` entry (appended at the end when there
            // is none), and the pack's own `after/` directory follows it —
            // before the pre-existing `after` entries when the pack has one,
            // appended at the very end otherwise.
            let is_after = |component: &str| component.split('/').any(|part| part == "after");
            let entries: Vec<String> = current
                .split(',')
                .filter(|existing| !existing.is_empty())
                .map(str::to_owned)
                .collect();
            let position = entries
                .iter()
                .position(|existing| is_after(existing))
                .unwrap_or(entries.len());
            let mut updated: Vec<String> = Vec::with_capacity(entries.len() + 2);
            updated.extend_from_slice(&entries[..position]);
            updated.push(entry.clone());
            let after_dir = directory.join("after");
            if after_dir.is_dir() {
                updated.push(after_dir.to_string_lossy().into_owned());
            }
            updated.extend_from_slice(&entries[position..]);
            editor
                .options_mut()
                .set_global("runtimepath", OptionValue::String(updated.join(",")))
                .is_ok()
        });
        if inserted {
            access.with_ex_editor(|editor| sync_runtime_roots(runtime, editor));
        }
        if command.bang {
            continue;
        }
        let mut plugin_files = Vec::new();
        for pattern in ["plugin/**/*.vim", "plugin/**/*.lua"] {
            let joined = directory.join(pattern).to_string_lossy().into_owned();
            plugin_files.extend(
                crate::fs_builtins::expand_glob(runtime.scripts.io(), &joined, false)
                    .into_iter()
                    .map(PathBuf::from),
            );
        }
        plugin_files.sort();
        for file in plugin_files {
            match source_path(runtime, access, scope, lua, &file, true) {
                Ok(Flow::Finish) => return Flow::Normal,
                Ok(flow) if !matches!(flow, Flow::Normal) => return flow,
                Ok(_) => {}
                Err(error) => return exec_error_flow(runtime, error),
            }
        }
        let ftdetect = directory.join("ftdetect");
        if ftdetect.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&ftdetect)
                .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
                .unwrap_or_default();
            files.sort();
            for file in files {
                match source_path(runtime, access, scope, lua, &file, true) {
                    Ok(Flow::Finish) => return Flow::Normal,
                    Ok(flow) if !matches!(flow, Flow::Normal) => return flow,
                    Ok(_) => {}
                    Err(error) => return exec_error_flow(runtime, error),
                }
            }
        }
    }
    Flow::Normal
}

/// `:clearjumps` (`ex_clearjumps`, mark.c 1107-1112): the jump list is
/// emptied and the navigation index reset.
fn command_clearjumps(editor: &mut Editor) -> Flow {
    editor.clear_jumplist();
    Flow::Normal
}

/// `:argadd[!] {name}...` (`ex_argadd`, arglist.c 750-756): the names are
/// inserted after the current entry, or after the addressed entry when a
/// count is given — `after = eap->line2`, so `:1argadd` on "a b c" yields
/// "a new b c" (`:h :argadd`). The bang is accepted and ignored: upstream
/// passes `will_edit = false` unconditionally. With no names the command
/// is a no-op here; upstream would add the current buffer's name instead
/// (arglist.c 417-424).
fn command_argadd<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    // `do_arglist` substitutes the current buffer's name for an empty
    // argument list (arglist.c:417-424); an unnamed buffer makes it fail,
    // which `ex_argadd` turns into a silent no-op.
    let mut current_name = String::new();
    let list = if command.args.trim().is_empty() {
        current_name = editor
            .current_buffer()
            .and_then(|buffer| editor.buffer(buffer).ok())
            .map(|buffer| buffer.name().to_string_lossy().into_owned())
            .unwrap_or_default();
        if current_name.is_empty() {
            return Flow::Normal;
        }
        current_name.as_str()
    } else {
        command.args.trim()
    };
    let after = if command.range.is_some() {
        match resolve_range_raw(editor, command) {
            Ok((_, end)) => end,
            Err(message) => return error_flow(runtime, "E16", message),
        }
    } else {
        editor.arglist().index() + 1
    };
    // `do_arglist` appends the substituted current-buffer name whole
    // (arg_escaped=false, arglist.c:265-275): whitespace in a buffer name
    // never splits it. Explicit command arguments go through the normal
    // whitespace split.
    let substituted = command.args.trim().is_empty();
    let mut names = Vec::new();
    for name in if substituted {
        vec![std::mem::take(&mut current_name)]
    } else {
        crate::arglist::split_file_list(list)
    } {
        // expand_wildcards with EW_NOTFOUND (arglist.c 432): wildcard
        // patterns expand to their sorted matches, and a pattern without
        // matches stays as the literal name.
        let matches = crate::fs_builtins::expand_glob(runtime.scripts.io(), &name, false);
        if matches.is_empty() {
            names.push(name);
        } else {
            names.extend(matches);
        }
    }
    editor.arglist_mut().insert_at(
        after,
        names
            .into_iter()
            .map(|name| OxStr::from(name.as_str()))
            .collect(),
    );
    Flow::Normal
}

fn command_find<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let pattern = command.args.trim();
    if pattern.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    // 'winfixbuf' rejects :find unless it bangs (`ex_find`, ex_docmd.c:5941).
    if let Some(flow) =
        access.with_ex_editor(|editor| winfixbuf_blocks(runtime, editor, command.bang))
    {
        return flow;
    }
    let Some(path) = crate::fs_builtins::expand_glob(runtime.scripts.io(), pattern, false)
        .into_iter()
        .next()
    else {
        return error_flow(runtime, "E345", "Can't find file in path");
    };
    let mut edit = command.clone();
    path.strip_prefix("./")
        .unwrap_or(&path)
        .clone_into(&mut edit.args);
    command_edit(runtime, access, scope, lua, &edit)
}

/// Bare `:edit` reloads the current buffer's file in place
/// (`do_ecmd` same-file path).
fn edit_reload_current<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    path: &Path,
) -> Flow {
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E32", "No file name");
    };
    let flow = fire_buffer_lifecycle(runtime, access, scope, lua, &[Event::BufReadPre], buffer);
    if !matches!(flow, Flow::Normal) {
        return flow;
    }
    let text = match runtime.scripts.io().read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            return error_flow(
                runtime,
                "E484",
                format!("Can't open file {}: {error}", path.display()),
            );
        }
    };
    let text = match Buffer::from_bytes(text.as_bytes()) {
        Ok(text) => text,
        Err(error) => return error_flow(runtime, "E474", error.to_string()),
    };
    access.with_ex_editor(|editor| {
        if let Ok(state) = editor.buffer_mut(buffer) {
            state.load(text);
            state.flags.set(crate::BufferFlags::NOTEDITED, false);
        }
    });
    fire_buffer_lifecycle(
        runtime,
        access,
        scope,
        lua,
        &[Event::BufReadPost, Event::BufEnter],
        buffer,
    )
}

/// 'winfixbuf' rejects `:edit` switching to another file (`do_ecmd`,
/// `ex_docmd.c:5987`): `is_other_file` is false when the target names the
/// current buffer, so editing its own name reloads and stays allowed.
fn edit_winfixbuf_guard<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    command: &ExCommand,
) -> Option<Flow> {
    let same_file = access.with_ex_editor(|editor| {
        editor.current_buffer().is_some_and(|current| {
            editor
                .buffer(current)
                .is_ok_and(|buffer| buffer.name().as_bytes() == command.args.trim().as_bytes())
        })
    });
    if same_file {
        return None;
    }
    access.with_ex_editor(|editor| winfixbuf_blocks(runtime, editor, command.bang))
}

fn command_edit<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    // `ea.arg` is the file name as written: the parser already skipped leading
    // space/tab and, because `:edit` is EX_TRLBAR without EX_NOTRLCOM, ran
    // `del_trailing_spaces` over it. Trimming again here would drop control
    // bytes upstream keeps, so `:edit Xa<CR>` really does name a buffer
    // ending in a CR.
    let reload_current = command.args.is_empty();
    let path = if reload_current {
        access.with_ex_editor(|editor| {
            editor
                .current_buffer()
                .and_then(|buffer| editor.buffer(buffer).ok())
                .map(|buffer| PathBuf::from(buffer.name().to_string_lossy().into_owned()))
                .unwrap_or_default()
        })
    } else {
        PathBuf::from(command.args.as_str())
    };
    if path.as_os_str().is_empty() {
        return error_flow(runtime, "E32", "No file name");
    }
    if let Some(current) = access.with_ex_editor(|editor| editor.current_buffer())
        && access.with_ex_editor(|editor| {
            !editor.is_terminal_buffer(current)
                && editor
                    .buffer(current)
                    .is_ok_and(|buffer| buffer.flags.contains(crate::BufferFlags::MODIFIED))
        })
        && !command.bang
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    if !reload_current && let Some(flow) = edit_winfixbuf_guard(runtime, access, command) {
        return flow;
    }
    if reload_current {
        return edit_reload_current(runtime, access, scope, lua, &path);
    }
    let (handle, created) =
        match access.with_ex_editor(|editor| buffer_from_file(runtime, editor, &path)) {
            Ok((handle, created)) => (handle, created),
            Err(flow) => return flow,
        };
    // `buf_alloc` (`buffer.c:2115-2135`) announces a freshly created listed
    // buffer before any window enters it, so a failing handler aborts the
    // entry with the caller still on the old buffer.
    if created {
        let flow = fire_buffer_lifecycle(
            runtime,
            access,
            scope,
            lua,
            &[Event::BufNew, Event::BufAdd],
            handle,
        );
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
    }
    if access.with_ex_editor(|editor| editor.current_window().is_none()) {
        match access
            .with_ex_editor(|editor| editor.create_tabpage(handle, DEFAULT_TABPAGE_GEOMETRY))
        {
            Ok(_) => {}
            Err(error) => return error_flow(runtime, "E948", error.to_string()),
        }
    } else if let Err(error) =
        access.with_ex_editor(|editor| editor.set_current_buffer(handle, BufferRelease::KeepLoaded))
    {
        return error_flow(runtime, "E948", error.to_string());
    }
    // `win_enter` (`window.c:2722`): entering the buffer fires `BufEnter`.
    fire_buffer_lifecycle(runtime, access, scope, lua, &[Event::BufEnter], handle)
}

/// `:tag`/` :tjump` and the rest of `ex_tag_cmd` (`tag.c` `do_tag`).
#[expect(
    clippy::too_many_lines,
    reason = "the tag command interpreter preserves Vim's command-specific stack transitions"
)]
fn command_tag<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let preview = command.command.name().starts_with('p') && command.command.name() != "pop";
    match command.command.name() {
        "tnext" | "ptnext" => return tag_step(runtime, access, scope, lua, 1, preview),
        "tprevious" | "tNext" | "ptprevious" | "ptNext" => {
            return tag_step(runtime, access, scope, lua, -1, preview);
        }
        "tfirst" | "trewind" | "ptfirst" | "ptrewind" => {
            return tag_step_to(runtime, access, scope, lua, 1, preview);
        }
        "tlast" | "ptlast" => return tag_step_to(runtime, access, scope, lua, usize::MAX, preview),
        "pop" => return access.with_ex_editor(|editor| command_pop(runtime, editor, command)),
        _ => {}
    }
    let needle = command.args.trim();
    if needle.is_empty() {
        return tag_forward(runtime, access, scope, lua, command, preview);
    }
    let tags_option = access.with_ex_editor(|editor| tags_option_value(editor));
    if access
        .with_ex_editor(|editor| option_number(editor, "verbose"))
        .unwrap_or(0)
        >= 5
    {
        for file in tags_option
            .split(',')
            .map(str::trim)
            .filter(|file| !file.is_empty())
        {
            access.with_ex_editor(|editor| {
                push_text_message(editor, format!("Searching tags file {file}"), false, true);
            });
        }
    }
    let taglength = match access.with_ex_editor(|editor| option_taglength(runtime, editor)) {
        Ok(taglength) => taglength,
        Err(flow) => return flow,
    };
    let ignorecase = matches!(
        access.with_ex_editor(
            |editor| option_value(editor, "ignorecase", SetLayer::Effective).cloned()
        ),
        Some(OptionValue::Boolean(true))
    );
    let tagbsearch = !matches!(
        access.with_ex_editor(
            |editor| option_value(editor, "tagbsearch", SetLayer::Effective).cloned()
        ),
        Some(OptionValue::Boolean(false))
    );
    let matches = match crate::tags::lookup_search(
        runtime.scripts.io(),
        &tags_option,
        needle,
        taglength,
        ignorecase,
        tagbsearch,
    ) {
        Ok(matches) => matches,
        Err((code, message)) => return error_flow(runtime, code, message),
    };
    let preferred = access.with_ex_editor(|editor| {
        editor.current_buffer().and_then(|buffer| {
            editor
                .buffer(buffer)
                .ok()
                .map(|state| state.name().to_string_lossy().into_owned())
        })
    });
    let matches = crate::tags::prefer_filename(matches, preferred.as_deref());
    let count = match command.count {
        Some(count) => match usize::try_from(count) {
            Ok(count) => count.max(1),
            Err(_) => return error_flow(runtime, "E475", "Invalid argument"),
        },
        None => wincmd_range_count(command).unwrap_or(1).max(1),
    };
    let index = count.saturating_sub(1);
    if index >= matches.len() {
        return error_flow(runtime, "E426", format!("Tag not found: {needle}"));
    }
    if matches[index].cmd.is_empty() {
        return Flow::Normal;
    }
    if matches!(
        command.command.name(),
        "tjump" | "ptjump" | "tselect" | "ptselect"
    ) && matches.len() > 1
    {
        access.with_ex_editor(|editor| {
            push_info_text_message(
                editor,
                format_tselect_listing(&matches, preferred.as_deref()),
            );
        });
        return Flow::Normal;
    }
    let split = preview || command.command.name() == "stag";
    let tab_after = match command
        .modifiers
        .iter()
        .find(|modifier| modifier.kind == ModifierKind::Tab)
        .map(|modifier| match modifier.count {
            Some(count) => usize::try_from(count)
                .map(|count| count.max(1))
                .map_err(|_| error_flow(runtime, "E475", "Invalid argument")),
            None => Ok(0),
        }) {
        Some(Ok(tab_after)) => Some(tab_after),
        Some(Err(flow)) => return flow,
        None => None,
    };
    let flow = jump_to_tag(
        runtime,
        access,
        scope,
        lua,
        needle,
        &matches,
        index,
        &TagJumpOptions {
            push: true,
            split,
            preview,
            tab_after,
            forceit: command.bang,
        },
    );
    if command.command.name() == "ltag"
        && matches!(flow, Flow::Normal)
        && let Some(matched) = matches.get(index)
    {
        access.with_ex_editor(|editor| fill_ltag_loclist(editor, matched));
    }
    flow
}

fn tags_option_value(editor: &Editor) -> String {
    option_value(editor, "tags", SetLayer::Effective)
        .or_else(|| editor.options().get_global("tags").ok())
        .and_then(|value| match value {
            OptionValue::String(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn option_number(editor: &Editor, name: &str) -> Option<i64> {
    match option_value(editor, name, SetLayer::Effective)
        .or_else(|| editor.options().get_global(name).ok())
    {
        Some(OptionValue::Number(value)) => Some(*value),
        _ => None,
    }
}

/// `'taglength'` as a count (`get_tag_length` keeps it non-negative). The
/// stored number is already clamped by `:set`, so a failed conversion is the
/// E475 absurd-argument path and is unreachable for option-representable
/// values.
fn option_taglength<F: FileIO>(runtime: &ExRuntime<F>, editor: &Editor) -> Result<usize, Flow> {
    usize::try_from(option_number(editor, "taglength").unwrap_or(0).max(0))
        .map_err(|_| error_flow(runtime, "E475", "Invalid argument"))
}

/// How `jump_to_tag` opens its target: which window kind and which
/// overrides, mirroring `do_tag`'s command flags (tag.c).
#[expect(
    clippy::struct_excessive_bools,
    reason = "the flag set is Vim's own command-modifier surface for tag jumps"
)]
struct TagJumpOptions {
    push: bool,
    split: bool,
    preview: bool,
    tab_after: Option<usize>,
    forceit: bool,
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "a tag jump is one ordered editor transaction with all Vim command modifiers"
)]
fn jump_to_tag<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    needle: &str,
    matches: &[crate::tags::TagMatch],
    index: usize,
    options: &TagJumpOptions,
) -> Flow {
    let mut index = index;
    let chosen = loop {
        let Some(candidate) = matches.get(index) else {
            let name = matches.get(index.saturating_sub(1)).map_or_else(
                || needle.to_owned(),
                |item| item.filename.display().to_string(),
            );
            if options.push
                && !options.preview
                && let Some(window) = access.with_ex_editor(|editor| editor.current_window())
                && let Some(item) = access.with_ex_editor(|editor| {
                    editor
                        .window(window)
                        .ok()
                        .map(|state| crate::tags::TagStackItem {
                            tagname: needle.to_owned(),
                            from_bufnr: state.buffer,
                            from_lnum: state.cursor.lnum,
                            from_col: state.cursor.col.saturating_add(1),
                            from_off: state.coladd,
                            bufnr: None,
                            matchnr: matches.len(),
                            user_data: None,
                        })
                })
            {
                access.with_ex_editor(|editor| {
                    if let Ok(stack) = editor.window_tag_stack_mut(window) {
                        stack.push_jump(item);
                    }
                });
            }
            return error_flow(runtime, "E429", format!("File \"{name}\" does not exist"));
        };
        if runtime.scripts.io().exists(&candidate.filename) {
            break candidate;
        }
        index += 1;
    };

    if swap_choice_aborts(runtime, access, scope, lua, &chosen.filename) {
        return Flow::Normal;
    }

    let origin_window = access.with_ex_editor(|editor| editor.current_window());
    let origin = origin_window.and_then(|window| {
        access.with_ex_editor(|editor| {
            editor
                .window(window)
                .ok()
                .map(|state| (state.buffer, state.cursor, state.coladd))
        })
    });
    let handle =
        match access.with_ex_editor(|editor| buffer_from_file(runtime, editor, &chosen.filename)) {
            Ok((handle, _)) => handle,
            Err(flow) => return flow,
        };
    if let Err(flow) = access.with_ex_editor(|editor| {
        open_tag_buffer(
            runtime,
            editor,
            handle,
            options.split,
            options.preview,
            options.tab_after,
            options.forceit,
        )
    }) {
        return flow;
    }

    let lines = match access.with_ex_editor(|editor| buffer_lines(editor, handle)) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let start_line = chosen
        .fields
        .iter()
        .find(|(key, _)| key == "line")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut search_pattern = tag_search_pattern(&chosen.cmd);
    let (target, guessed) = match crate::tags::cmd_target_from(&lines, &chosen.cmd, start_line) {
        Some(found) => found,
        None => match crate::tags::guess_target(&lines, &chosen.name) {
            Some((position, pattern)) => {
                search_pattern = Some(pattern);
                (position, true)
            }
            None if chosen.cmd.contains('/') => {
                return error_flow(runtime, "E434", "Can't find tag pattern");
            }
            None => (Position { lnum: 1, col: 0 }, false),
        },
    };
    if guessed {
        scope.replace_pair(
            ScopeKind::Vim,
            "statusmsg",
            Typval::String(OxStr::from("E435: Couldn't find tag, just guessing!")),
        );
    }

    if matches!(access.with_ex_editor(|editor| option_value(editor, "cpoptions", SetLayer::Effective).cloned()), Some(OptionValue::String(value)) if value.contains('t'))
        && let Some(pattern) = search_pattern
        && let Ok(content) = crate::register::RegisterContent::characterwise(pattern.as_bytes())
    {
        let _ = access.with_ex_editor(|editor| editor.registers_mut().set('/', content));
        scope.set_register(b"/", Typval::String(OxStr::from(pattern.as_str())));
    }

    if let Some(window) = access.with_ex_editor(|editor| editor.current_window())
        && let Err(error) = access.with_ex_editor(|editor| editor.set_window_cursor(window, target))
    {
        return error_flow(runtime, "E16", error.to_string());
    }

    let stack_item = origin.map(|(buffer, cursor, coladd)| crate::tags::TagStackItem {
        tagname: chosen.name.clone(),
        from_bufnr: buffer,
        from_lnum: cursor.lnum,
        from_col: cursor.col.saturating_add(1),
        from_off: coladd,
        bufnr: Some(handle),
        matchnr: index + 1,
        user_data: None,
    });
    if options.preview {
        if let Some(item) = stack_item.clone() {
            runtime.preview_tag = Some(item);
        } else if let Some(item) = runtime.preview_tag.as_mut() {
            item.matchnr = index + 1;
            item.bufnr = Some(handle);
        }
    } else if options.push
        && let Some(window) = access.with_ex_editor(|editor| editor.current_window())
        && let Some(item) = stack_item
    {
        access.with_ex_editor(|editor| {
            if let Ok(stack) = editor.window_tag_stack_mut(window) {
                stack.push_jump(item);
            }
        });
    } else if let Some(window) = access.with_ex_editor(|editor| editor.current_window()) {
        access.with_ex_editor(|editor| {
            if let Ok(stack) = editor.window_tag_stack_mut(window)
                && let Some(item) = stack.current_mut()
            {
                item.matchnr = index + 1;
                item.bufnr = Some(handle);
            }
        });
    }
    if options.preview
        && let Some(origin_window) = origin_window
        && let Err(error) = access.with_ex_editor(|editor| editor.set_current_window(origin_window))
    {
        return error_flow(runtime, "E36", error.to_string());
    }
    Flow::Normal
}

fn tag_search_pattern(cmd: &str) -> Option<String> {
    let cmd = cmd.trim().trim_end_matches(';').trim();
    let bytes = cmd.as_bytes();
    if bytes.first() != Some(&b'/') {
        return None;
    }
    let inner = if bytes.last() == Some(&b'/') && bytes.len() > 1 {
        &cmd[1..cmd.len() - 1]
    } else {
        &cmd[1..]
    };
    Some(inner.to_owned())
}

fn fill_ltag_loclist(editor: &mut Editor, matched: &crate::tags::TagMatch) {
    let bufnr = editor
        .current_buffer()
        .map(i64::from)
        .or_else(|| {
            editor
                .buffers()
                .into_iter()
                .find(|handle| {
                    editor.buffer(*handle).is_ok_and(|state| {
                        let name = state.name().to_string_lossy();
                        name == matched.filename.to_string_lossy()
                            || std::path::Path::new(name.as_ref()).file_name()
                                == matched.filename.file_name()
                    })
                })
                .map(i64::from)
        })
        .unwrap_or(0);

    let (lnum, pattern) = if let Ok(line) = matched.cmd.trim().parse::<i64>() {
        (line, String::new())
    } else if let Some(inner) = tag_search_pattern(&matched.cmd) {
        (
            0,
            format!(
                "^\\V{}\\$",
                inner.trim_start_matches('^').trim_end_matches('$')
            ),
        )
    } else {
        (0, String::new())
    };
    let item = crate::quickfix::QuickfixItem {
        bufnr,
        module: OxStr::from(""),
        lnum,

        col: 0,
        end_lnum: 0,
        end_col: 0,
        vcol: 0,
        nr: 0,
        pattern: OxStr::from(pattern.as_str()),
        text: OxStr::from(matched.name.as_str()),
        item_type: OxStr::from(""),
        valid: true,
        user_data: Typval::Special(Special::Null),
    };
    if editor.quickfix().current().is_none() {
        editor
            .quickfix_mut()
            .push(OxStr::from(matched.name.as_str()));
    }
    if let Some(list) = editor.quickfix_mut().current_mut() {
        list.set_items(vec![item]);
    }
}

fn format_tselect_listing(matches: &[crate::tags::TagMatch], preferred: Option<&str>) -> String {
    let mut lines = vec!["Select a tag:".to_owned()];
    for (index, matched) in matches.iter().enumerate() {
        let kind = matched
            .fields
            .iter()
            .find(|(key, _)| key == "kind")
            .map_or(" ", |(_, value)| value.as_str());
        let static_tag = matched.fields.iter().any(|(key, _)| key == "file");
        let current = preferred.is_some_and(|name| {
            matched.filename.to_str() == Some(name)
                || matched.filename.file_name().and_then(|file| file.to_str())
                    == std::path::Path::new(name)
                        .file_name()
                        .and_then(|file| file.to_str())
        });
        let mut pri = String::from("F");
        if static_tag {
            pri.push('S');
        } else {
            pri.push(' ');
        }
        if current {
            pri.push('C');
        } else {
            pri.push(' ');
        }
        let filename = matched
            .filename
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| matched.filename.to_str().unwrap_or(""));
        lines.push(format!(
            "{}:   {:<3} {:<4} {:<18} {}",
            index + 1,
            pri,
            kind,
            matched.name,
            filename
        ));
    }
    lines.join("\n")
}

fn open_tag_buffer<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    handle: BufHandle,
    split: bool,
    preview: bool,
    tab_after: Option<usize>,
    forceit: bool,
) -> Result<(), Flow> {
    if preview && let Some(existing) = preview_window(editor) {
        editor
            .set_current_window(existing)
            .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        editor
            .set_current_buffer(handle, BufferRelease::KeepLoaded)
            .map_err(|error| error_flow(runtime, "E948", error.to_string()))?;
        return Ok(());
    }
    if let Some(after) = tab_after {
        editor
            .create_tabpage_at(handle, DEFAULT_TABPAGE_GEOMETRY, after)
            .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        return Ok(());
    }
    let switchbuf = option_value(editor, "switchbuf", SetLayer::Effective)
        .and_then(|value| match value {
            OptionValue::String(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if split
        && switchbuf.contains("useopen")
        && let Some(window) = window_showing(editor, handle)
    {
        editor
            .set_current_window(window)
            .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        return Ok(());
    }
    if split
        && switchbuf.contains("usetab")
        && let Some((tab, window)) = tab_showing(editor, handle)
    {
        editor
            .set_current_tabpage(tab)
            .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        editor
            .set_current_window(window)
            .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        return Ok(());
    }
    if split && switchbuf.contains("newtab") {
        editor
            .create_tabpage(handle, DEFAULT_TABPAGE_GEOMETRY)
            .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        return Ok(());
    }
    if split && let (Some(tab), Some(window)) = (editor.current_tabpage(), editor.current_window())
    {
        let created = if preview || switchbuf.contains("vsplit") {
            if switchbuf.contains("vsplit") {
                editor.split_left(tab, window, handle, true)
            } else {
                editor.split_above(tab, window, handle, true)
            }
        } else {
            editor.split_horizontal(tab, window, handle, true)
        }
        .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        editor
            .set_current_window(created)
            .map_err(|error| error_flow(runtime, "E36", error.to_string()))?;
        if preview {
            let _ = editor.options_mut().set_window(
                created,
                "previewwindow",
                OptionValue::Boolean(true),
            );
        }
        return Ok(());
    }
    if editor.current_window().is_none() {
        editor
            .create_tabpage(handle, DEFAULT_TABPAGE_GEOMETRY)
            .map_err(|error| error_flow(runtime, "E948", error.to_string()))?;
        return Ok(());
    }
    // Opening the tag in this window is upstream's `postponed_split == 0`
    // path: 'winfixbuf' rejects it without the bang (tag.c:308, tag.c:2633).
    if !split
        && editor.current_buffer() != Some(handle)
        && let Some(flow) = winfixbuf_blocks(runtime, editor, forceit)
    {
        return Err(flow);
    }
    editor
        .set_current_buffer(handle, BufferRelease::KeepLoaded)
        .map_err(|error| error_flow(runtime, "E948", error.to_string()))?;
    Ok(())
}

fn window_showing(editor: &Editor, handle: BufHandle) -> Option<WinHandle> {
    editor.windows().into_iter().find(|window| {
        editor
            .window(*window)
            .is_ok_and(|state| state.buffer == handle)
    })
}

fn tab_showing(editor: &Editor, handle: BufHandle) -> Option<(TabHandle, WinHandle)> {
    for tab in editor.tabpages() {
        if let Ok(state) = editor.tabpage(tab) {
            for window in state.windows() {
                if editor
                    .window(window)
                    .is_ok_and(|window_state| window_state.buffer == handle)
                {
                    return Some((tab, window));
                }
            }
        }
    }
    None
}

fn swap_path_for(path: &std::path::Path) -> PathBuf {
    let file_name = path.file_name().unwrap_or_default();
    let mut name = std::ffi::OsString::from(".");
    name.push(file_name);
    name.push(".swp");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

fn swap_choice_aborts<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    path: &std::path::Path,
) -> bool {
    if !runtime.scripts.io().exists(&swap_path_for(path)) {
        return false;
    }
    scope.replace_pair(
        ScopeKind::Vim,
        "swapchoice",
        Typval::String(OxStr::from("")),
    );
    let name = path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let buffer = access.with_ex_editor(|editor| editor.current_buffer());
    let plan = access.with_ex_editor(|editor| {
        editor.autocmds_mut().plan(
            Event::SwapExists,
            AutocmdContext {
                buffer,
                file_name: Some(name.as_str()),
                match_name: None,
                nested: true,
                data: None,
            },
        )
    });

    let _ = run_autocmd_plan(runtime, access, scope, lua, plan);
    matches!(
        scope.get_scoped(ScopeKind::Vim, b"swapchoice", 0),
        Ok(Typval::String(value)) if value.as_bytes().first().is_some_and(|byte| byte.eq_ignore_ascii_case(&b'q'))
    )
}

fn tag_step<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    delta: isize,
    preview: bool,
) -> Flow {
    let item = if preview {
        runtime.preview_tag.clone()
    } else {
        access.with_ex_editor(|editor| {
            editor.current_window().and_then(|window| {
                editor
                    .window_tag_stack(window)
                    .ok()
                    .and_then(|stack| stack.current().cloned())
            })
        })
    };
    let Some(item) = item else {
        return error_flow(runtime, "E426", "Tag not found: ");
    };

    let tags_option = access.with_ex_editor(|editor| tags_option_value(editor));
    let taglength = match access.with_ex_editor(|editor| option_taglength(runtime, editor)) {
        Ok(value) => value,
        Err(flow) => return flow,
    };
    let ignorecase = matches!(
        access.with_ex_editor(
            |editor| option_value(editor, "ignorecase", SetLayer::Effective).cloned()
        ),
        Some(OptionValue::Boolean(true))
    );
    let matches = match crate::tags::lookup_with(
        runtime.scripts.io(),
        &tags_option,
        &item.tagname,
        taglength,
        ignorecase,
    ) {
        Ok(matches) => matches,
        Err((code, message)) => return error_flow(runtime, code, message),
    };
    let next = item.matchnr.saturating_add_signed(delta);
    if next == 0 {
        return error_flow(runtime, "E425", "Cannot go before first matching tag");
    }
    if next > matches.len() {
        let code = if matches.len() == 1 { "E427" } else { "E428" };
        let message = if matches.len() == 1 {
            "There is only one matching tag"
        } else {
            "Cannot go beyond last matching tag"
        };
        return error_flow(runtime, code, message.to_owned());
    }

    jump_to_tag(
        runtime,
        access,
        scope,
        lua,
        &item.tagname,
        &matches,
        next - 1,
        &TagJumpOptions {
            push: false,
            split: preview,
            preview,
            tab_after: None,
            forceit: false,
        },
    )
}

fn tag_step_to<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    matchnr: usize,
    preview: bool,
) -> Flow {
    let item = if preview {
        runtime.preview_tag.clone()
    } else {
        access.with_ex_editor(|editor| {
            editor.current_window().and_then(|window| {
                editor
                    .window_tag_stack(window)
                    .ok()
                    .and_then(|stack| stack.current().cloned())
            })
        })
    };
    let Some(item) = item else {
        return error_flow(
            runtime,
            if matchnr == 0 { "E555" } else { "E426" },
            if matchnr == 0 {
                "At bottom of tag stack"
            } else {
                "Tag not found: "
            }
            .to_owned(),
        );
    };

    let tags_option = access.with_ex_editor(|editor| tags_option_value(editor));
    let taglength = match access.with_ex_editor(|editor| option_taglength(runtime, editor)) {
        Ok(taglength) => taglength,
        Err(flow) => return flow,
    };
    let ignorecase = matches!(
        access.with_ex_editor(
            |editor| option_value(editor, "ignorecase", SetLayer::Effective).cloned()
        ),
        Some(OptionValue::Boolean(true))
    );
    let matches = match crate::tags::lookup_with(
        runtime.scripts.io(),
        &tags_option,
        &item.tagname,
        taglength,
        ignorecase,
    ) {
        Ok(matches) => matches,
        Err((code, message)) => return error_flow(runtime, code, message),
    };
    let index = if matchnr == usize::MAX {
        matches.len().saturating_sub(1)
    } else if matchnr == 0 {
        item.matchnr.saturating_sub(1)
    } else {
        matchnr.saturating_sub(1)
    };
    jump_to_tag(
        runtime,
        access,
        scope,
        lua,
        &item.tagname,
        &matches,
        index,
        &TagJumpOptions {
            push: false,
            split: preview,
            preview,
            tab_after: None,
            forceit: false,
        },
    )
}

/// The tag-file lookup `tag_forward` needs: the effective `'tags'`,
/// `'taglength'`, and `'ignorecase'` options feed `lookup_with`.
fn tag_lookup_matches<F: FileIO, E: ExEditorAccess>(
    runtime: &ExRuntime<F>,
    access: &E,
    tagname: &str,
) -> Result<Vec<crate::tags::TagMatch>, Flow> {
    let tags_option = access.with_ex_editor(|editor| tags_option_value(editor));
    let taglength = access.with_ex_editor(|editor| option_taglength(runtime, editor))?;
    let ignorecase = matches!(
        access.with_ex_editor(
            |editor| option_value(editor, "ignorecase", SetLayer::Effective).cloned()
        ),
        Some(OptionValue::Boolean(true))
    );
    crate::tags::lookup_with(
        runtime.scripts.io(),
        &tags_option,
        tagname,
        taglength,
        ignorecase,
    )
    .map_err(|(code, message)| error_flow(runtime, code, message))
}

fn tag_forward<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
    preview: bool,
) -> Flow {
    if preview {
        return tag_step_to(runtime, access, scope, lua, 0, true);
    }
    let Some(window) = access.with_ex_editor(|editor| editor.current_window()) else {
        return error_flow(runtime, "E73", "Tag stack empty");
    };
    let count = command
        .count
        .and_then(|value| usize::try_from(value).ok())
        .or_else(|| wincmd_range_count(command))
        .unwrap_or(1);
    let (old_idx, target, item) = {
        let state = access.with_ex_editor(|editor| {
            let Ok(stack) = editor.window_tag_stack(window) else {
                return Err("E73");
            };
            if stack.items().is_empty() {
                return Err("E73");
            }
            let old_idx = stack.curidx();
            let Some(target) = old_idx
                .checked_add(count)
                .and_then(|idx| idx.checked_sub(1))
            else {
                return Err("E556");
            };
            if target == 0 {
                return Err("E555");
            }
            if target > stack.len() {
                return Err("E556");
            }
            Ok((old_idx, target, stack.items()[target - 1].clone()))
        });
        match state {
            Ok((old_idx, target, item)) => (old_idx, target, item),
            Err("E555") => return error_flow(runtime, "E555", "At bottom of tag stack"),
            Err("E556") => return error_flow(runtime, "E556", "At top of tag stack"),
            Err(_) => return error_flow(runtime, "E73", "Tag stack empty"),
        }
    };
    let matches = match tag_lookup_matches(runtime, access, &item.tagname) {
        Ok(matches) => matches,
        Err(flow) => return flow,
    };
    let preferred = access.with_ex_editor(|editor| {
        editor.current_buffer().and_then(|buffer| {
            editor
                .buffer(buffer)
                .ok()
                .map(|state| state.name().to_string_lossy().into_owned())
        })
    });
    let matches = crate::tags::prefer_filename(matches, preferred.as_deref());
    access.with_ex_editor(|editor| {
        if let Ok(stack) = editor.window_tag_stack_mut(window) {
            stack.set_curidx(i64::try_from(target).unwrap_or(i64::MAX));
        }
    });
    let flow = jump_to_tag(
        runtime,
        access,
        scope,
        lua,
        &item.tagname,
        &matches,
        item.matchnr.saturating_sub(1),
        &TagJumpOptions {
            push: false,
            split: false,
            preview: false,
            tab_after: None,
            forceit: false,
        },
    );
    access.with_ex_editor(|editor| {
        if let Ok(stack) = editor.window_tag_stack_mut(window) {
            let idx = if matches!(flow, Flow::Normal) {
                target.saturating_add(1)
            } else {
                old_idx
            };
            stack.set_curidx(i64::try_from(idx).unwrap_or(i64::MAX));
        }
    });
    flow
}

fn command_pop<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(window) = editor.current_window() else {
        return error_flow(runtime, "E73", "Tag stack empty");
    };
    let count = command
        .count
        .and_then(|value| usize::try_from(value).ok())
        .or_else(|| wincmd_range_count(command))
        .unwrap_or(1);
    let old_idx = editor
        .window_tag_stack(window)
        .map_or(1, crate::tags::TagStack::curidx);
    let item = match editor
        .window_tag_stack_mut(window)
        .map(|stack| stack.pop(count))
    {
        Ok(Ok(item)) => item,
        Ok(Err(crate::tags::TagStackBoundary::Empty)) => {
            return error_flow(runtime, "E73", "Tag stack empty");
        }
        Ok(Err(crate::tags::TagStackBoundary::Bottom)) => {
            return error_flow(runtime, "E555", "At bottom of tag stack");
        }
        Ok(Err(crate::tags::TagStackBoundary::Top)) => {
            return error_flow(runtime, "E556", "At top of tag stack");
        }
        Err(_) => return error_flow(runtime, "E73", "Tag stack empty"),
    };
    let current = editor.current_buffer();
    if current != Some(item.from_bufnr)
        && current.is_some_and(|handle| {
            editor
                .buffer(handle)
                .is_ok_and(|buffer| buffer.flags.contains(crate::BufferFlags::MODIFIED))
        })
        && !command.bang
    {
        if let Ok(stack) = editor.window_tag_stack_mut(window) {
            stack.set_curidx(i64::try_from(old_idx).unwrap_or(i64::MAX));
        }
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    if editor
        .set_current_buffer(item.from_bufnr, BufferRelease::KeepLoaded)
        .is_err()
    {
        if let Ok(stack) = editor.window_tag_stack_mut(window) {
            stack.set_curidx(i64::try_from(old_idx).unwrap_or(i64::MAX));
        }
        return error_flow(runtime, "E555", "At bottom of tag stack");
    }
    let target = Position {
        lnum: item.from_lnum.max(1),
        col: item.from_col.saturating_sub(1),
    };
    if let Err(error) = editor.set_window_cursor(window, target) {
        if let Ok(stack) = editor.window_tag_stack_mut(window) {
            stack.set_curidx(i64::try_from(old_idx).unwrap_or(i64::MAX));
        }
        return error_flow(runtime, "E16", error.to_string());
    }
    Flow::Normal
}

fn command_findpat<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let name = command.command.name();
    let define = name.starts_with('d');
    let action = match name {
        "ilist" | "dlist" => crate::include_search::IdentSearchAction::List,
        "ijump" | "djump" => crate::include_search::IdentSearchAction::Goto,
        "isplit" | "dsplit" => crate::include_search::IdentSearchAction::Split,
        _ => crate::include_search::IdentSearchAction::Show,
    };
    let kind = if define {
        crate::include_search::IdentSearchKind::Define
    } else {
        crate::include_search::IdentSearchKind::Any
    };
    let (start, end) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let mut rest = command.args.trim_start();
    let Ok(mut count) = usize::try_from(command.count.unwrap_or(0)) else {
        return error_flow(runtime, "E475", "Invalid argument");
    };
    if count == 0 {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            count = rest[..digits].parse().unwrap_or(1);
            rest = rest[digits..].trim_start();
        } else {
            count = 1;
        }
    }
    let (pattern, whole) = if let Some(body) = rest.strip_prefix('/') {
        match body.find('/') {
            Some(end) => {
                let pattern = &body[..end];
                let trailing = body[end + 1..].trim_start();
                if !trailing.is_empty() {
                    return error_flow(runtime, "E488", format!("Trailing characters: {trailing}"));
                }
                (pattern.to_owned(), false)
            }
            None => (body.to_owned(), false),
        }
    } else {
        (rest.to_owned(), true)
    };
    if pattern.is_empty() {
        return error_flow(runtime, "E389", "Couldn't find pattern");
    }
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let current = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .map_or(1, |state| state.cursor.lnum);
    let relative_to = buffer_search_dir(editor, buffer);
    let hits = crate::include_search::collect_hits_with_includes(
        &lines,
        pattern.as_bytes(),
        whole,
        kind,
        start,
        end,
        relative_to.as_deref(),
    );
    match crate::include_search::apply(editor, &hits, action, count, current, kind) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

fn wincmd_ident_search<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    key: char,
) -> Flow {
    let Some(window) = editor.current_window() else {
        return error_flow(runtime, "E749", "No current window");
    };
    let Ok(state) = editor.window(window) else {
        return error_flow(runtime, "E957", "Invalid window");
    };
    let buffer = state.buffer;
    let cursor = state.cursor;
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let Some(ident) = crate::motion::ident_under(&lines, cursor) else {
        return error_flow(runtime, "E349", "No identifier under cursor");
    };
    let kind = if key == 'd' {
        crate::include_search::IdentSearchKind::Define
    } else {
        crate::include_search::IdentSearchKind::Any
    };
    let count = match command.count {
        Some(value) => match usize::try_from(value) {
            Ok(value) => value,
            Err(_) => return error_flow(runtime, "E475", "Invalid argument"),
        },
        None => wincmd_range_count(command).unwrap_or(1),
    };
    let relative_to = buffer_search_dir(editor, buffer);
    let hits = crate::include_search::collect_hits_with_includes(
        &lines,
        ident,
        true,
        kind,
        1,
        lines.len(),
        relative_to.as_deref(),
    );
    match crate::include_search::apply(
        editor,
        &hits,
        crate::include_search::IdentSearchAction::Split,
        count.max(1),
        cursor.lnum,
        kind,
    ) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

fn buffer_search_dir(editor: &Editor, buffer: BufHandle) -> Option<PathBuf> {
    editor.buffer(buffer).ok().and_then(|state| {
        let name = state.name().to_string_lossy();
        let path = std::path::Path::new(name.as_ref());
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
    })
}

fn wincmd_range_count(command: &ExCommand) -> Option<usize> {
    let range = command.range.as_ref()?;
    let address = range.end.as_ref().or(range.start.as_ref())?;
    match address.base {
        AddressBase::Line(value) if address.offsets.is_empty() => usize::try_from(value).ok(),
        _ => None,
    }
}

fn command_stopinsert<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    _access: &E,
) -> Flow {
    runtime.pending_edit_mode = Some(PendingEditMode::StopInsert);
    Flow::Normal
}

fn command_terminal<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let autowrite = access.with_ex_editor(|editor| {
        matches!(
            editor.options().get_global("autowrite"),
            Ok(OptionValue::Boolean(true))
        ) && editor.current_buffer().is_some_and(|buffer| {
            editor.buffer(buffer).is_ok_and(|state| {
                state.flags.contains(crate::BufferFlags::MODIFIED)
                    && !state.name().as_bytes().is_empty()
            })
        })
    });
    if autowrite {
        let mut write = command.clone();
        write.args.clear();
        write.bang = false;
        write.usefilter = false;
        let flow = command_write(runtime, access, scope, lua, &write);
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
    }
    let (_, buffer) = match crate::builtins::process::start_terminal(
        runtime,
        access,
        command.args.trim_start(),
    ) {
        Ok(terminal) => terminal,
        Err(error) => return eval_error_flow(runtime, error),
    };
    let flow = fire_buffer_lifecycle(runtime, access, scope, lua, &[Event::BufNew], buffer);
    if !matches!(flow, Flow::Normal) {
        return flow;
    }
    access.with_ex_editor(|editor| {
        let origin = editor.current_window().and_then(|window| {
            editor
                .window(window)
                .ok()
                .map(|state| (state.buffer, state.cursor))
        });
        if let Some((buffer, position)) = origin {
            editor
                .jumplist_mut()
                .push(crate::marks::MarkLocation::in_buffer(buffer, position));
        }
    });
    if let Err(error) =
        access.with_ex_editor(|editor| editor.set_current_buffer(buffer, BufferRelease::KeepLoaded))
    {
        return error_flow(runtime, "E948", error.to_string());
    }
    fire_buffer_lifecycle(runtime, access, scope, lua, &[Event::TermOpen], buffer)
}

fn command_enew<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    // `:enew` always names a different buffer, so 'winfixbuf' rejects it
    // unless the command bangs (`do_ecmd`, ex_docmd.c:5987).
    if let Some(flow) =
        access.with_ex_editor(|editor| winfixbuf_blocks(runtime, editor, command.bang))
    {
        return flow;
    }
    if let Some(current) = access.with_ex_editor(|editor| editor.current_buffer())
        && access.with_ex_editor(|editor| {
            editor
                .buffer(current)
                .is_ok_and(|buffer| buffer.flags.contains(crate::BufferFlags::MODIFIED))
        })
        && !command.bang
        && !access.with_ex_editor(|editor| {
            matches!(
                editor.options().get_global("hidden"),
                Ok(OptionValue::Boolean(true))
            )
        })
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    let handle = match access.with_ex_editor(|editor| editor.create_buffer(true)) {
        Ok(handle) => handle,
        Err(error) => return error_flow(runtime, "E948", error.to_string()),
    };
    // `buf_alloc` (`buffer.c:2115-2135`): the new listed buffer fires `BufNew`
    // then `BufAdd` before it is entered.
    let flow = fire_buffer_lifecycle(
        runtime,
        access,
        scope,
        lua,
        &[Event::BufNew, Event::BufAdd],
        handle,
    );
    if !matches!(flow, Flow::Normal) {
        return flow;
    }
    if access.with_ex_editor(|editor| editor.current_window().is_none()) {
        if let Err(error) = access.with_ex_editor(|editor| {
            editor.create_tabpage(
                handle,
                Geometry {
                    row: 0,
                    col: 0,
                    width: 80,
                    height: 24,
                },
            )
        }) {
            return error_flow(runtime, "E948", error.to_string());
        }
    } else if let Err(error) =
        access.with_ex_editor(|editor| editor.set_current_buffer(handle, BufferRelease::KeepLoaded))
    {
        return error_flow(runtime, "E948", error.to_string());
    }
    // `win_enter` (`window.c:2722`).
    fire_buffer_lifecycle(runtime, access, scope, lua, &[Event::BufEnter], handle)
}

/// `:bwipeout`/`:bdelete` (`ex_cmds.c` `ex_bwipe/ex_bdelete)`: resolve the
/// buffer from the count or argument (defaulting to the current buffer),
/// move displaying windows onto another buffer, then wipe or unload it.
/// The modified-buffer guard matches `do_buffer`'s E89.
#[expect(
    clippy::too_many_lines,
    reason = "buffer removal keeps window migration and modified-buffer checks atomic"
)]
fn command_buffer_remove<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    wipe: bool,
) -> Flow {
    let arg = command.args.trim();
    let requested = command
        .count
        .and_then(|value| i64::try_from(value).ok())
        .or_else(|| arg.parse::<i64>().ok());
    let mut targets = if command.range.is_some() {
        let (start, end) = match resolve_range(editor, command) {
            Ok(range) => range,
            Err(message) => return error_flow(runtime, "E16", message),
        };
        editor
            .buffers()
            .into_iter()
            .filter(|handle| {
                usize::try_from(i64::from(*handle))
                    .is_ok_and(|number| (start..=end).contains(&number))
            })
            .collect::<Vec<_>>()
    } else {
        let target =
            if let Some(handle) = requested.and_then(|value| BufHandle::try_from(value).ok()) {
                handle
            } else if arg.is_empty() {
                match editor.current_buffer() {
                    Some(handle) => handle,
                    None => return error_flow(runtime, "E85", "There is no listed buffer"),
                }
            } else {
                let matches: Vec<_> = editor
                    .buffers()
                    .into_iter()
                    .filter(|handle| {
                        editor
                            .buffer(*handle)
                            .is_ok_and(|buffer| buffer_name_matches(buffer.name(), arg))
                    })
                    .collect();
                match matches.as_slice() {
                    [handle] => *handle,
                    [] => {
                        return error_flow(runtime, "E94", format!("No matching buffer for {arg}"));
                    }
                    _ => match editor
                        .current_buffer()
                        .filter(|current| matches.contains(current))
                    {
                        Some(current) => current,
                        None => {
                            return error_flow(
                                runtime,
                                "E93",
                                format!("More than one match for {arg}"),
                            );
                        }
                    },
                }
            };
        vec![target]
    };
    if targets.is_empty() {
        let (code, message) = if wipe {
            ("E517", "No buffers were wiped out")
        } else {
            ("E516", "No buffers were deleted")
        };
        return error_flow(runtime, code, message);
    }
    // Deleting the current buffer last prevents each replacement from
    // becoming current and loading just before it is deleted.
    if let Some(current) = editor.current_buffer()
        && let Some(index) = targets.iter().position(|target| *target == current)
    {
        targets.remove(index);
        targets.push(current);
    }
    for target in &targets {
        if editor.buffer(*target).is_err() {
            return error_flow(
                runtime,
                "E86",
                format!("Buffer {} does not exist", i64::from(*target)),
            );
        }
        if !command.bang
            && editor
                .buffer(*target)
                .is_ok_and(|state| state.flags.contains(crate::BufferFlags::MODIFIED))
        {
            return error_flow(
                runtime,
                "E89",
                "No write since last change (add ! to override)",
            );
        }
    }

    let selected: std::collections::HashSet<_> = targets.iter().copied().collect();
    for target in targets {
        let attached = editor
            .windows()
            .into_iter()
            .filter(|window| {
                editor
                    .window(*window)
                    .is_ok_and(|state| state.buffer == target)
            })
            .collect::<Vec<_>>();
        if !attached.is_empty() {
            let replacement = match editor.buffers().into_iter().find(|buffer| {
                !selected.contains(buffer)
                    && editor.buffer(*buffer).is_ok_and(|state| {
                        state.flags.contains(crate::BufferFlags::LISTED)
                            && state.residency.is_loaded()
                    })
            }) {
                Some(buffer) => buffer,
                None => match editor.create_buffer(true) {
                    Ok(handle) => handle,
                    Err(error) => return error_flow(runtime, "E948", error.to_string()),
                },
            };
            for window in attached {
                if let Err(error) =
                    editor.set_window_buffer(window, replacement, BufferRelease::KeepLoaded)
                {
                    return error_flow(runtime, "E948", error.to_string());
                }
            }
        }
        if !wipe {
            if let Ok(state) = editor.buffer_mut(target) {
                state.flags.set(crate::BufferFlags::LISTED, false);
            }
            if let Err(error) = editor.unload_buffer(target) {
                return error_flow(runtime, "E90", error.to_string());
            }
            continue;
        }
        if let Err(error) = editor.wipe_buffer(target) {
            return error_flow(runtime, "E90", error.to_string());
        }
        // A wipe drops the buffer's local user commands; unload/delete do
        // not (`do_buffer`'s DOBUF_WIPE branch).
        runtime.user_commands.borrow_mut().remove_buffer(target);
        for window in editor.windows() {
            if let Ok(stack) = editor.window_tag_stack_mut(window) {
                stack.forget_buffer(target);
            }
        }
        if runtime
            .preview_tag
            .as_ref()
            .is_some_and(|item| item.from_bufnr == target)
        {
            runtime.preview_tag = None;
        }
    }
    Flow::Normal
}

/// `:read` and `:read !cmd` (`ex_docmd.c` `ex_read`:6163-6195).
///
/// Both forms insert lines after the addressed line, which defaults to the
/// cursor line and may be 0 (`ZEROR`) to prepend. The file form reads through
/// the [`FileIO`] seam and leaves the cursor on the *first* inserted line; the
/// filter form runs the tail through the shell and leaves the cursor on the
/// *last* inserted line (`ex_cmds.c` `do_filter`:1430-1436). Both then move to
/// the first non-blank column (`beginline(BL_WHITE | BL_FIX)`). An unreadable
/// file is E484 and a bare `:read` in a nameless buffer is E32.
///
/// `:read` reaches the buffer through `readfile`, so it carries `readfile`'s
/// autocommands. The file form fires `FileReadCmd` first, and a matching
/// definition *replaces* the read (`fileio.c:336-340`); otherwise
/// `FileReadPre` runs before the insert (`fileio.c:640`) and `FileReadPost`
/// after it (`fileio.c:1925`). The filter form reads with `READ_FILTER`, so it
/// fires `FilterReadPre`/`FilterReadPost` instead (`fileio.c:631,1914`), and
/// `do_bang` adds `ShellFilterPost` (`ex_cmds.c:1236`).
///
/// Upstream's `"name" 2L, 4B` file report is not emitted: no command in this
/// port has a file-message model, `:edit` included.
#[expect(
    clippy::too_many_lines,
    reason = "read command autocmd ordering and file insertion form one indivisible transaction"
)]
fn command_read<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let after = match access.with_ex_editor(|editor| resolve_range_raw(editor, command)) {
        Ok((_, end)) => end.min(access.with_ex_editor(|editor| buffer_last_line(editor))),
        Err(message) => return error_flow(runtime, "E16", message),
    };

    let mut matched = None;
    let lines = if command.usefilter {
        let output = match filter_output(runtime, scope, &command.args) {
            Ok(lines) => lines,
            Err(flow) => return flow,
        };
        // readfile(READ_FILTER) fires FilterReadPre once the shell has produced
        // its output and before the lines land.
        let flow = fire_read_autocmd(runtime, access, scope, lua, Event::FilterReadPre, None);
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
        output
    } else {
        let name = command.args.trim();
        let path = if name.is_empty() {
            let existing = access.with_ex_editor(|editor| {
                editor
                    .buffer(buffer)
                    .map(|state| state.name().to_string_lossy().into_owned())
            });
            let existing = match existing {
                Ok(name) => name,
                Err(error) => return error_flow(runtime, "E32", error.to_string()),
            };
            if existing.is_empty() {
                return error_flow(runtime, "E32", "No file name");
            }
            PathBuf::from(existing)
        } else {
            access.with_ex_editor(|editor| argument_path(editor, name))
        };
        let name = path.to_string_lossy().into_owned();
        // FileReadCmd intercepts: when a definition matches, it does the read
        // itself and this command performs none of its own work.
        let plan = access.with_ex_editor(|editor| {
            editor.autocmds_mut().plan(
                Event::FileReadCmd,
                AutocmdContext {
                    buffer: None,
                    file_name: Some(&name),
                    ..AutocmdContext::default()
                },
            )
        });
        if !plan.ready.is_empty() {
            return run_autocmd_plan(runtime, access, scope, lua, plan);
        }
        let flow = fire_read_autocmd(runtime, access, scope, lua, Event::FileReadPre, Some(&name));
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
        let Ok(bytes) = runtime.scripts.io().read_bytes(&path) else {
            return error_flow(
                runtime,
                "E484",
                format!("Can't open file {}", path.display()),
            );
        };
        matched = Some(name);
        split_read_lines(&bytes)
    };

    if !lines.is_empty() {
        let window = access.with_ex_editor(|editor| editor.current_window());
        let cursor = window
            .and_then(|window| {
                access.with_ex_editor(|editor| editor.window(window).ok().map(|state| state.cursor))
            })
            .unwrap_or(Position {
                lnum: after.max(1),
                col: 0,
            });
        if let Err(error) = access
            .with_ex_editor(|editor| editor.append_buffer_lines(buffer, after, &lines, cursor, 0))
        {
            return error_flow(runtime, "E484", error.to_string());
        }
        let target = if command.usefilter {
            after + lines.len()
        } else {
            after + 1
        };
        let column = lines
            .get(if command.usefilter {
                lines.len() - 1
            } else {
                0
            })
            .map_or(0, |line| {
                line.iter()
                    .take_while(|byte| matches!(byte, b' ' | b'\t'))
                    .count()
            });
        if let Some(window) = window
            && let Err(error) = access.with_ex_editor(|editor| {
                editor.set_window_cursor(
                    window,
                    Position {
                        lnum: target,
                        col: column,
                    },
                )
            })
        {
            return error_flow(runtime, "E484", error.to_string());
        }
    }

    // Both post events fire even for an empty read: upstream's readfile runs
    // them on the way out regardless of how many lines arrived.
    let post = if command.usefilter {
        Event::FilterReadPost
    } else {
        Event::FileReadPost
    };
    let flow = fire_read_autocmd(runtime, access, scope, lua, post, matched.as_deref());
    if !matches!(flow, Flow::Normal) || !command.usefilter {
        return flow;
    }
    fire_shell_filter_post(runtime, access, scope, lua)
}

/// Runs one `readfile` autocommand event.
///
/// `matched` is the file name upstream matches the pattern against for the
/// `FileRead*` events, which pass `sfname` with a null buffer
/// (`fileio.c:336,640,1925`). The `Filter*` events pass a null name and
/// `curbuf` instead, so they match the current buffer's name, as
/// `:help FilterReadPre` documents.
fn fire_read_autocmd<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    event: Event,
    matched: Option<&str>,
) -> Flow {
    let (buffer, name) = match matched {
        // FileRead* events still bind `<abuf>` to the current buffer, matching
        // upstream `readfile` which passes `curbuf` alongside `sfname`.
        Some(name) => (
            access.with_ex_editor(|editor| editor.current_buffer()),
            name.to_owned(),
        ),
        None => (
            access.with_ex_editor(|editor| editor.current_buffer()),
            access.with_ex_editor(|editor| current_buffer_name(editor)),
        ),
    };
    let plan = access.with_ex_editor(|editor| {
        editor.autocmds_mut().plan(
            event,
            AutocmdContext {
                buffer,
                file_name: Some(&name),
                ..AutocmdContext::default()
            },
        )
    });
    run_autocmd_plan(runtime, access, scope, lua, plan)
}

/// `do_filetype_autocmd` (`autocmd.c:2516-2539`) as reached from a committed
/// `filetype` assignment: fire `FileType` for the buffer whose option was
/// just written, with the committed value as `<amatch>`, the buffer name as
/// `<afile>`, and the buffer as `<abuf>`.
///
/// Recursion rules follow upstream: a same-value assignment inside a running
/// `FileType` plan is suppressed even from a `++nested` handler (`ft_recursive`
/// guard), a changed one fires nested, and a top-level same-value write fires
/// like any other. A handler failure keeps the assignment committed; its
/// flow propagates so later `:set` arguments are abandoned.
fn fire_filetype_autocmd<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    assignment: &OptionAssignment,
) -> Flow {
    let Some(buffer) = assignment.buffer else {
        return Flow::Normal;
    };
    let OptionValue::String(filetype) = &assignment.value else {
        return Flow::Normal;
    };
    if runtime.filetype_autocmd_depth > 0 && !assignment.changed {
        return Flow::Normal;
    }
    // `apply_autocmds` (`autocmd.c:1465-1468`): while autocommands are busy,
    // a non-forced event fires only through a `++nested` handler.
    let force = assignment.changed || runtime.filetype_autocmd_depth == 0;
    if runtime.autocmd_busy > 0 && !force && !runtime.active_autocmd.nested {
        return Flow::Normal;
    }
    let name = access.with_ex_editor(|editor| {
        editor
            .buffer(buffer)
            .ok()
            .map(|state| state.name().to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    // Upstream passes `force || ft_recursive == 1`, which is always true
    // once the gates above pass, so this event may itself nest.
    let plan = access.with_ex_editor(|editor| {
        editor.autocmds_mut().plan(
            Event::FileType,
            AutocmdContext {
                buffer: Some(buffer),
                file_name: Some(&name),
                match_name: Some(filetype.as_str()),
                nested: true,
                data: None,
            },
        )
    });
    if plan.ready.is_empty() {
        return Flow::Normal;
    }
    runtime.filetype_autocmd_depth += 1;
    let flow = run_autocmd_plan(runtime, access, scope, lua, plan);
    runtime.filetype_autocmd_depth -= 1;
    flow
}

/// `ShellFilterPost`, which `do_bang` applies after every ranged `do_filter`
/// run whether or not the filter produced output (`ex_cmds.c:1236`).
fn fire_shell_filter_post<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
) -> Flow {
    let name = access.with_ex_editor(|editor| current_buffer_name(editor));
    let buffer = access.with_ex_editor(|editor| editor.current_buffer());
    let plan = access.with_ex_editor(|editor| {
        editor.autocmds_mut().plan(
            Event::ShellFilterPost,
            AutocmdContext {
                buffer,
                file_name: Some(&name),
                ..AutocmdContext::default()
            },
        )
    });
    run_autocmd_plan(runtime, access, scope, lua, plan)
}

fn current_buffer_name(editor: &Editor) -> String {
    editor
        .current_buffer()
        .and_then(|buffer| editor.buffer(buffer).ok())
        .map(|state| state.name().to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Splits read bytes into buffer lines. A trailing newline terminates the last
/// line rather than starting an empty one; text without it still contributes a
/// final line, as `readfile`'s "noeol" handling does.
fn split_read_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    body.split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line).to_vec())
        .collect()
}

/// Runs one shell command and returns its standard output as buffer lines,
/// publishing the exit status in `v:shell_error`. The shell is the same
/// `sh -c` invocation `system()` uses in this port.
fn filter_output<F: FileIO>(
    runtime: &ExRuntime<F>,
    scope: &mut Scope,
    command: &str,
) -> Result<Vec<Vec<u8>>, Flow> {
    let command = command.trim();
    if command.is_empty() {
        return Err(error_flow(runtime, "E471", "Argument required"));
    }
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let output = match std::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return Err(error_flow(
                runtime,
                "E485",
                format!("Can't read file {command}: {error}"),
            ));
        }
    };
    let status = output.status.code().unwrap_or(-1);
    scope.replace_pair(
        ScopeKind::Vim,
        "shell_error",
        Typval::Number(i64::from(status)),
    );
    Ok(split_read_lines(&output.stdout))
}

fn command_file<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let argument = command.args.trim();
    if command.range.is_some() {
        let clear_name = matches!(
            command.range.as_ref().map(|range| &range.kind),
            Some(RangeKind::Single)
        ) && access
            .with_ex_editor(|editor| resolve_range_raw(editor, command))
            .is_ok_and(|range| range == (0, 0));
        if !clear_name || !argument.is_empty() {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        return rename_current_buffer(runtime, access, scope, lua, OxStr::from(""));
    }
    if argument.is_empty() {
        access.with_ex_editor(|editor| show_current_file(runtime, editor));
    }
    rename_current_buffer(runtime, access, scope, lua, OxStr::from(argument))
}

fn rename_current_buffer<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: OxStr,
) -> Flow {
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E32", "No file name");
    };
    if access.with_ex_editor(|editor| {
        editor
            .buffer(buffer)
            .is_ok_and(|state| state.name() == &name)
    }) {
        if let Some(window) = access.with_ex_editor(|editor| editor.current_window()) {
            access.with_ex_editor(|editor| {
                if let Ok(state) = editor.window_mut(window) {
                    state.alternate_buffer = Some(buffer);
                }
            });
        }
        access.with_ex_editor(|editor| show_current_file(runtime, editor));
    }
    let flow = fire_buffer_lifecycle(runtime, access, scope, lua, &[Event::BufFilePre], buffer);
    if !matches!(flow, Flow::Normal)
        || access.with_ex_editor(|editor| editor.current_buffer()) != Some(buffer)
    {
        return flow;
    }
    if let Err(error) = access.with_ex_editor(|editor| editor.rename_buffer(buffer, name)) {
        return match error {
            EditorError::NameInUse(_) => {
                error_flow(runtime, "E95", "Buffer with this name already exists")
            }
            _ => error_flow(runtime, "E749", error.to_string()),
        };
    }
    let flow = fire_buffer_lifecycle(runtime, access, scope, lua, &[Event::BufFilePost], buffer);
    if matches!(flow, Flow::Normal) {
        access.with_ex_editor(|editor| show_current_file(runtime, editor))
    } else {
        flow
    }
}

fn show_current_file<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E32", "No file name");
    };
    let state = match editor.buffer(buffer) {
        Ok(state) => state,
        Err(error) => return error_flow(runtime, "E749", error.to_string()),
    };
    let name = state.name().to_string_lossy();
    let display_name = if name.is_empty() {
        "[No Name]"
    } else {
        name.as_ref()
    };
    let mut message = format!("\"{display_name}\"");
    if state.flags.contains(crate::BufferFlags::NOTEDITED) {
        message.push_str(" [Not edited]");
    }
    if state.flags.contains(crate::BufferFlags::MODIFIED) {
        message.push_str(" [Modified]");
    }
    if state.text().is_ok_and(|text| text.to_bytes().is_empty()) {
        message.push_str(" --No lines in buffer--");
    }
    push_text_message(editor, message, false, false);
    Flow::Normal
}

fn command_write<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    if command.usefilter {
        return command_write_filter(runtime, access, scope, lua, command);
    }
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E32", "No file name");
    };
    if !command.bang
        && access.with_ex_editor(|editor| {
            editor
                .buffer(buffer)
                .is_ok_and(|state| state.flags.contains(crate::BufferFlags::READONLY))
        })
    {
        return error_flow(
            runtime,
            "E45",
            "'readonly' option is set (add ! to override)",
        );
    }
    let name = command.args.trim();
    let path = if name.is_empty() {
        let existing = access.with_ex_editor(|editor| {
            editor
                .buffer(buffer)
                .map(|state| state.name().to_string_lossy().into_owned())
        });
        let existing = match existing {
            Ok(name) => name,
            Err(error) => return error_flow(runtime, "E32", error.to_string()),
        };
        if existing.is_empty() {
            return error_flow(runtime, "E32", "No file name");
        }
        PathBuf::from(existing)
    } else {
        PathBuf::from(name)
    };
    if access.with_ex_editor(|editor| {
        editor
            .buffer(buffer)
            .is_ok_and(|state| state.flags.contains(crate::BufferFlags::NOTEDITED))
    }) && runtime.scripts.io().exists(&path)
        && !command.bang
    {
        return error_flow(runtime, "E13", "File exists (add ! to override)");
    }
    let mut bytes = match access.with_ex_editor(|editor| {
        editor
            .buffer(buffer)
            .and_then(|state| state.text().map_err(Into::into))
            .map(ox_text::Buffer::to_bytes)
    }) {
        Ok(bytes) => bytes,
        Err(error) => return error_flow(runtime, "E749", error.to_string()),
    };
    if bytes.last().is_some_and(|byte| *byte != b'\n') {
        bytes.push(b'\n');
    }
    let contents = String::from_utf8_lossy(&bytes);
    if let Err(error) = runtime.scripts.io().write_string(&path, &contents) {
        return error_flow(
            runtime,
            "E212",
            format!("Can't open file for writing: {error}"),
        );
    }
    access.with_ex_editor(|editor| {
        if let Ok(state) = editor.buffer_mut(buffer) {
            state.set_name(OxStr::from(path.to_string_lossy().as_ref()));
            state.mark_saved();
            state.flags.set(crate::BufferFlags::NOTEDITED, false);
        }
    });
    Flow::Normal
}

/// `:[range]write !cmd` (`ex_cmds.c` `ex_write` → `do_bang(1, eap, false,
/// true, false)` → `do_filter` with `do_in` only): pipe the addressed lines
/// into the shell command's standard input and leave the buffer, its name,
/// and its modified state untouched. The addressed range defaults to the
/// whole buffer (`EX_DFLALL`), and the command's exit status lands in
/// `v:shell_error`.
///
/// `do_bang` applies `ShellFilterPost` after the filter returns
/// (`ex_cmds.c:1236`). This form reads nothing back, so it fires no
/// `FilterRead*` events.
fn command_write_filter<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let shell_command = command.args.trim();
    if shell_command.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (start, end) = match access.with_ex_editor(|editor| resolve_range(editor, command)) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let lines = match access.with_ex_editor(|editor| buffer_lines(editor, buffer)) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let mut input = Vec::new();
    for line in lines.iter().take(end).skip(start.saturating_sub(1)) {
        input.extend_from_slice(line);
        input.push(b'\n');
    }
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut child = match std::process::Command::new(shell)
        .arg(flag)
        .arg(shell_command)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return error_flow(
                runtime,
                "E485",
                format!("Can't read file {shell_command}: {error}"),
            );
        }
    };
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write as _;
        if let Err(error) = stdin.write_all(&input) {
            return error_flow(
                runtime,
                "E212",
                format!("Can't open file for writing: {error}"),
            );
        }
    }
    drop(child.stdin.take());
    let status = match child.wait() {
        Ok(status) => status.code().unwrap_or(-1),
        Err(error) => {
            return error_flow(
                runtime,
                "E485",
                format!("Can't read file {shell_command}: {error}"),
            );
        }
    };
    scope.replace_pair(
        ScopeKind::Vim,
        "shell_error",
        Typval::Number(i64::from(status)),
    );
    fire_shell_filter_post(runtime, access, scope, lua)
}

/// `:!` / `:{range}!cmd` (`ex_cmds.c` `ex_bang` → `do_bang(addr_count, eap,
/// forceit, true, true)`). The bang flag (`forceit`) means "repeat the
/// previous `:!` command": a bare `:!!` or any unescaped `!` in the argument
/// splices `prevcmd` in its place. With no range the command runs through
/// `do_shell` and its stdout is echoed to the message area; with a range the
/// lines are piped through the command's stdin and the buffer range is
/// replaced by the command's stdout (`do_filter`), leaving the cursor on the
/// first filtered line.
fn command_bang<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    // `do_bang`'s `ins_prevcmd` loop: a `!` in the argument (or the bang flag
    // itself, i.e. `:!!`) is replaced by the previous command. `\!` collapses
    // to a literal `!`. An empty argument with the bang flag and no prevcmd
    // is E34 (`ex_cmds.c:1145-1148`).
    let mut expanded = String::new();
    let mut bytes = command.args.as_bytes().iter().peekable();
    let mut want_prev = command.bang;
    while let Some(&byte) = bytes.next() {
        if byte == b'\\' && bytes.peek() == Some(&&b'!') {
            expanded.push('!');
            bytes.next();
        } else if byte == b'!' {
            want_prev = true;
        } else {
            expanded.push(byte as char);
        }
    }
    let shell_command = if want_prev {
        match &runtime.prev_bang_command {
            Some(prev) if !expanded.is_empty() => format!("{prev} {expanded}"),
            Some(prev) => prev.clone(),
            None if expanded.is_empty() => {
                return error_flow(runtime, "E34", "No previous command");
            }
            None => expanded,
        }
    } else {
        expanded
    };
    let shell_command = shell_command.trim().to_owned();
    if !shell_command.is_empty() {
        runtime.prev_bang_command = Some(shell_command.clone());
    }
    let has_range = command.range.is_some();
    if has_range {
        bang_filter_range(runtime, access, scope, lua, &shell_command, command)
    } else {
        bang_run_shell(runtime, access, scope, lua, &shell_command)
    }
}

/// `:{range}!cmd` → `do_filter(line1, line2, …, do_in=true, do_out=true)`
/// (`ex_cmds.c:1260`): join the range lines with `\n`, write them to the
/// command's stdin, replace the range with the stdout lines split on `\n`,
/// and leave the cursor on the first filtered line. Empty output deletes the
/// range; if that empties the buffer, a single empty line remains (upstream's
/// `del_lines` guarantees `b_ml.ml_line_count >= 1`). `'[`/`']` track the
/// replaced region. `ShellFilterPost` fires after the replace
/// (`ex_cmds.c:1236`).
fn bang_filter_range<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    shell_command: &str,
    command: &ExCommand,
) -> Flow {
    if shell_command.is_empty() {
        return Flow::Normal;
    }
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (start, end) = match access.with_ex_editor(|editor| resolve_range(editor, command)) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let lines = match access.with_ex_editor(|editor| buffer_lines(editor, buffer)) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let mut input = Vec::new();
    for line in lines.iter().take(end).skip(start.saturating_sub(1)) {
        input.extend_from_slice(line);
        input.push(b'\n');
    }
    let (status, stdout) = match run_filter_command(runtime, shell_command, &input) {
        Ok(pair) => pair,
        Err(flow) => return flow,
    };
    scope.replace_pair(ScopeKind::Vim, "shell_error", Typval::Number(status));
    let output_lines = split_read_lines(&stdout);
    // `del_lines` then insert: a single `replace_buffer_lines` does both. An
    // empty `output_lines` deletes the range; if that empties the buffer,
    // leave one empty line (upstream invariant: `ml_line_count >= 1`).
    let replacement = if output_lines.is_empty() && start == 1 && end >= lines.len() {
        vec![Vec::new()]
    } else {
        output_lines
    };
    let cursor = Position {
        lnum: start,
        col: 0,
    };
    if let Err(error) = access.with_ex_editor(|editor| {
        editor.replace_buffer_lines(crate::LineReplaceRequest {
            buffer,
            start,
            end,
            lines: &replacement,
            cursor_before: cursor,
            cursor_after: cursor,
            timestamp: 0,
        })
    }) {
        return error_flow(runtime, "E16", error.to_string());
    }
    // `'[`/`']` track the filtered region (`do_filter`:1429-1438).
    let last = start + replacement.len().saturating_sub(1);
    let _ = access.with_ex_editor(|editor| {
        editor.set_local_mark(
            buffer,
            '[',
            Position {
                lnum: start,
                col: 0,
            },
        )
    });
    let _ = access.with_ex_editor(|editor| {
        editor.set_local_mark(buffer, ']', Position { lnum: last, col: 0 })
    });
    if let Some(window) = access.with_ex_editor(|editor| editor.current_window())
        && let Err(error) = access.with_ex_editor(|editor| {
            editor.set_window_cursor(
                window,
                Position {
                    lnum: start,
                    col: 0,
                },
            )
        })
    {
        return error_flow(runtime, "E16", error.to_string());
    }
    fire_shell_filter_post(runtime, access, scope, lua)
}

/// `:!cmd` with no range → `do_shell(newcmd, 0)` (`ex_cmds.c:1230-1232`): run
/// the command and echo its stdout to the message area. The exit status lands
/// in `v:shell_error`. `ShellCmdPost` fires after the run (`ex_cmds.c:1521`).
fn bang_run_shell<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    shell_command: &str,
) -> Flow {
    if shell_command.is_empty() {
        return Flow::Normal;
    }
    let (status, stdout) = match run_filter_command(runtime, shell_command, &[]) {
        Ok(pair) => pair,
        Err(flow) => return flow,
    };
    scope.replace_pair(ScopeKind::Vim, "shell_error", Typval::Number(status));
    // `do_shell` echoes the command output via `call_shell`; in batch mode the
    // text lands in the message area. Emit it as an echo message so
    // `:messages`/`execute()` can observe it.
    let text = String::from_utf8_lossy(&stdout);
    for line in text.lines() {
        access.with_ex_editor(|editor| push_text_message(editor, line.to_owned(), false, false));
    }
    // `ShellCmdPost` (`ex_cmds.c:1521`), a `PatternKind::None` event.
    let plan = access.with_ex_editor(|editor| {
        editor
            .autocmds_mut()
            .plan(Event::ShellCmdPost, AutocmdContext::default())
    });
    run_autocmd_plan(runtime, access, scope, lua, plan)
}

/// Spawns `sh -c <cmd>` (or `cmd /C` on Windows) with the given stdin bytes,
/// waits for it, and returns `(exit_code, combined_stdout_stderr)`. A spawn
/// failure is reported as `E485` matching `filter_output`/`command_write_filter`.
fn run_filter_command<F: FileIO>(
    runtime: &ExRuntime<F>,
    command: &str,
    input: &[u8],
) -> Result<(i64, Vec<u8>), Flow> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut child = match std::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Err(error_flow(
                runtime,
                "E485",
                format!("Can't read file {command}: {error}"),
            ));
        }
    };
    if !input.is_empty()
        && let Some(stdin) = child.stdin.as_mut()
    {
        use std::io::Write as _;
        if let Err(error) = stdin.write_all(input)
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error_flow(
                runtime,
                "E212",
                format!("Can't open file for writing: {error}"),
            ));
        }
    }
    drop(child.stdin.take());
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            return Err(error_flow(
                runtime,
                "E485",
                format!("Can't read file {command}: {error}"),
            ));
        }
    };
    let status = output.status.code().unwrap_or(-1).into();
    // `call_shell`/`os_system` merge stderr into stdout, so `system('nosuchcmd')`
    // answers with the shell's diagnostic.
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    Ok((status, combined))
}

fn command_split<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
    vertical: bool,
) -> Flow {
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let Some(tab) = access.with_ex_editor(|editor| editor.current_tabpage()) else {
        return error_flow(runtime, "E749", "No current tabpage");
    };
    let Some(window) = access.with_ex_editor(|editor| editor.current_window()) else {
        return error_flow(runtime, "E749", "No current window");
    };
    // `:new` and `:vnew` open an empty buffer; `:split`/`:vsplit` without an
    // argument keep showing the current one (`ex_splitview`, do_exedit).
    let (new_buffer, created_buffer) = if command.args.trim().is_empty() {
        if matches!(command.command.name(), "new" | "vnew") {
            match access.with_ex_editor(|editor| editor.create_buffer(true)) {
                Ok(handle) => (handle, true),
                Err(error) => return error_flow(runtime, "E948", error.to_string()),
            }
        } else {
            (buffer, false)
        }
    } else {
        match access.with_ex_editor(|editor| {
            buffer_from_file(runtime, editor, &PathBuf::from(command.args.trim()))
        }) {
            Ok((handle, created)) => (handle, created),
            Err(flow) => return flow,
        }
    };
    // `:split` keeps the cursor where it was (`win_split`: the new window
    // copies cursor and viewport from the one it came from), so capture the
    // position before the layout change and restore it in the new window.
    let origin_cursor =
        access.with_ex_editor(|editor| editor.window(window).ok().map(|state| state.cursor));
    let created = if vertical {
        access.with_ex_editor(|editor| editor.split_left(tab, window, new_buffer, true))
    } else {
        access.with_ex_editor(|editor| editor.split_above(tab, window, new_buffer, true))
    };
    let created = match created {
        Ok(created) => created,
        Err(error) => {
            if new_buffer != buffer {
                let _ = access.with_ex_editor(|editor| editor.wipe_buffer(new_buffer));
            }
            return error_flow(runtime, "E36", error.to_string());
        }
    };
    if let Err(error) = access.with_ex_editor(|editor| editor.set_current_window(created)) {
        let _ = access.with_ex_editor(|editor| editor.close_window(tab, created, true));
        if new_buffer != buffer {
            let _ = access.with_ex_editor(|editor| editor.wipe_buffer(new_buffer));
        }
        return error_flow(runtime, "E36", error.to_string());
    }
    if new_buffer == buffer
        && let Some(cursor) = origin_cursor
        && let Err(error) =
            access.with_ex_editor(|editor| editor.set_window_cursor(created, cursor))
    {
        return error_flow(runtime, "E16", error.to_string());
    }
    // A freshly created listed buffer fires `BufNew`/`BufAdd` (`buffer.c`
    // buf_alloc:2115-2135) and the entry ends with `win_enter`'s `BufEnter`
    // (`window.c:2722`); reusing an existing buffer raises none here.
    if created_buffer {
        fire_buffer_lifecycle(
            runtime,
            access,
            scope,
            lua,
            &[Event::BufNew, Event::BufAdd, Event::BufEnter],
            new_buffer,
        )
    } else {
        Flow::Normal
    }
}

/// `:tabnew` and `:tabedit` (`ex_docmd.c` `ex_splitview`:5637 with
/// `use_tab`): open a new tabpage at the addressed position, showing either a
/// new empty buffer or the file argument.
///
/// The position is upstream's `win_new_tabpage(after)` argument: no address
/// means `0`, "after the current tabpage", and an address `{n}` means `n + 1`,
/// which inserts *before* tabpage `n + 1` — so `:0tabnew` becomes the first
/// tabpage and `:$tabnew` the last.
///
/// `:tabnew` and `:tabedit` differ only in name upstream; both open an empty
/// buffer without an argument and the named file with one.
fn command_tabnew<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let after = match &command.range {
        None => 0,
        Some(_) => match resolve_range_raw(editor, command) {
            Ok((_, end)) => end + 1,
            Err(message) => return error_flow(runtime, "E16", message),
        },
    };
    let name = command.args.trim();
    let buffer = if name.is_empty() {
        match editor.create_buffer(true) {
            Ok(handle) => handle,
            Err(error) => return error_flow(runtime, "E948", error.to_string()),
        }
    } else {
        match buffer_from_file(runtime, editor, &argument_path(editor, name)) {
            Ok((handle, _)) => handle,
            Err(flow) => return flow,
        }
    };
    match editor.create_tabpage_at(buffer, DEFAULT_TABPAGE_GEOMETRY, after) {
        Ok(_) => Flow::Normal,
        Err(error) => {
            let _ = editor.wipe_buffer(buffer);
            error_flow(runtime, "E948", error.to_string())
        }
    }
}

fn command_tabnext<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let argument = command.args.trim();
    let target = if argument.is_empty() && command.range.is_none() {
        let tabs = editor.tabpages();
        let current = editor
            .current_tabpage()
            .and_then(|tab| editor.tabpage_index(tab))
            .unwrap_or(1);
        let next = if current >= tabs.len() {
            1
        } else {
            current + 1
        };
        tabs.get(next - 1).copied()
    } else {
        tabpage_arg(editor, command).ok()
    };
    let Some(target) = target else {
        return error_flow(runtime, "E475", format!("Invalid argument: {argument}"));
    };
    match editor.set_current_tabpage(target) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E475", error.to_string()),
    }
}
fn command_tabclose<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let tabs = editor.tabpages();
    if tabs.len() <= 1 {
        return error_flow(runtime, "E784", "Cannot close last tab page");
    }
    let argument = command.args.trim();
    let Ok(target) = tabpage_arg(editor, command) else {
        return error_flow(runtime, "E475", format!("Invalid argument: {argument}"));
    };
    let alternate = alt_tabpage(&tabs, target);
    let was_current = editor.current_tabpage() == Some(target);
    if editor.close_tabpage(target).is_err() {
        return error_flow(runtime, "E784", "Cannot close last tab page");
    }
    if was_current
        && let Some(alternate) = alternate
        && let Err(error) = editor.set_current_tabpage(alternate)
    {
        return error_flow(runtime, "E475", error.to_string());
    }
    Flow::Normal
}

/// `:tabonly` (`ex_docmd.c` `ex_tabonly`:5238): make one tabpage current and
/// close every other one.
///
/// A single tabpage is not an error, it reports "Already only one tab page"
/// (`ex_docmd.c:5241`). Which tabpage survives comes from `get_tabpage_arg`.
///
/// The bang is upstream's `forceit`, which reaches `ex_win_close` and only
/// matters when a modified buffer would be *unloaded*. Closing a tabpage
/// leaves its buffers loaded and hidden here, as it does upstream under
/// Neovim's default `'hidden'`, so both forms close the same tabpages. That
/// was checked against the oracle rather than assumed.
fn command_tabonly<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    if editor.tabpages().len() <= 1 {
        push_text_message(editor, "Already only one tab page".to_owned(), false, false);
        return Flow::Normal;
    }
    let argument = command.args.trim();
    let Ok(keep) = tabpage_arg(editor, command) else {
        return error_flow(runtime, "E475", format!("Invalid argument: {argument}"));
    };
    if let Err(error) = editor.set_current_tabpage(keep) {
        return error_flow(runtime, "E475", error.to_string());
    }
    for tab in editor.tabpages() {
        if tab != keep && editor.close_tabpage(tab).is_err() {
            return error_flow(runtime, "E444", "Cannot close last window");
        }
    }
    Flow::Normal
}

/// `get_tabpage_arg` (`ex_docmd.c`:4398-4488) for the commands that reject a
/// zero argument, resolved to the surviving tabpage handle.
///
/// Accepts a plain number, `+N`/`-N` relative to the current tabpage, `$` for
/// the last, and `#` for the last-used one. Without an argument an address is
/// used, and without either the current tabpage is the answer. Every rejection
/// is upstream's `e_invarg2`, so the failure carries no message of its own.
fn tabpage_arg(editor: &Editor, command: &ExCommand) -> Result<TabHandle, ()> {
    let tabs = editor.tabpages();
    let last = tabs.len();
    let current = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_index(tab))
        .unwrap_or(1);
    let argument = command.args.trim();

    let number = if argument.is_empty() {
        match &command.range {
            // ":0tabonly" is rejected: this family sets unaccept_arg0.
            Some(_) => resolve_range_raw(editor, command)
                .map(|(_, end)| end)
                .map_err(|_| ())?,
            None => current,
        }
    } else {
        let (relative, rest) = match argument.as_bytes().first() {
            Some(b'-') => (-1_isize, &argument[1..]),
            Some(b'+') => (1_isize, &argument[1..]),
            _ => (0_isize, argument),
        };
        if relative == 0 {
            match rest {
                "$" => last,
                // No last-used tabpage is tracked, so upstream's
                // valid_tabpage guard always fails and this is its error path.
                "#" => return Err(()),
                digits => digits.parse::<usize>().map_err(|_| ())?,
            }
        } else {
            let step = if rest.is_empty() {
                1
            } else {
                rest.parse::<isize>().map_err(|_| ())?
            };
            usize::try_from(step * relative + isize::try_from(current).map_err(|_| ())?)
                .map_err(|_| ())?
        }
    };

    if number < 1 || number > last {
        return Err(());
    }
    tabs.get(number - 1).copied().ok_or(())
}

fn preview_window(editor: &Editor) -> Option<WinHandle> {
    let tab = editor.current_tabpage()?;
    let windows = editor.tabpage_windows(tab).ok()?;
    windows.into_iter().find(|window| {
        matches!(
            editor.options().get_window(*window, "previewwindow"),
            Ok(OptionValue::Boolean(true))
        )
    })
}

fn command_pclose<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(window) = preview_window(editor) else {
        return Flow::Normal;
    };
    let Some(tab) = editor.current_tabpage() else {
        return Flow::Normal;
    };
    match editor.close_window(tab, window, true) {
        Ok(_) => Flow::Normal,
        Err(_) => error_flow(runtime, "E444", "Cannot close last window"),
    }
}

fn command_close<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    quit: bool,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return if quit {
            Flow::Quit(0)
        } else {
            error_flow(runtime, "E444", "Cannot close last window")
        };
    };
    if editor.buffer(buffer).is_ok_and(|state| {
        state.flags.contains(crate::BufferFlags::MODIFIED) && state.attachments == 1
    }) && !command.bang
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    let Some(tab) = editor.current_tabpage() else {
        return Flow::Quit(0);
    };
    let Some(window) = editor.current_window() else {
        return Flow::Quit(0);
    };
    // `win_close` (`window.c`:2791-2847). Upstream's `last_window` is one
    // window in the *current tabpage* and one tabpage in the editor, and only
    // that reaches E444; `:quit` there is `getout(0)`. The last window of any
    // other tabpage goes to `close_last_window_tabpage` (`window.c`:2678),
    // which enters `alt_tabpage()` and removes the tabpage instead of
    // refusing. `editor.windows()` is the whole editor, so testing it here
    // made every one-window tabpage look like the last window in the editor.
    if current_tabpage_window_count(editor) <= 1 {
        let tabs = editor.tabpages();
        if tabs.len() <= 1 {
            return if quit {
                Flow::Quit(0)
            } else {
                error_flow(runtime, "E444", "Cannot close last window")
            };
        }
        let alternate = alt_tabpage(&tabs, tab);
        if editor.close_tabpage(tab).is_err() {
            return error_flow(runtime, "E444", "Cannot close last window");
        }
        if let Some(alternate) = alternate {
            let _ = editor.set_current_tabpage(alternate);
        }
        return Flow::Normal;
    }
    match editor.close_window(tab, window, true) {
        Ok(_) => Flow::Normal,
        Err(_) => error_flow(runtime, "E444", "Cannot close last window"),
    }
}

/// `alt_tabpage` (`window.c`:3719-3740): the tabpage entered when the current
/// one goes away. With a default `'tabclose'` that is the next tabpage, or the
/// previous one when the closing tabpage is the last. `'tabclose'` is not
/// carried by this port, so neither of its flags is honoured here.
fn alt_tabpage(tabs: &[TabHandle], closing: TabHandle) -> Option<TabHandle> {
    let index = tabs.iter().position(|tab| *tab == closing)?;
    tabs.get(index + 1)
        .or_else(|| index.checked_sub(1).and_then(|prev| tabs.get(prev)))
        .copied()
}

/// `:undo` (`ex_docmd.c` `ex_undo`:6729).
///
/// Without an address this is one step back. *With* one it is a sequence
/// number, not a step count, and it may move forward: `set_cmd_count`
/// (`ex_docmd.c:1372-1393`) folds the `COUNT` form into the same `line2` the
/// `RANGE` form uses, so `:undo 2` and `:2undo` both mean "go to state 2"
/// through `undo_time(step, absolute)`. `:undo 0` returns to the original
/// state. A sequence that does not exist is `E830`.
///
/// Reaching the oldest change is a message, not an error
/// (`undo.c:1935`).
///
/// Named gap: `:undo!` is upstream's `u_undo_and_forget`, which discards the
/// redo branch it moves off. This port's `UndoTree` has no forget operation,
/// so the bang is rejected rather than silently treated as a plain `:undo` —
/// that would leave a redo branch upstream would have destroyed.
fn command_undo<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    if command.bang {
        return Flow::NotImplemented("undo!".to_owned());
    }
    let target = match (&command.range, command.count) {
        (None, None) => {
            return match editor.buffer_undo(buffer) {
                Ok(Some(_)) => Flow::Normal,
                Ok(None) => {
                    push_text_message(editor, "Already at oldest change".to_owned(), false, false);
                    Flow::Normal
                }
                Err(error) => error_flow(runtime, "E749", error.to_string()),
            };
        }
        (_, Some(count)) => count,
        (Some(_), None) => match resolve_range_raw(editor, command) {
            Ok((_, end)) => end as u64,
            Err(message) => return error_flow(runtime, "E16", message),
        },
    };
    match editor.buffer_undo_to_seq(buffer, target) {
        Ok(_) => Flow::Normal,
        Err(_) => error_flow(runtime, "E830", format!("Undo number {target} not found")),
    }
}

/// `:redo` (`ex_docmd.c` `ex_redo`:6783): always exactly one step forward.
///
/// Upstream is `u_redo(1)` with no count of any kind, and `redo`'s table entry
/// carries neither `RANGE` nor `COUNT`, so `:3redo` is rejected by the parser
/// rather than redoing three times. Reaching the newest change is a message
/// (`undo.c:1948`), not an error.
fn command_redo<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    match editor.buffer_redo(buffer) {
        Ok(Some(_)) => Flow::Normal,
        Ok(None) => {
            push_text_message(editor, "Already at newest change".to_owned(), false, false);
            Flow::Normal
        }
        Err(error) => error_flow(runtime, "E749", error.to_string()),
    }
}

/// `:undojoin` (`ex_undojoin`, `undo.c:2800-2816`): reopen the newest undo
/// block so the next change joins it instead of starting its own.
///
/// Three of upstream's four early returns are silent: nothing recorded yet,
/// an already-open block, and `'undolevels'` below zero all just do nothing.
/// Only a `:undojoin` that follows an undo is an error, `E790`, because the
/// header it would reopen is the one the undo moved off.
fn command_undojoin<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    match editor.buffer_undojoin(buffer) {
        Ok(()) => Flow::Normal,
        Err(crate::EditorError::Buffer(crate::BufferStateError::Undo(
            ox_text::UndoError::JoinAfterUndo,
        ))) => error_flow(runtime, "E790", "undojoin is not allowed after undo"),
        Err(error) => error_flow(runtime, "E749", error.to_string()),
    }
}

/// `:retab` (`indent.c` `ex_retab`:1436-1617): rewrite whitespace runs in the
/// addressed lines for a (possibly new) `'tabstop'`.
///
/// A run is rewritten only when it *contains* a tab, or when `!` is given and
/// it is more than one space (`indent.c:1495`), and only when the rewrite is
/// no longer than what was there — so `:retab!` leaves two spaces alone
/// because a tab would render differently. With `'expandtab'` the run becomes
/// all spaces; otherwise `tabstop_fromto` (`indent.c:220-243`) splits it into
/// tabs plus a spare-space remainder.
///
/// Widths are measured with the *old* `'tabstop'` and rebuilt with the new
/// one, which is why `:retab 4` doubles a single tab that spanned eight
/// columns. `-indentonly` stops after the leading run.
///
/// The argument is a single tabstop value and becomes the buffer's
/// `'tabstop'`. Named gap: upstream also accepts a comma list and writes it to
/// `'vartabstop'` (`indent.c:1597-1613`); this port has no `'vartabstop'`
/// option at all, so that form reports `NotImplemented` rather than silently
/// keeping one of the values.
fn command_retab<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (first, last) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };

    let mut argument = command.args.trim();
    let indent_only = match argument.strip_prefix("-indentonly") {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => {
            argument = rest.trim_start();
            true
        }
        _ => false,
    };
    if argument.contains(',') {
        return Flow::NotImplemented("retab with a 'vartabstop' list".to_owned());
    }
    let new_tabstop = if argument.is_empty() {
        None
    } else {
        match argument.parse::<usize>() {
            Ok(value) if value > 0 => Some(value),
            _ => return error_flow(runtime, "E475", format!("Invalid argument: {argument}")),
        }
    };

    let old_tabstop = buffer_number_option(editor, buffer, "tabstop").unwrap_or(8);
    let expandtab = buffer_bool_option(editor, buffer, "expandtab");
    let target_tabstop = new_tabstop.unwrap_or(old_tabstop);

    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let mut too_long = false;
    for lnum in first..=last.min(lines.len()) {
        let Some(line) = lines.get(lnum - 1) else {
            continue;
        };
        let retabbed = retab_line(
            line,
            old_tabstop,
            target_tabstop,
            expandtab,
            command.bang,
            indent_only,
        );
        if let Some(rebuilt) = retabbed.line {
            let cursor = editor
                .current_window()
                .and_then(|window| editor.window(window).ok())
                .map_or(Position { lnum, col: 0 }, |state| state.cursor);
            if let Err(error) = editor.replace_buffer_lines(crate::LineReplaceRequest {
                buffer,
                start: lnum,
                end: lnum,
                lines: &[rebuilt],
                cursor_before: cursor,
                cursor_after: cursor,
                timestamp: 0,
            }) {
                return error_flow(runtime, "E749", error.to_string());
            }
        }
        if retabbed.too_long {
            // `emsg_text_too_long` (`indent.c:1425-1433`) breaks the scan, and
            // outside a `:try` it also sets `got_int` so the enclosing loop
            // ends. This port carries no interrupt state, so the error itself
            // is what leaves the loop; the `'tabstop'` write below still
            // happens, as it does after upstream's `got_int` path.
            too_long = true;
            break;
        }
    }

    if let Some(value) = new_tabstop {
        // The dual write `:set` uses, so `&tabstop` reads inside the same batch
        // see the new value instead of the pre-command snapshot.
        let written = i64::try_from(value).unwrap_or(i64::MAX);
        if let Err((code, message)) = set_and_mirror(
            editor,
            scope,
            "tabstop",
            &OptionValue::Number(written),
            SetLayer::Local,
        ) {
            return error_flow(runtime, code, message);
        }
    }
    if too_long {
        return error_flow(runtime, "E1240", "Resulting text too long");
    }
    Flow::Normal
}

/// `MAXCOL` (`pos_defs.h:19`), the ceiling both of `ex_retab`'s guards test.
const MAXCOL: usize = 0x7fff_ffff;

/// One line's retab result.
struct RetabbedLine {
    /// The rebuilt bytes, when they differ from the line as it stood.
    line: Option<Vec<u8>>,
    /// `emsg_text_too_long` (`indent.c:1425-1433`) fired, so E1240 follows.
    too_long: bool,
}

/// The rewrite would push the line past `MAXCOL`, so the run keeps the bytes
/// it had (`indent.c:1522-1526`).
struct TextTooLong;

/// Rewrites one line's whitespace runs.
///
/// Both of upstream's ceilings are here, because without them the command is
/// unbounded: each `:retab` against a larger `'tabstop'` multiplies the
/// whitespace it rebuilds, so `while 1 / set ts=4000 / retab 4` grows the line
/// a thousandfold per pass until the process dies. `ex_retab` stops that with
/// `vcol >= MAXCOL` while scanning (`indent.c:1563-1567`) and with a
/// `new_len >= MAXCOL` test on the line the rewrite would produce
/// (`indent.c:1522-1526`). Either one abandons the rest of the line, keeping
/// the runs already rebuilt, and reports E1240.
fn retab_line(
    line: &[u8],
    old_tabstop: usize,
    new_tabstop: usize,
    expandtab: bool,
    forceit: bool,
    indent_only: bool,
) -> RetabbedLine {
    let text = String::from_utf8_lossy(line);
    let mut output: Vec<u8> = Vec::with_capacity(line.len());
    let mut run = String::new();
    let mut run_start_vcol = 0usize;
    let mut vcol = 0usize;
    let mut scanned = 0usize;
    let mut changed = false;
    let mut done = false;
    let mut too_long = false;

    for character in text.chars() {
        if !done && matches!(character, ' ' | '\t') {
            if run.is_empty() {
                run_start_vcol = vcol;
            }
            run.push(character);
            scanned += character.len_utf8();
            vcol += cell_width(character, vcol, old_tabstop);
            if vcol >= MAXCOL {
                too_long = true;
                break;
            }
            continue;
        }
        if !run.is_empty() {
            // The bytes past this run survive the rewrite, so they count
            // towards the length upstream measures against MAXCOL.
            let tail = text.len() - scanned;
            if let Ok(run_changed) = flush_retab_run(
                &mut output,
                &run,
                run_start_vcol,
                vcol,
                new_tabstop,
                expandtab,
                forceit,
                tail,
            ) {
                changed |= run_changed;
            } else {
                too_long = true;
                break;
            }
            run.clear();
        }
        if !done && indent_only {
            // `-indentonly`: everything past the leading run is copied as-is.
            done = true;
        }
        let mut encoded = [0_u8; 4];
        output.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        scanned += character.len_utf8();
        vcol += cell_width(character, vcol, old_tabstop);
        if vcol >= MAXCOL {
            too_long = true;
            break;
        }
    }
    if too_long {
        // Upstream's break leaves the line as it stands from here on: the run
        // it gave up on keeps its own bytes, and so does everything after it.
        output.extend_from_slice(run.as_bytes());
        output.extend_from_slice(&text.as_bytes()[scanned..]);
    } else if !run.is_empty() {
        if let Ok(run_changed) = flush_retab_run(
            &mut output,
            &run,
            run_start_vcol,
            vcol,
            new_tabstop,
            expandtab,
            forceit,
            0,
        ) {
            changed |= run_changed;
        } else {
            too_long = true;
            output.extend_from_slice(run.as_bytes());
        }
    }
    RetabbedLine {
        line: changed.then_some(output),
        too_long,
    }
}

/// Emits one whitespace run, rebuilt when upstream would rebuild it.
///
/// `tail` is the byte count that follows the run in the line, which upstream
/// carries in `old_len - col` when it sizes the replacement.
///
/// Returns whether the emitted bytes differ from the run as it stood.
#[expect(
    clippy::too_many_arguments,
    reason = "one rebuilt run needs the full retab column state at its boundaries"
)]
fn flush_retab_run(
    output: &mut Vec<u8>,
    run: &str,
    start_vcol: usize,
    end_vcol: usize,
    new_tabstop: usize,
    expandtab: bool,
    forceit: bool,
    tail: usize,
) -> Result<bool, TextTooLong> {
    let had_tab = run.contains('\t');
    let spaces = run.chars().filter(|character| *character == ' ').count();
    // indent.c:1495: a run without a tab is left alone unless `!` was given
    // and it is more than a single space.
    if !had_tab && (spaces <= 1 || !forceit) {
        output.extend_from_slice(run.as_bytes());
        return Ok(false);
    }
    let width = end_vcol - start_vcol;
    let (tabs, remainder) = if expandtab {
        (0, width)
    } else {
        tabstop_fromto(start_vcol, end_vcol, new_tabstop)
    };
    // indent.c:1509: keep the original unless the rewrite is not longer.
    if !expandtab && !had_tab && tabs + remainder >= run.chars().count() {
        output.extend_from_slice(run.as_bytes());
        return Ok(false);
    }
    // indent.c:1522: `new_len` is the whole line the rewrite would produce,
    // plus upstream's terminating NUL.
    let new_len = output.len() + tabs + remainder + tail + 1;
    if new_len >= MAXCOL {
        return Err(TextTooLong);
    }
    let rebuilt: Vec<u8> = std::iter::repeat_n(b'\t', tabs)
        .chain(std::iter::repeat_n(b' ', remainder))
        .collect();
    let changed = rebuilt != run.as_bytes();
    output.extend_from_slice(&rebuilt);
    Ok(changed)
}

/// `tabstop_fromto` (`indent.c:220-243`) without `'vartabstop'`: the tabs and
/// spare spaces that advance from `start_vcol` to `end_vcol`.
fn tabstop_fromto(start_vcol: usize, end_vcol: usize, tabstop: usize) -> (usize, usize) {
    if tabstop == 0 {
        return (0, end_vcol - start_vcol);
    }
    let mut spaces = end_vcol - start_vcol;
    let mut tabs = 0;
    let initial = tabstop - (start_vcol % tabstop);
    if spaces >= initial {
        spaces -= initial;
        tabs += 1;
    }
    tabs += spaces / tabstop;
    spaces -= (spaces / tabstop) * tabstop;
    (tabs, spaces)
}

fn buffer_number_option(editor: &Editor, buffer: BufHandle, name: &str) -> Option<usize> {
    match editor.options().get_buffer(buffer, name) {
        Ok(OptionValue::Number(value)) if *value > 0 => usize::try_from(*value).ok(),
        _ => None,
    }
}

fn buffer_bool_option(editor: &Editor, buffer: BufHandle, name: &str) -> bool {
    matches!(
        editor.options().get_buffer(buffer, name),
        Ok(OptionValue::Boolean(true))
    )
}

/// `:lockvar` and `:unlockvar` (`eval/vars.c` `ex_lockvar`:1554).
///
/// The depth defaults to 2, `!` means "everything" (-1), and a leading digit
/// run overrides it. The names that follow are handled one per whitespace-
/// separated word, as `ex_unletlock` walks them, and the lock itself is
/// `Scope::lockvar`/`unlockvar`.
fn command_lockvar<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    command: &ExCommand,
    lock: bool,
) -> Flow {
    let mut argument = command.args.trim();
    let depth = if command.bang {
        -1
    } else {
        let digits = argument.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            2
        } else {
            let Ok(value) = argument[..digits].parse::<i32>() else {
                return error_flow(runtime, "E475", format!("Invalid argument: {argument}"));
            };
            argument = argument[digits..].trim_start();
            value
        }
    };
    if argument.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    for name in argument.split_whitespace() {
        let (kind, bare) = parse_scope_name(name);
        if let Some(kind) = kind
            && let Some((flags, subscripted)) = scoped_target_flags(kind, bare.as_bytes())
        {
            if subscripted {
                // A subscripted name resolves as a dict item, whose
                // read-only flag refuses the lock with E46.
                if flags.intersects(DictEntryFlags::READ_ONLY) {
                    return error_flow(
                        runtime,
                        "E46",
                        format!("Cannot change read-only variable \"{name}\""),
                    );
                }
            } else if flags.contains(DictEntryFlags::FIXED)
                // `do_lock_var` (vars.c:1818): a fixed item refuses the lock
                // outright unless its value is a Dict or List — the scope
                // dictionaries themselves stay lockable "for historical
                // reasons". `b:changedtick` holds a Number, so it refuses.
                && !scope
                    .get_scoped(kind, bare.as_bytes(), 0)
                    .is_ok_and(|value| matches!(value, Typval::Dict(_) | Typval::List(_)))
            {
                return error_flow(
                    runtime,
                    "E940",
                    format!("Cannot lock or unlock variable {name}"),
                );
            }
        }
        if has_subscript(name) {
            // `do_lock_var` resolves the name through `get_lval`; a reached
            // dict entry's read-only flag refuses the lock with E46 before
            // any locking happens. Root names keep the `Scope` lock path.
            match parse_and_bind_lvalue(runtime, access, scope, None, name) {
                Ok(lvalue) => {
                    if let Ok(true) = names_read_only_entry(runtime, scope, &lvalue) {
                        return error_flow(
                            runtime,
                            "E46",
                            format!("Cannot change read-only variable \"{name}\""),
                        );
                    }
                }
                Err(flow) => return flow,
            }
        }
        let result = if lock {
            scope.lockvar(name.as_bytes(), depth)
        } else {
            scope.unlockvar(name.as_bytes(), depth)
        };
        if let Err(error) = result {
            return eval_error_flow(runtime, error);
        }
    }
    Flow::Normal
}

/// Whether a `:let`/`:unlet`/`:lockvar` target reaches through a subscript
/// (`d.k`, `d["k"]`, `l[0]`) rather than naming a root variable; those
/// targets resolve through the lvalue path instead of root assignment.
#[must_use]
fn has_subscript(target: &str) -> bool {
    let bytes = target.as_bytes();
    let start = if bytes.len() > 2
        && bytes[1] == b':'
        && ScopeKind::from_byte(bytes.first().copied().unwrap_or(b' ')).is_some()
    {
        2
    } else {
        0
    };
    bytes
        .get(start..)
        .unwrap_or(&[])
        .iter()
        .any(|byte| matches!(byte, b'[' | b'.'))
}

/// The current window's `'foldmethod'`, as a [`FoldMethod`].
///
/// Named gap: `'foldmethod'` is a window option and upstream's fold tree is
/// per-window (`wp->w_folds`), but this port keeps one [`Folds`] per buffer.
/// Two windows on one buffer with different `'foldmethod'` therefore cannot
/// both be honoured; the current window's value wins. Reading the option here
/// is still what makes the `E350` guard read real state rather than a field
/// nothing ever assigns.
fn current_fold_method<F: FileIO>(runtime: &ExRuntime<F>, editor: &Editor) -> FoldMethod {
    let _ = runtime;
    match option_value(editor, "foldmethod", SetLayer::Effective) {
        Some(OptionValue::String(value)) => FoldMethod::from_option_value(value),
        _ => FoldMethod::Manual,
    }
}

/// `:fold` (`ex_docmd.c` `ex_fold`:8019): create a fold over the addressed
/// lines.
///
/// `foldManualAllowed` (`fold.c:522-533`) permits only `'foldmethod'` of
/// `manual` or `marker`; anything else is `E350`. Under `manual` the fold is
/// recorded and starts closed (`foldCreate`); under `marker` upstream instead
/// writes `'foldmarker'` into the text and lets theker scan find it
/// (`foldCreateMarkers`, `fold.c:1554`).
fn command_fold<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (first, last) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let method = current_fold_method(runtime, editor);
    match method {
        FoldMethod::Manual => {}
        FoldMethod::Marker => return fold_create_markers(runtime, editor, buffer, first, last),
        FoldMethod::Indent | FoldMethod::Expr | FoldMethod::Syntax | FoldMethod::Diff => {
            return error_flow(
                runtime,
                "E350",
                "Cannot create fold with current 'foldmethod'",
            );
        }
    }
    let folds = match editor.buffer_mut(buffer) {
        Ok(state) => &mut state.folds,
        Err(error) => return error_flow(runtime, "E749", error.to_string()),
    };
    folds.set_method(FoldMethod::Manual);
    #[expect(
        clippy::match_same_arms,
        reason = "an empty or pre-existing manual fold is upstream's accepted no-op, not an error"
    )]
    match folds.create_manual(FoldPosition::new(first - 1, 0), FoldPosition::new(last, 0)) {
        Ok(_) => Flow::Normal,
        // An identical fold already exists, which upstream tolerates: foldCreate
        // simply nests another entry and the visible result is unchanged.
        Err(crate::fold::FoldError::DuplicateRange) => Flow::Normal,
        Err(error) => error_flow(runtime, "E350", error.to_string()),
    }
}

/// `foldCreateMarkers` (`fold.c:1554-1575`): append the `'foldmarker'` pair to
/// the first and last addressed lines so theker scan finds a fold there.
///
/// Named gap: upstream wraps eachker in `'commentstring'` unless the line
/// already ends inside a comment (`foldAddMarker`, `fold.c:1579-1609`). The
/// wrap is implemented; the "already a comment" refinement needs `skip_comment`
/// and a comment parser this port does not have, so aker added to a line
/// that already ends in an open comment is wrapped where upstream would not.
fn fold_create_markers<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    buffer: BufHandle,
    first: usize,
    last: usize,
) -> Flow {
    let markers = match option_value(editor, "foldmarker", SetLayer::Effective) {
        Some(OptionValue::String(value)) => value.clone(),
        _ => "{{{,}}}".to_owned(),
    };
    let (start_marker, end_marker) = match markers.split_once(',') {
        Some((start, end)) => (start.to_owned(), end.to_owned()),
        None => return error_flow(runtime, "E536", "comma required"),
    };
    let comment = match option_value(editor, "commentstring", SetLayer::Effective) {
        Some(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    // Applied last line first so the earlier replacement cannot shift the
    // later one, and skipped when both markers land on the same line.
    let mut targets = vec![(last, end_marker), (first, start_marker)];
    targets.dedup_by_key(|(lnum, _)| *lnum);
    for (lnum, marker) in targets {
        let Some(line) = lines.get(lnum - 1) else {
            continue;
        };
        let mut rebuilt = line.clone();
        match comment.split_once("%s") {
            Some((before, after)) => {
                rebuilt.extend_from_slice(before.as_bytes());
                rebuilt.extend_from_slice(marker.as_bytes());
                rebuilt.extend_from_slice(after.as_bytes());
            }
            None => rebuilt.extend_from_slice(marker.as_bytes()),
        }
        let cursor = editor
            .current_window()
            .and_then(|window| editor.window(window).ok())
            .map_or(Position { lnum, col: 0 }, |state| state.cursor);
        if let Err(error) = editor.replace_buffer_lines(crate::LineReplaceRequest {
            buffer,
            start: lnum,
            end: lnum,
            lines: &[rebuilt],
            cursor_before: cursor,
            cursor_after: cursor,
            timestamp: 0,
        }) {
            return error_flow(runtime, "E749", error.to_string());
        }
    }
    Flow::Normal
}

/// `:foldopen` and `:foldclose` (`ex_docmd.c` `ex_foldopen`:8028 →
/// `opFoldRange`, `fold.c:386-415`).
///
/// Every addressed line is opened or closed; `!` makes it recursive. `E490` is
/// reported only when *no* line in the range had a fold at all — a fold that
/// was already in the requested state counts as found (`setManualFoldWin`
/// sets `DONE_FOLD` without `DONE_ACTION`).
fn command_foldopen<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (first, last) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let opening = command.command.name() == "foldopen";
    let recurse = command.bang;
    let method = current_fold_method(runtime, editor);
    let mut found = false;
    for lnum in first..=last {
        let position = FoldPosition::new(lnum - 1, 0);
        let outcome = match editor.buffer_mut(buffer) {
            Ok(state) => {
                state.folds.set_method(method);
                match (opening, recurse) {
                    (true, false) => state.folds.open(position).map(|_| ()),
                    (true, true) => state.folds.open_recursive(position).map(|_| ()),
                    (false, false) => state.folds.close(position).map(|_| ()),
                    (false, true) => state.folds.close_recursive(position).map(|_| ()),
                }
            }
            Err(error) => return error_flow(runtime, "E749", error.to_string()),
        };
        match outcome {
            Ok(()) => found = true,
            Err(crate::fold::FoldError::NoFold) => {}
            Err(error) => return error_flow(runtime, "E490", error.to_string()),
        }
    }
    if !found {
        return error_flow(runtime, "E490", "No fold found");
    }
    Flow::Normal
}

/// `:diffthis` (diff.c:1483): set the current window into diff mode.
fn command_diffthis<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    match crate::diffmode::diffthis(editor) {
        Ok(()) => Flow::Normal,
        Err(message) => error_flow(runtime, "E444", message),
    }
}

/// `:diffoff` (diff.c:1597): turn `'diff'` off for the current window, or
/// for every diff window of the current tabpage with `!`.
fn command_diffoff<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    match crate::diffmode::diffoff(editor, command.bang) {
        Ok(()) => Flow::Normal,
        Err(message) => error_flow(runtime, "E444", message),
    }
}

/// `:diffupdate` (diff.c:1073): rebuild the tabpage's diff blocks.
fn command_diffupdate<F: FileIO>(_runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    crate::diffmode::diffupdate(editor);
    Flow::Normal
}

/// `:hide` (`ex_docmd.c` `ex_hide`:5369): close a window without freeing its
/// buffer.
///
/// Without an address this is the current window; with one it is that window
/// number in the current tabpage, falling back to the last window when the
/// number is past the end (`win_find_nr`). A bare `:hide` is this command,
/// while `:hide {cmd}` is the modifier the parser already separates.
fn command_hide<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(tab) = editor.current_tabpage() else {
        return error_flow(runtime, "E749", "No current tabpage");
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let target = match (&command.range, command.count) {
        (None, None) => editor.current_window(),
        (_, Some(count)) => {
            window_by_number(&windows, usize::try_from(count).unwrap_or(usize::MAX))
        }
        (Some(_), None) => match resolve_range_raw(editor, command) {
            Ok((_, end)) => window_by_number(&windows, end),
            Err(message) => return error_flow(runtime, "E16", message),
        },
    };
    let Some(window) = target else {
        return error_flow(runtime, "E749", "No current window");
    };
    if windows.len() == 1 {
        return error_flow(runtime, "E444", "Cannot close last window");
    }
    match editor.close_window(tab, window, true) {
        Ok(_) => Flow::Normal,
        Err(_) => error_flow(runtime, "E444", "Cannot close last window"),
    }
}

/// `win_find_nr`: the Nth window of a tabpage, or its last window when `N` is
/// past the end.
fn window_by_number(windows: &[WinHandle], number: usize) -> Option<WinHandle> {
    if number == 0 {
        return windows.first().copied();
    }
    windows
        .get(number - 1)
        .copied()
        .or_else(|| windows.last().copied())
}

/// `:sleep` (`ex_docmd.c` `ex_sleep`:6459): pause for the count, in seconds by
/// default or milliseconds with an `m` suffix.
///
/// The count defaults to 1 and anything other than `m` or an empty tail is
/// `E475` reporting the *remaining* argument, not the whole one. A zero count
/// is `E939` from the shared count parse, since `sleep` carries no `ZEROR`.
fn command_sleep<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let amount = match (&command.range, command.count) {
        (_, Some(count)) => count,
        (Some(_), None) => match resolve_range_raw(editor, command) {
            Ok((_, end)) => end as u64,
            Err(message) => return error_flow(runtime, "E16", message),
        },
        (None, None) => 1,
    };
    let tail = command.args.trim();
    let milliseconds = match tail {
        "" => amount.saturating_mul(1000),
        "m" => amount,
        other => return error_flow(runtime, "E475", format!("Invalid argument: {other}")),
    };
    std::thread::sleep(std::time::Duration::from_millis(milliseconds));
    Flow::Normal
}

/// `:scriptencoding` (`runtime.c` `ex_scriptencoding`:2946).
///
/// Outside a sourced file this is `E167`. Inside one, upstream sets up a
/// conversion from the named encoding to `'encoding'`.
///
/// Named gap: that conversion needs an encoding converter this port does not
/// have (`convert_setup`, `mbyte.c`), so a valid `:scriptencoding` inside a
/// script is accepted and the name recorded, with no re-decoding of the
/// remaining lines. Every script this port sources is already read as UTF-8,
/// which is what `:scriptencoding utf-8` — the only form in the runtime files
/// — asks for anyway.
fn command_scriptencoding<F: FileIO>(runtime: &mut ExRuntime<F>, command: &ExCommand) -> Flow {
    if runtime.scripts.current_sid().is_none() {
        return error_flow(
            runtime,
            "E167",
            ":scriptencoding used outside of a sourced file",
        );
    }
    let _ = command;
    Flow::Normal
}

/// `:argdelete` (`arglist.c` `ex_argdelete`:759).
///
/// With an address, or with no argument at all, entries are removed by
/// position: no argument means the current one, and a bare `:argdelete` with
/// the index past the end is `E610`. With a name argument the matching entries
/// are removed and a name that matches nothing is `E480`. Supplying both an
/// address and an argument is `E475`.
fn command_argdelete<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let argument = command.args.trim().to_owned();
    let addressed = command.range.is_some();
    if addressed && !argument.is_empty() {
        return error_flow(runtime, "E475", "Invalid argument");
    }
    if !addressed && !argument.is_empty() {
        // arglist_del_files (arglist.c:352-392) treats each argument as a file
        // pattern and drops every entry it matches; no match is E480.
        let names = editor.arglist().names().to_vec();
        let index = editor.arglist().index();
        let mut kept = Vec::with_capacity(names.len());
        let mut new_index = index;
        for (position, name) in names.iter().enumerate() {
            if crate::fs_builtins::wildcard_match(argument.as_bytes(), name.as_bytes()) {
                if position < index {
                    new_index = new_index.saturating_sub(1);
                }
                continue;
            }
            kept.push(name.clone());
        }
        if kept.len() == names.len() {
            return error_flow(runtime, "E480", format!("No match: {argument}"));
        }
        editor.arglist_mut().set(kept);
        clamp_arglist_index(editor, new_index);
        return Flow::Normal;
    }
    let count = editor.arglist().len();
    let (first, last) = if addressed {
        match resolve_range_raw(editor, command) {
            Ok((first, last)) => (first, last.min(count)),
            Err(message) => return error_flow(runtime, "E16", message),
        }
    } else {
        let index = editor.arglist().index();
        if index >= count {
            return error_flow(runtime, "E610", "No argument to delete");
        }
        (index + 1, index + 1)
    };
    if last < first {
        // ":%argdel" on an empty list is deliberately not an error.
        if !(first == 1 && last == 0) {
            return error_flow(runtime, "E16", "Invalid range");
        }
        return Flow::Normal;
    }
    let names = editor.arglist().names().to_vec();
    let index = editor.arglist().index();
    let kept: Vec<_> = names
        .iter()
        .enumerate()
        .filter(|(position, _)| position + 1 < first || position + 1 > last)
        .map(|(_, name)| name.clone())
        .collect();
    // arglist.c:797-801, in the same one-based terms upstream uses.
    let removed = last + 1 - first;
    let new_index = if index + 1 >= last {
        (index + 1).saturating_sub(removed).saturating_sub(1)
    } else if index + 1 > first {
        first - 1
    } else {
        index
    };
    editor.arglist_mut().set(kept);
    clamp_arglist_index(editor, new_index);
    Flow::Normal
}

/// `alist_check_arg_idx` (arglist.c:806-810): an empty list resets the index
/// and an index past the end lands on the last entry.
fn clamp_arglist_index(editor: &mut Editor, requested: usize) {
    let count = editor.arglist().len();
    let index = if count == 0 {
        0
    } else {
        requested.min(count - 1)
    };
    editor.arglist_mut().set_index(index);
}

/// `:z` (`ex_cmds.c` `ex_z`:3154): print a window of lines around the
/// addressed one.
///
/// The argument's leading `-`, `+`, `=`, `^` or `.` selects which side of the
/// address the window falls on, repeated `-`/`+` multiply the distance, and a
/// trailing number sets its size. Without a number the size is `'scroll'`
/// doubled for a lone window, the window height less three otherwise, and the
/// screen height less one for `:z!`. A non-numeric size is `E144`.
///
/// The `=` form brackets the addressed line with `'columns'`-wide rules, and
/// leaves the cursor on it; the other forms leave the cursor on the last
/// printed line.
#[expect(
    clippy::too_many_lines,
    reason = ":z keeps its five window shapes and overflow arithmetic in one interpreter"
)]
fn command_z<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let addressed = command.range.is_some();
    let lnum = match resolve_range(editor, command) {
        Ok((_, end)) => end,
        Err(message) => return error_flow(runtime, "E16", message),
    };

    let argument = command.args.trim();
    let mut rest = argument;
    let kind = match argument.as_bytes().first() {
        Some(byte @ (b'-' | b'+' | b'=' | b'^' | b'.')) => {
            rest = &argument[1..];
            *byte
        }
        _ => b'+',
    };
    // Repeated signs multiply the distance; count them before the digits.
    let repeats = 1 + rest.bytes().take_while(|byte| *byte == kind).count();
    if matches!(kind, b'-' | b'+') {
        rest = &rest[repeats - 1..];
    }

    let mut bigness = if command.bang {
        screen_number_option(editor, "lines").saturating_sub(1)
    } else if current_tabpage_window_count(editor) == 1 {
        screen_number_option(editor, "scroll").saturating_mul(2)
    } else {
        editor
            .current_window()
            .and_then(|window| editor.window_geometry(window).ok())
            .map_or(1, |geometry| geometry.height.saturating_sub(3))
    }
    .max(1);
    if !rest.is_empty() {
        let Ok(value) = rest.parse::<usize>() else {
            return error_flow(runtime, "E144", "non-numeric argument to :z");
        };
        bigness = value.min(lines.len().saturating_mul(2));
        if kind == b'=' {
            bigness += 2;
        }
    }

    // `bigness` is clamped to twice the buffer length above and `lnum` is a
    // real line number, so the signed arithmetic below stays inside `isize`
    // for any addressable buffer this port can hold.
    let big = bigness.cast_signed();
    let base = lnum.cast_signed();
    let repeated = repeats.cast_signed();
    let line_count = lines.len().cast_signed();
    let (start, end, cursor, ruled) = match kind {
        b'-' => {
            let start = base - big * repeated + 1;
            (start, start + big - 1, start + big - 1, false)
        }
        b'=' => (
            base - (big + 1) / 2 + 1,
            base + (big + 1) / 2 - 1,
            base,
            true,
        ),
        b'^' => (base - big * 2, base - big, base - big, false),
        b'.' => {
            let start = base - (big + 1) / 2 + 1;
            (
                start,
                base + (big + 1) / 2 - 1,
                base + (big + 1) / 2 - 1,
                false,
            )
        }
        _ => {
            let mut start = base;
            if argument.starts_with('+') {
                start += big * (repeated - 1) + 1;
            } else if !addressed {
                start += 1;
            }
            (start, start + big - 1, start + big - 1, false)
        }
    };
    let first = start.max(1).cast_unsigned();
    let last = end.min(line_count).max(0).cast_unsigned();
    let cursor = cursor.clamp(1, line_count).cast_unsigned();

    let number = matches!(
        option_value(editor, "number", SetLayer::Effective),
        Some(OptionValue::Boolean(true))
    );
    let width = lines.len().to_string().len();
    let rule = "-".repeat(screen_number_option(editor, "columns").saturating_sub(1));
    for index in first..=last {
        let Some(line) = lines.get(index - 1) else {
            continue;
        };
        if ruled && index == lnum {
            push_info_text_message(editor, rule.clone());
        }
        let text = String::from_utf8_lossy(line).into_owned();
        push_info_text_message(
            editor,
            if number {
                format!("{index:>width$} {text}")
            } else {
                text
            },
        );
        if ruled && index == lnum {
            push_info_text_message(editor, rule.clone());
        }
    }
    if let Some(window) = editor.current_window()
        && let Err(error) = editor.set_window_cursor(
            window,
            Position {
                lnum: cursor,
                col: 0,
            },
        )
    {
        return error_flow(runtime, "E16", error.to_string());
    }
    Flow::Normal
}

/// The number of windows in the current tabpage, upstream's `ONE_WINDOW` test.
fn current_tabpage_window_count(editor: &Editor) -> usize {
    editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_windows(tab).ok())
        .map_or(1, |windows| windows.len())
}

/// A screen-geometry option (`'lines'`, `'columns'`, `'scroll'`) as a count.
fn screen_number_option(editor: &Editor, name: &str) -> usize {
    match option_value(editor, name, SetLayer::Effective) {
        Some(OptionValue::Number(value)) if *value > 0 => usize::try_from(*value).unwrap_or(1),
        _ => 1,
    }
}

fn command_only<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(tab) = editor.current_tabpage() else {
        return error_flow(runtime, "E749", "No current tabpage");
    };
    let Some(current) = editor.current_window() else {
        return error_flow(runtime, "E749", "No current window");
    };
    for window in editor.windows() {
        if window != current
            && let Err(error) = editor.close_window(tab, window, true)
        {
            return error_flow(runtime, "E445", error.to_string());
        }
    }
    Flow::Normal
}

/// `:qall` (`ex_docmd.c` `ex_quitall)`: quit all windows and the host
/// process when no buffer has unwritten changes; the bang form always
/// quits.  `check_changed_any` blocks on any modified buffer, hidden or
/// displayed, matching upstream's process-wide guard.
fn command_qall<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    if !command.bang
        && editor.buffers().into_iter().any(|buffer| {
            editor
                .buffer(buffer)
                .is_ok_and(|state| state.flags.contains(crate::BufferFlags::MODIFIED))
        })
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    Flow::Quit(0)
}

/// `:cquit [code]` (`ex_docmd.c` `ex_cquit)`: terminate the host with
/// `code`, defaulting to `EXIT_FAILURE` when no count is given.
fn command_cquit<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &Editor,
    command: &ExCommand,
) -> Flow {
    let code = if let Some(count) = command.count {
        i64::try_from(count).unwrap_or(i64::MAX)
    } else if command.range.is_some() {
        let (_, end) = match resolve_range_raw(editor, command) {
            Ok(range) => range,
            Err(message) => return error_flow(runtime, "E16", message),
        };
        i64::try_from(end).unwrap_or(i64::MAX)
    } else {
        1
    };
    Flow::Quit(code)
}

fn command_buffer_step<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    step: isize,
) -> Flow {
    let buffers = editor.buffers();
    if buffers.is_empty() {
        return error_flow(runtime, "E85", "There is no listed buffer");
    }
    if let Some(current) = editor.current_buffer()
        && editor
            .buffer(current)
            .is_ok_and(|state| state.flags.contains(crate::BufferFlags::MODIFIED))
        && !command.bang
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    let current = editor.current_buffer();
    let current_index = current
        .and_then(|current| buffers.iter().position(|buffer| *buffer == current))
        .unwrap_or(0);
    let next = (current_index.cast_signed() + step)
        .rem_euclid(buffers.len().cast_signed())
        .cast_unsigned();
    // 'winfixbuf' pins the window (`do_buffer`, buffer.c:1397).
    if current != Some(buffers[next])
        && let Some(flow) = winfixbuf_blocks(runtime, editor, command.bang)
    {
        return flow;
    }
    match editor.set_current_buffer(buffers[next], BufferRelease::KeepLoaded) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E86", error.to_string()),
    }
}

/// `:bf[irst]`/`:br[ewind]` and `:bl[ast]` (`ex_buffer_all`, buffer.c):
/// jump to the first or last listed buffer; the 'winfixbuf' guard matches
/// `do_buffer`'s (buffer.c:1397).
fn command_buffer_absolute<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    target: isize,
) -> Flow {
    let buffers = editor.buffers();
    if buffers.is_empty() {
        return error_flow(runtime, "E85", "There is no listed buffer");
    }
    if let Some(current) = editor.current_buffer()
        && editor
            .buffer(current)
            .is_ok_and(|state| state.flags.contains(crate::BufferFlags::MODIFIED))
        && !command.bang
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    let index = if target == isize::MAX {
        buffers.len() - 1
    } else {
        0
    };
    if editor.current_buffer() != Some(buffers[index])
        && let Some(flow) = winfixbuf_blocks(runtime, editor, command.bang)
    {
        return flow;
    }
    match editor.set_current_buffer(buffers[index], BufferRelease::KeepLoaded) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E86", error.to_string()),
    }
}

/// `:fir[st]`/`:rew[ind]` and `:la[st]`: display the first or last argument
/// (`ex_rewind`, arglist.c); the winfixbuf guard lives in the shared
/// `edit_argument_file` sink.
fn command_argument_absolute<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    target: i64,
) -> Flow {
    let count = i64::try_from(editor.arglist().len()).unwrap_or(i64::MAX);
    let entry = if target == i64::MAX {
        count.saturating_sub(1)
    } else {
        0
    };
    do_argfile(runtime, editor, command.bang, entry)
}

/// `:argu[ment] [count]`: display the count-th argument, defaulting to the
/// current one (`ex_argument`, arglist.c).
fn command_argument<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let count = command
        .count
        .and_then(|count| i64::try_from(count).ok())
        .or_else(|| command.args.trim().parse::<i64>().ok())
        .unwrap_or_else(|| i64::try_from(editor.arglist().index()).unwrap_or(0));
    do_argfile(runtime, editor, command.bang, count.saturating_sub(1))
}

fn command_buffer<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let arg = command.args.trim();
    let requested = command
        .count
        .and_then(|value| i64::try_from(value).ok())
        .or_else(|| arg.parse::<i64>().ok());
    let handle = if let Some(handle) = requested.and_then(|value| BufHandle::try_from(value).ok()) {
        handle
    } else {
        let matches: Vec<BufHandle> = editor
            .buffers()
            .into_iter()
            .filter(|handle| {
                editor
                    .buffer(*handle)
                    .is_ok_and(|buffer| buffer_name_matches(buffer.name(), arg))
            })
            .collect();
        match matches.as_slice() {
            [handle] => *handle,
            [] => return error_flow(runtime, "E94", format!("No matching buffer for {arg}")),
            _ => return error_flow(runtime, "E93", format!("More than one match for {arg}")),
        }
    };
    if let Some(current) = editor.current_buffer()
        && editor
            .buffer(current)
            .is_ok_and(|state| state.flags.contains(crate::BufferFlags::MODIFIED))
        && !command.bang
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    // 'winfixbuf' pins the window: switching to another buffer needs the
    // bang (`do_buffer`, buffer.c:1397); staying is always allowed.
    if editor.current_buffer() != Some(handle)
        && let Some(flow) = winfixbuf_blocks(runtime, editor, command.bang)
    {
        return flow;
    }
    match editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E86", error.to_string()),
    }
}

#[derive(Clone, Copy)]
struct BufferListContext {
    current: Option<BufHandle>,
    alternate: Option<BufHandle>,
    current_line: usize,
}

fn command_buffer_list<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let args = command.args.trim();
    if !args.is_empty() {
        return error_flow(runtime, "E488", format!("Trailing characters: {args}"));
    }
    let context = editor
        .current_window()
        .and_then(|handle| editor.window(handle).ok())
        .map_or(
            BufferListContext {
                current: None,
                alternate: None,
                current_line: 0,
            },
            |window| BufferListContext {
                current: Some(window.buffer),
                alternate: window.alternate_buffer,
                current_line: window.cursor.lnum,
            },
        );
    let mut rows = Vec::new();
    for buffer in editor.buffers() {
        let state = match editor.buffer(buffer) {
            Ok(state) => state,
            Err(error) => return error_flow(runtime, "E749", error.to_string()),
        };
        if !command.bang && !state.flags.contains(crate::BufferFlags::LISTED) {
            continue;
        }
        rows.push(format_buffer_list_row(editor, buffer, state, context));
    }
    for row in rows {
        push_info_text_message(editor, row);
    }
    Flow::Normal
}

fn format_buffer_list_row(
    editor: &Editor,
    buffer: BufHandle,
    state: &crate::BufferState,
    context: BufferListContext,
) -> String {
    let listed = if state.flags.contains(crate::BufferFlags::LISTED) {
        ' '
    } else {
        'u'
    };
    let selected = if context.current == Some(buffer) {
        '%'
    } else if context.alternate == Some(buffer) {
        '#'
    } else {
        ' '
    };
    let activity = if state.residency.is_hidden() {
        'h'
    } else if state.residency.is_loaded() {
        'a'
    } else {
        ' '
    };
    let policy = if !buffer_bool_option(editor, buffer, "modifiable") {
        '-'
    } else if buffer_bool_option(editor, buffer, "readonly") {
        '='
    } else {
        ' '
    };
    let changed = if state.flags.contains(crate::BufferFlags::MODIFIED) {
        '+'
    } else {
        ' '
    };
    let name = state.name().to_string_lossy();
    let display_name = if name.is_empty() {
        "[No Name]"
    } else {
        name.as_ref()
    };
    let mut row = format!(
        "{:>3}{listed}{selected}{activity}{policy}{changed} \"{display_name}\"",
        i64::from(buffer),
    );
    let width = row.chars().fold(0, |column, character| {
        column + cell_width(character, column, 8)
    });
    row.extend(std::iter::repeat_n(
        ' ',
        40usize.saturating_sub(width).max(1),
    ));
    let line = if context.current == Some(buffer) {
        context.current_line
    } else {
        0
    };
    let _ = write!(row, "line {line}");
    row
}

fn buffer_name_matches(name: &OxStr, needle: &str) -> bool {
    let name = name.to_string_lossy();
    name == needle
        || Path::new(name.as_ref())
            .file_name()
            .is_some_and(|file_name| file_name == needle)
}

/// `:args` (`ex_args`, arglist.c 502): with file arguments the list is
/// redefined and the first entry edited, exactly like `:next`; without
/// arguments the list is printed with the current entry in brackets.
fn command_args<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    if !command.args.trim().is_empty() {
        return command_next(runtime, editor, command);
    }
    let arglist = editor.arglist();
    if arglist.is_empty() {
        return Flow::Normal;
    }
    let current = arglist.index();
    let mut line = String::new();
    for (position, name) in arglist.names().iter().enumerate() {
        if position > 0 {
            line.push_str("  ");
        }
        if position == current {
            line.push('[');
            line.push_str(&name.to_string_lossy());
            line.push(']');
        } else {
            line.push_str(&name.to_string_lossy());
        }
    }
    push_text_message(editor, line, false, false);
    Flow::Normal
}

/// `:next` (`ex_next`, arglist.c 670): with file arguments the argument
/// list is redefined and its first entry edited; otherwise the count-th
/// following entry is edited through `do_argfile`.
fn command_next<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let list = command.args.trim();
    if list.is_empty() {
        let step = command_step(command);
        let target = i64::try_from(editor.arglist().index()).unwrap_or(i64::MAX) + step;
        return do_argfile(runtime, editor, command.bang, target);
    }
    // The changed-buffer guard runs before the list is replaced (ex_next
    // checks first so a failure leaves the old list intact).
    if let Some(current) = editor.current_buffer()
        && editor
            .buffer(current)
            .is_ok_and(|state| state.flags.contains(crate::BufferFlags::MODIFIED))
        && !command.bang
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    let mut names = Vec::new();
    for name in crate::arglist::split_file_list(list) {
        // expand_wildcards with EW_NOTFOUND (arglist.c 432): wildcard
        // patterns expand to their sorted matches, and a pattern without
        // matches stays as the literal name.
        let matches = crate::fs_builtins::expand_glob(runtime.scripts.io(), &name, false);
        if matches.is_empty() {
            names.push(name);
        } else {
            names.extend(matches);
        }
    }
    if names.is_empty() {
        return error_flow(runtime, "E479", "No match");
    }
    editor.arglist_mut().set(
        names
            .into_iter()
            .map(|name| OxStr::from(name.as_str()))
            .collect(),
    );
    do_argfile(runtime, editor, command.bang, 0)
}

fn command_step(command: &ExCommand) -> i64 {
    // EX_COUNT commands take their count either trailing or as the single
    // leading number (do_one_cmd converts one numeric address to a count).
    if let Some(count) = command.count.and_then(|value| i64::try_from(value).ok()) {
        return count;
    }
    if let Some(range) = &command.range
        && matches!(range.kind, RangeKind::Single)
        && let Some(address) = &range.start
        && let AddressBase::Line(line) = address.base
        && address.offsets.is_empty()
        && let Ok(value) = i64::try_from(line)
    {
        return value;
    }
    1
}
fn command_previous<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let step = command_step(command);
    let arglist = editor.arglist();
    let index = i64::try_from(arglist.index()).unwrap_or(i64::MAX);
    let count = i64::try_from(arglist.len()).unwrap_or(i64::MAX);
    let target = if index - step >= count {
        count - 1
    } else {
        index - step
    };
    do_argfile(runtime, editor, command.bang, target)
}

/// Edits entry `target` of the argument list (`do_argfile`, arglist.c
/// 600): out-of-range targets fail with E163/E164/E165, and the index
/// only advances when the edit succeeded.
fn do_argfile<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    force: bool,
    target: i64,
) -> Flow {
    let entry = match editor.arglist().check_target(target) {
        Ok(entry) => entry,
        Err(error) => return error_flow(runtime, error.code, error.message),
    };
    let name = editor
        .arglist()
        .name(entry)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let flow = edit_argument_file(runtime, editor, force, &name);
    if matches!(flow, Flow::Normal) {
        editor.arglist_mut().set_index(entry);
    }
    flow
}

/// Displays the argument's file: reuse the buffer already carrying the
/// name (`alist_name` prefers the associated buffer), else load the file
/// like `:edit` does, treating a missing file as an empty new buffer.
fn edit_argument_file<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    force: bool,
    name: &str,
) -> Flow {
    if !force
        && let Some(current) = editor.current_buffer()
        && editor
            .buffer(current)
            .is_ok_and(|state| state.flags.contains(crate::BufferFlags::MODIFIED))
    {
        return error_flow(
            runtime,
            "E37",
            "No write since last change (add ! to override)",
        );
    }
    for handle in editor.buffers() {
        if editor
            .buffer(handle)
            .is_ok_and(|state| state.name().as_bytes() == name.as_bytes())
        {
            // 'winfixbuf' rejects editing a different argument in place
            // (do_argfile, arglist.c:620); the bang overrides.
            if editor.current_buffer() != Some(handle)
                && let Some(flow) = winfixbuf_blocks(runtime, editor, force)
            {
                return flow;
            }
            return match editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
                Ok(()) => Flow::Normal,
                Err(error) => error_flow(runtime, "E86", error.to_string()),
            };
        }
    }
    // An argument with no buffer yet opens a new file, which is always a
    // different buffer: same guard.
    if let Some(flow) = winfixbuf_blocks(runtime, editor, force) {
        return flow;
    }
    let text = match runtime.scripts.io().read_to_string(Path::new(name)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return error_flow(runtime, "E484", format!("Can't open file {name}: {error}"));
        }
    };
    let buffer_text = match Buffer::from_bytes(text.as_bytes()) {
        Ok(buffer) => buffer,
        Err(error) => return error_flow(runtime, "E474", error.to_string()),
    };
    let handle = match editor.create_buffer_with(buffer_text, true) {
        Ok(handle) => handle,
        Err(error) => return error_flow(runtime, "E948", error.to_string()),
    };
    if let Ok(state) = editor.buffer_mut(handle) {
        state.set_name(OxStr::from(name));
        state.mark_saved();
    }
    if editor.current_window().is_none() {
        return match editor.create_tabpage(
            handle,
            crate::Geometry {
                row: 0,
                col: 0,
                width: 80,
                height: 24,
            },
        ) {
            Ok(_) => Flow::Normal,
            Err(error) => error_flow(runtime, "E948", error.to_string()),
        };
    }
    match editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E86", error.to_string()),
    }
}

/// `:argdo` (`ex_listdo` `CMD_argdo`, `ex_cmds2.c` 461): for every entry in
/// the range switch to its buffer and execute the command tail; a failing
/// switch or command aborts the loop. The entry already displayed is not
/// re-edited (upstream avoids reloading it).
fn command_argdo<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let nested = command.args.trim();
    if nested.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let count = access.with_ex_editor(|editor| editor.arglist().len());
    if count == 0 {
        return Flow::Normal;
    }
    let (start, end) = match access.with_ex_editor(|editor| resolve_arg_range(editor, command)) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    if start > count {
        return Flow::Normal;
    }
    let logical = vec![LogicalLine {
        text: nested.to_owned(),
        first_line: runtime.scripts.current_line(),
    }];
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    for entry in start..=end.min(count) {
        let index = entry - 1;
        if access.with_ex_editor(|editor| editor.arglist().index()) != index
            || !access.with_ex_editor(|editor| editing_argument(editor, index))
        {
            let flow = access.with_ex_editor(|editor| {
                do_argfile(
                    runtime,
                    editor,
                    command.bang,
                    i64::try_from(index).unwrap_or(i64::MAX),
                )
            });
            if !matches!(flow, Flow::Normal) {
                return flow;
            }
            if access.with_ex_editor(|editor| editor.arglist().index()) != index {
                break;
            }
        }
        let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
    }
    Flow::Normal
}

/// Whether the current buffer already displays argument `index`
/// (`editing_arg_idx`, arglist.c 463).
fn editing_argument(editor: &Editor, index: usize) -> bool {
    let Some(name) = editor.arglist().name(index) else {
        return false;
    };
    editor.current_buffer().is_some_and(|buffer| {
        editor
            .buffer(buffer)
            .is_ok_and(|state| state.name().as_bytes() == name.as_bytes())
    })
}

/// Runs one Ex command for each initially selected window in the current tab.
///
/// The stable-handle snapshot makes deletion by an earlier iteration safe.
/// Focus stays on the last live window visited, matching `:windo`.
fn command_windo<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let nested = command.args.trim();
    if nested.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let Some(tab) = access.with_ex_editor(|editor| editor.current_tabpage()) else {
        return Flow::Normal;
    };
    let windows = match access.with_ex_editor(|editor| editor.tabpage_windows(tab)) {
        Ok(windows) => windows,
        Err(error) => return error_flow(runtime, "E957", error.to_string()),
    };
    let (start, end) = match access.with_ex_editor(|editor| resolve_range(editor, command)) {
        Ok(range) => range,
        Err(message) if message == "Invalid range" => {
            return error_flow(runtime, "E493", "Backwards range given");
        }
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let logical = vec![LogicalLine {
        text: nested.to_owned(),
        first_line: runtime.scripts.current_line(),
    }];
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    for &window in windows
        .iter()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start).saturating_add(1))
    {
        if access.with_ex_editor(|editor| editor.window(window).is_err()) {
            continue;
        }
        if let Err(error) = access.with_ex_editor(|editor| editor.set_current_window(window)) {
            return error_flow(runtime, "E957", error.to_string());
        }
        let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
    }
    Flow::Normal
}

/// Resolves a `:argdo` range against the argument list itself (entries,
/// not buffer lines); without a range the whole list is addressed.
fn resolve_arg_range(editor: &Editor, command: &ExCommand) -> Result<(usize, usize), String> {
    let count = editor.arglist().len();
    let current = editor.arglist().index() + 1;
    let Some(range) = &command.range else {
        return Ok((1, count));
    };
    if matches!(range.kind, RangeKind::WholeBuffer) {
        return Ok((1, count));
    }
    let start = range.start.as_ref().map_or(Ok(current), |address| {
        resolve_address(editor, address, current, count)
    })?;
    let end = range.end.as_ref().map_or(Ok(start), |address| {
        resolve_address(editor, address, current, count)
    })?;
    if start > end {
        return Err("Invalid range".to_owned());
    }
    Ok((start.max(1), end))
}

fn command_put<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let (buffer, position) = access.with_ex_editor(|editor| {
        let position = editor
            .current_window()
            .and_then(|window| editor.window(window).ok())
            .map_or(Position { lnum: 1, col: 0 }, |window| window.cursor);
        (editor.current_buffer(), position)
    });
    let Some(buffer) = buffer else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let register = command.register.unwrap_or('"');
    if register == '=' && !command.args.is_empty() {
        let value = match eval_text(runtime, access, scope, lua, &command.args) {
            Ok(value) => value,
            Err(flow) => return flow,
        };
        let lines = typval_to_text(&value)
            .split('\n')
            .map(|line| line.as_bytes().to_vec())
            .collect::<Vec<_>>();
        return match access.with_ex_editor(|editor| {
            editor.buffer_mut(buffer).and_then(|state| {
                state
                    .append_lines(position.lnum, &lines, position, 0)
                    .map_err(Into::into)
            })
        }) {
            Ok(_) => Flow::Normal,
            Err(error) => error_flow(runtime, "E354", error.to_string()),
        };
    }
    access.with_ex_editor(|editor| {
        let content = match editor.registers().get(register) {
            Ok(Some(content)) => content.clone(),
            Ok(None) => {
                return error_flow(runtime, "E353", format!("Nothing in register {register}"));
            }
            Err(error) => return error_flow(runtime, "E353", error.to_string()),
        };
        match editor.put_content(buffer, position, &content, 0) {
            Ok(()) => Flow::Normal,
            Err(error) => error_flow(runtime, "E353", error.to_string()),
        }
    })
}

fn command_delete<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (start, end) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let selected = lines[start.saturating_sub(1)..end.min(lines.len())].to_vec();
    let content = match RegisterContent::linewise(selected) {
        Ok(content) => content,
        Err(error) => return error_flow(runtime, "E354", error.to_string()),
    };
    if let Some(register) = command.register {
        if let Err(error) = editor.registers_mut().delete_to(register, content.clone()) {
            return error_flow(runtime, "E354", error.to_string());
        }
    } else {
        editor.registers_mut().delete(content);
    }
    let replacement = if start == 1 && end >= lines.len() {
        vec![Vec::new()]
    } else {
        Vec::new()
    };
    let cursor = Position {
        lnum: start
            .min(lines.len().saturating_sub(end - start + 1))
            .max(1),
        col: 0,
    };
    match editor.replace_buffer_lines(crate::LineReplaceRequest {
        buffer,
        start,
        end,
        lines: &replacement,
        cursor_before: cursor,
        cursor_after: cursor,
        timestamp: 0,
    }) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, "E16", error.to_string()),
    }
}

fn command_yank<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (start, end) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let content = match RegisterContent::linewise(
        lines[start.saturating_sub(1)..end.min(lines.len())].to_vec(),
    ) {
        Ok(content) => content,
        Err(error) => return error_flow(runtime, "E354", error.to_string()),
    };
    let result = if let Some(register) = command.register {
        editor.registers_mut().yank_to(register, content)
    } else {
        editor.registers_mut().yank(content);
        Ok(())
    };
    match result {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E354", error.to_string()),
    }
}

/// `:print` / `:p` — `ex_docmd.c` `ex_print`: every addressed line goes to
/// the message sink as an Echo message. Numbering follows `print_line` →
/// `print_line_no_prefix` (`ex_cmds.c`): the 'number' option prefixes each
/// line with its right-aligned line number padded to the width of the last
/// line number (`number_width`). An empty buffer raises E749 first, and the
/// cursor lands on the last printed line.
fn command_print<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    if lines.len() == 1 && lines[0].is_empty() {
        return error_flow(runtime, "E749", "Empty buffer");
    }
    let (start, end) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let number = matches!(
        option_value(editor, "number", SetLayer::Effective),
        Some(OptionValue::Boolean(true))
    );
    let width = lines.len().to_string().len();
    let last = end.min(lines.len());
    for lnum in start..=last {
        let text = String::from_utf8_lossy(&lines[lnum - 1]).into_owned();
        let message = if number {
            format!("{lnum:>width$} {text}")
        } else {
            text
        };
        push_info_text_message(editor, message);
    }
    if let Some(window) = editor.current_window()
        && let Err(error) = editor.set_window_cursor(window, Position { lnum: last, col: 0 })
    {
        return error_flow(runtime, "E16", error.to_string());
    }
    Flow::Normal
}

/// `ex_range_without_command` (`ex_docmd.c:2421-2446`): a bare address
/// moves the cursor to the clamped last line. Line 0 goes to line 1.
/// Then `beginline(BL_SOL | BL_FIX)` (`insert.c:2430`). `'startofline'`
/// defaults off, so a search address keeps the match column `do_search`
/// stored in `curswant`; with `'startofline'` the column is the first
/// non-blank.
fn command_range_only<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let (_start, end) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) if message.starts_with("E486:") => {
            return error_flow(runtime, "E486", message.trim_start_matches("E486: ").trim());
        }
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let last = if end == 0 {
        1
    } else {
        end.min(lines.len().max(1))
    };
    let Some(window) = editor.current_window() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let startofline = matches!(
        editor.options().get_global("startofline"),
        Ok(OptionValue::Boolean(true))
    );
    let current_col = editor.window(window).map_or(0, |state| state.cursor.col);
    let col = if startofline {
        lines
            .get(last.saturating_sub(1))
            .and_then(|line| line.iter().position(|b| !b.is_ascii_whitespace()))
            .unwrap_or(0)
    } else if let Some(matched) = range_search_match(editor, command) {
        matched.col
    } else {
        current_col
    };
    if let Err(error) = editor.set_window_cursor(window, Position { lnum: last, col }) {
        return error_flow(runtime, "E16", error.to_string());
    }
    Flow::Normal
}

/// Column `do_search` would leave for a bare `/pat/` or `?pat?` address.
fn range_search_match(editor: &Editor, command: &ExCommand) -> Option<Position> {
    let range = command.range.as_ref()?;
    let address = range.end.as_ref().or(range.start.as_ref())?;
    if !address.offsets.is_empty() {
        return None;
    }
    let current = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .map_or(1, |window| window.cursor.lnum);
    match &address.base {
        AddressBase::ForwardSearch(pattern) => search_address(editor, pattern, true, current).ok(),
        AddressBase::BackwardSearch(pattern) => {
            search_address(editor, pattern, false, current).ok()
        }
        _ => None,
    }
}

fn command_mark<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let Some(name) = command.args.trim().chars().next() else {
        return error_flow(
            runtime,
            "E191",
            "Argument must be a letter or forward/backward quote",
        );
    };
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E20", "Mark not set");
    };
    let position = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .map_or(Position { lnum: 1, col: 0 }, |window| Position {
            lnum: window.cursor.lnum,
            col: 0,
        });
    // `A-Z` and `0-9` are global file marks (`mark.c` setpcmark), everything
    // else `:mark` accepts is a buffer-local mark.
    let result = if name.is_ascii_uppercase() || name.is_ascii_digit() {
        editor
            .global_marks_mut()
            .set(
                name,
                crate::marks::MarkLocation::in_buffer(buffer, position),
            )
            .map(|_| ())
            .map_err(EditorError::Mark)
    } else {
        editor.set_local_mark(buffer, name, position).map(|_| ())
    };
    match result {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E191", error.to_string()),
    }
}

fn command_marks<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E20", "Mark not set");
    };
    let marks = match editor.buffer(buffer) {
        Ok(state) => state.marks.iter().collect::<Vec<_>>(),
        Err(error) => return error_flow(runtime, "E20", error.to_string()),
    };
    push_text_message(editor, "mark line  col file/text".to_owned(), false, false);
    for (name, position) in marks {
        push_text_message(
            editor,
            format!(" {name} {:>5} {:>4}", position.lnum, position.col),
            false,
            false,
        );
    }
    Flow::Normal
}

fn command_jumps(editor: &mut Editor) -> Flow {
    push_text_message(editor, " jump line  col file/text".to_owned(), false, false);
    let entries = editor.jumplist().entries().to_vec();
    for (index, location) in entries.iter().enumerate() {
        let text = jump_location_text(editor, location);
        push_text_message(
            editor,
            format!(
                "{:>5} {:>5} {:>4} {text}",
                index + 1,
                location.position.lnum,
                location.position.col
            ),
            false,
            false,
        );
    }
    if let Some(window) = editor.current_window()
        && let Ok(state) = editor.window(window)
    {
        push_text_message(
            editor,
            format!(">     {:>5} {:>4}", state.cursor.lnum, state.cursor.col),
            false,
            false,
        );
    }
    Flow::Normal
}

fn jump_location_text(editor: &Editor, location: &crate::marks::MarkLocation) -> String {
    match &location.target {
        crate::marks::MarkTarget::Buffer(buffer) => editor
            .buffer(*buffer)
            .ok()
            .and_then(|state| state.text().ok())
            .and_then(|text| text.line(location.position.lnum).ok())
            .map_or_else(String::new, |line| {
                String::from_utf8_lossy(&line).into_owned()
            }),
        crate::marks::MarkTarget::File(path) => path.to_string_lossy().into_owned(),
    }
}

fn command_delmarks<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let args = command.args.as_str();
    if command.bang {
        if !args.trim().is_empty() {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        let Some(buffer) = editor.current_buffer() else {
            return error_flow(runtime, "E20", "Mark not set");
        };
        return match editor.clear_local_marks(buffer) {
            Ok(()) => Flow::Normal,
            Err(error) => error_flow(runtime, "E20", error.to_string()),
        };
    }
    if args.trim().is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E20", "Mark not set");
    };
    let bytes = args.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let mark = bytes[index];
        if mark == b' ' {
            index += 1;
            continue;
        }
        if mark == b'\\' && bytes.get(index + 1) == Some(&b'"') {
            if let Err(error) = remove_delmark(editor, buffer, '"') {
                return error_flow(runtime, "E20", error.to_string());
            }
            index += 2;
            continue;
        }
        if mark.is_ascii_lowercase() || mark.is_ascii_uppercase() || mark.is_ascii_digit() {
            let mut end = mark;
            if bytes.get(index + 1) == Some(&b'-') {
                let Some(candidate) = bytes.get(index + 2).copied() else {
                    return invalid_delmarks_argument(runtime, &args[index..]);
                };
                let same_class = (mark.is_ascii_lowercase() && candidate.is_ascii_lowercase())
                    || (mark.is_ascii_uppercase() && candidate.is_ascii_uppercase())
                    || (mark.is_ascii_digit() && candidate.is_ascii_digit());
                if !same_class || candidate < mark {
                    return invalid_delmarks_argument(runtime, &args[index..]);
                }
                end = candidate;
                index += 2;
            }
            for name in mark..=end {
                if let Err(error) = remove_delmark(editor, buffer, char::from(name)) {
                    return error_flow(runtime, "E20", error.to_string());
                }
            }
            index += 1;
            continue;
        }
        if matches!(mark, b'"' | b'^' | b':' | b'.' | b'[' | b']' | b'<' | b'>') {
            if matches!(mark, b'"' | b'^' | b'.' | b'[' | b']')
                && let Err(error) = remove_delmark(editor, buffer, char::from(mark))
            {
                return error_flow(runtime, "E20", error.to_string());
            }
            index += 1;
            continue;
        }
        return invalid_delmarks_argument(runtime, &args[index..]);
    }
    Flow::Normal
}

fn remove_delmark(editor: &mut Editor, buffer: BufHandle, name: char) -> Result<(), EditorError> {
    if name.is_ascii_lowercase() || matches!(name, '"' | '^' | '.' | '[' | ']') {
        editor.remove_local_mark(buffer, name)?;
    } else {
        editor.global_marks_mut().remove(name)?;
    }
    Ok(())
}

fn invalid_delmarks_argument<F: FileIO>(runtime: &ExRuntime<F>, argument: &str) -> Flow {
    error_flow(runtime, "E475", format!("Invalid argument: {argument}"))
}

fn command_registers<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    args: &str,
) -> Flow {
    let requested = args.trim();
    let names = if requested.is_empty() {
        "0123456789abcdefghijklmnopqrstuvwxyz\"-:.%#=*+_/@"
    } else {
        requested
    };
    let mut messages = Vec::new();
    for name in names.chars() {
        match editor.registers().get(name) {
            Ok(Some(content)) => messages.push(format!(
                "\"{name}   {}",
                String::from_utf8_lossy(&content.to_bytes()).replace('\n', "^J")
            )),
            Ok(None) => {}
            Err(error) => return error_flow(runtime, "E354", error.to_string()),
        }
    }
    for message in messages {
        push_text_message(editor, message, false, false);
    }
    Flow::Normal
}

/// Sources one file found under 'runtimepath', routing `.lua` through the
/// installed Lua host and everything else through the Vimscript sourcer.
/// Upstream `source_callback` (`runtime.c:975-990`) makes the same split via
/// `do_source`.
fn source_runtime_file<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    path: &Path,
) -> Flow {
    if path.extension().is_some_and(|extension| extension == "lua") {
        let Some(host) = lua else {
            return Flow::NotImplemented("luafile".to_owned());
        };
        if let Err(error) = access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
            return exec_error_flow(runtime, error);
        }
        let result = host.borrow_mut().execute_file(path);
        let sync = access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
        return match (result, sync) {
            (Err(error), _) => lua_error_flow(runtime, error, "E5112", "E5113"),
            (Ok(()), Err(error)) => exec_error_flow(runtime, error),
            (Ok(()), Ok(())) => Flow::Normal,
        };
    }
    match source_path(runtime, access, scope, lua, path, false) {
        Ok(Flow::Finish) => Flow::Normal,
        Ok(flow) => flow,
        Err(error) => exec_error_flow(runtime, error),
    }
}

/// `source_runtime(names, DIP_ALL)` (`runtime.c` `do_in_path`:430-515):
/// walk 'runtimepath' in order and, in each entry, source every one of the
/// whitespace-separated `names` that exists there. Wildcards in `names` are
/// not expanded; the `:filetype` file lists are all literal names.
fn source_runtime_all<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    names: &str,
) -> Flow {
    let roots: Vec<PathBuf> = runtime
        .scripts
        .runtime_roots()
        .iter()
        .map(|root| root.path().to_path_buf())
        .collect();
    for root in roots {
        for name in names.split_ascii_whitespace() {
            let path = root.join(name);
            if !runtime.scripts.io().exists(&path) {
                continue;
            }
            let flow = source_runtime_file(runtime, access, scope, lua, &path);
            if !matches!(flow, Flow::Normal) {
                return flow;
            }
        }
    }
    Flow::Normal
}

/// `:filetype` (`ex_docmd.c` `ex_filetype`:7886-7949).
///
/// Without an argument the three enablement states are reported. Otherwise
/// the leading words `plugin` and `indent` are accepted in either order and
/// any number of times, and the remainder must be exactly `on`, `detect`, or
/// `off`; anything else is E475. `on`/`detect` source `filetype.lua`,
/// `ftplugin.vim`, and `indent.vim` from 'runtimepath'; `off` sources
/// `ftoff.vim`, or `ftplugof.vim`/`indoff.vim` when `plugin`/`indent` was
/// named. `detect` additionally re-fires the `filetypedetect` group's
/// `BufRead` autocommands. Upstream also runs `do_modelines` there; modeline
/// scanning (`option.c` `do_modelines`) has no counterpart in this port, so
/// `:filetype detect` re-runs detection autocommands only.
fn command_filetype<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let mut arg = command.args.trim();
    if arg.is_empty() {
        let state = runtime.filetype;
        let detect = if state.detect == Some(true) {
            "ON"
        } else {
            "OFF"
        };
        let dependent = |value: Option<bool>| match (value, state.detect) {
            (Some(true), Some(true)) => "ON",
            (Some(true), _) => "(on)",
            _ => "OFF",
        };
        access.with_ex_editor(|editor| {
            push_text_message(
                editor,
                format!(
                    "filetype detection:{detect}  plugin:{}  indent:{}",
                    dependent(state.plugin),
                    dependent(state.indent)
                ),
                false,
                false,
            );
        });
        return Flow::Normal;
    }

    let mut plugin = false;
    let mut indent = false;
    loop {
        if let Some(rest) = arg.strip_prefix("plugin") {
            plugin = true;
            arg = rest.trim_start();
            continue;
        }
        if let Some(rest) = arg.strip_prefix("indent") {
            indent = true;
            arg = rest.trim_start();
            continue;
        }
        break;
    }

    match arg {
        "on" | "detect" => {
            if arg == "on" || runtime.filetype.detect != Some(true) {
                let flow = source_runtime_all(runtime, access, scope, lua, FILETYPE_FILE);
                if !matches!(flow, Flow::Normal) {
                    return flow;
                }
                runtime.filetype.detect = Some(true);
                if plugin {
                    let flow = source_runtime_all(runtime, access, scope, lua, FTPLUGIN_FILE);
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                    runtime.filetype.plugin = Some(true);
                }
                if indent {
                    let flow = source_runtime_all(runtime, access, scope, lua, INDENT_FILE);
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                    runtime.filetype.indent = Some(true);
                }
            }
            if arg == "detect" {
                return filetype_detect_autocmds(runtime, access, scope, lua);
            }
            Flow::Normal
        }
        "off" => {
            if !plugin && !indent {
                let flow = source_runtime_all(runtime, access, scope, lua, FTOFF_FILE);
                if !matches!(flow, Flow::Normal) {
                    return flow;
                }
                runtime.filetype.detect = Some(false);
                return Flow::Normal;
            }
            if plugin {
                let flow = source_runtime_all(runtime, access, scope, lua, FTPLUGOF_FILE);
                if !matches!(flow, Flow::Normal) {
                    return flow;
                }
                runtime.filetype.plugin = Some(false);
            }
            if indent {
                let flow = source_runtime_all(runtime, access, scope, lua, INDOFF_FILE);
                if !matches!(flow, Flow::Normal) {
                    return flow;
                }
                runtime.filetype.indent = Some(false);
            }
            Flow::Normal
        }
        other => error_flow(runtime, "E475", format!("Invalid argument: {other}")),
    }
}

/// `do_doautocmd("filetypedetect BufRead", true, NULL)` from `ex_filetype`:
/// re-fire the `filetypedetect` augroup's `BufRead` autocommands against the
/// current buffer. An absent group has nothing to fire.
fn filetype_detect_autocmds<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
) -> Flow {
    let Some(group) = access.with_ex_editor(|editor| editor.autocmds().group("filetypedetect"))
    else {
        return Flow::Normal;
    };
    let buffer = access.with_ex_editor(|editor| editor.current_buffer());
    let name = buffer
        .and_then(|buffer| {
            access.with_ex_editor(|editor| {
                editor
                    .buffer(buffer)
                    .ok()
                    .map(|state| state.name().to_string_lossy().into_owned())
            })
        })
        .unwrap_or_default();
    let plan = access.with_ex_editor(|editor| {
        editor.autocmds_mut().plan_in_group(
            Event::BufReadPost,
            group,
            AutocmdContext {
                buffer,
                file_name: Some(&name),
                ..AutocmdContext::default()
            },
        )
    });
    run_autocmd_plan(runtime, access, scope, lua, plan)
}

fn command_colorscheme<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let name = command.args.trim();
    if name.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }

    let mut scheme = None;
    for root in runtime.scripts.runtime_roots() {
        let base = root.path().join("colors").join(name);
        let vim = base.with_extension("vim");
        if runtime.scripts.io().exists(&vim) {
            scheme = Some(vim);
            break;
        }
        let lua = base.with_extension("lua");
        if runtime.scripts.io().exists(&lua) {
            scheme = Some(lua);
            break;
        }
    }

    let Some(path) = scheme else {
        return error_flow(
            runtime,
            "E185",
            format!("Cannot find color scheme '{name}'"),
        );
    };
    let flow = source_runtime_file(runtime, access, scope, lua, &path);
    if !matches!(flow, Flow::Normal) {
        return flow;
    }

    if let Err(error) = scope.set_scoped(
        ScopeKind::Global,
        b"colors_name",
        0,
        Typval::String(OxStr::from(name)),
    ) {
        return eval_error_flow(runtime, error);
    }
    let plan = access.with_ex_editor(|editor| {
        editor.autocmds_mut().plan(
            Event::ColorScheme,
            AutocmdContext {
                file_name: Some(name),
                ..AutocmdContext::default()
            },
        )
    });
    run_autocmd_plan(runtime, access, scope, lua, plan)
}

fn release_removed_autocmds<E: ExEditorAccess>(
    access: &E,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    removals: impl IntoIterator<Item = AutocmdKind>,
) -> Result<(), LuaExecError> {
    let mut references = Vec::new();
    for kind in removals {
        let AutocmdKind::LuaCallback(reference) = kind else {
            continue;
        };
        if !references.contains(&reference) {
            references.push(reference);
        }
    }
    let Some(lua) = lua else {
        return Ok(());
    };
    for reference in references {
        if access.with_ex_editor(|editor| editor.autocmds().uses_lua_callback(reference)) {
            continue;
        }
        let reference = usize::try_from(reference).map_err(|_| {
            LuaExecError::Conversion("Lua callback reference is out of range".to_owned())
        })?;
        lua.borrow_mut().free_callback(reference)?;
    }
    Ok(())
}

fn run_lua_autocmd_callback<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    action: &crate::AutocmdAction,
    reference: u64,
) -> (Flow, bool) {
    let Some(lua) = lua else {
        return (
            error_flow(runtime, "E5108", "Lua callbacks are not installed"),
            false,
        );
    };
    if let Err(error) = access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
        return (exec_error_flow(runtime, error), false);
    }
    let Ok(reference) = usize::try_from(reference) else {
        return (
            error_flow(runtime, "E5108", "Lua callback reference is out of range"),
            false,
        );
    };
    let args = match action.callback_args() {
        Ok(args) => args,
        Err(error) => return (error_flow(runtime, "E5108", error.to_string()), false),
    };
    let result = lua.borrow_mut().invoke_callback(reference, args);
    let sync = access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
    match (result, sync) {
        (Err(error), _) => (lua_error_flow(runtime, error, "E5107", "E5108"), false),
        (Ok(_), Err(error)) => (exec_error_flow(runtime, error), false),
        (Ok(value), Ok(())) => {
            let delete = !matches!(value, Object::Nil | Object::Boolean(false));
            (Flow::Normal, delete)
        }
    }
}

/// Executes one [`FiringPlan`] in order, acknowledging `++once` definitions
/// as each action starts and stopping at the first non-normal flow
/// (`autocmd.c` `apply_autocmds_group` runs the matched list in sequence).
fn run_autocmd_plan<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    plan: FiringPlan,
) -> Flow {
    // `autocmd_busy` (`autocmd.c:1657`): one plan counts as one busy span,
    // matching `apply_autocmds` which sets and restores it around the group.
    runtime.autocmd_busy += 1;
    let mut flow = Flow::Normal;
    for action in plan.ready {
        if !access.with_ex_editor(|editor| editor.autocmds().is_entry_live(action.entry_id)) {
            continue;
        }
        let mut removed = Vec::new();
        if action.once
            && let Some(kind) =
                access.with_ex_editor(|editor| editor.autocmds_mut().consume_once(action.entry_id))
        {
            removed.push(kind);
        }
        let previous_context = std::mem::replace(
            &mut runtime.active_autocmd,
            ActiveAutocmdContext {
                matched: action.match_name.clone(),
                file: action.file_name.clone(),
                buffer: action.buffer,
                nested: action.nested,
            },
        );
        let (action_flow, delete) = match &action.kind {
            AutocmdKind::ExString(source) => {
                let logical = vec![LogicalLine {
                    text: source.clone(),
                    first_line: runtime.scripts.current_line(),
                }];
                let program = parse_program(
                    &runtime.user_commands,
                    access.with_ex_editor(|editor| editor.current_buffer()),
                    &logical,
                );
                let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
                (flow, false)
            }
            AutocmdKind::VimscriptFunction(name) => {
                let (first, last) = access.with_ex_editor(|editor| current_line_pair(editor));
                let flow = match call_user_function(
                    runtime,
                    access,
                    scope,
                    lua,
                    name,
                    Vec::new(),
                    first,
                    last,
                ) {
                    Ok(_) => Flow::Normal,
                    Err(flow) => flow,
                };
                (flow, false)
            }
            AutocmdKind::LuaCallback(reference) => {
                run_lua_autocmd_callback(runtime, access, scope, lua, &action, *reference)
            }
        };
        runtime.active_autocmd = previous_context;
        if delete
            && let Some(kind) =
                access.with_ex_editor(|editor| editor.autocmds_mut().delete_entry(action.entry_id))
        {
            removed.push(kind);
        }
        if let Err(error) = release_removed_autocmds(access, lua, removed) {
            flow = lua_error_flow(runtime, error, "E5107", "E5108");
            break;
        }
        if !matches!(action_flow, Flow::Normal) {
            flow = action_flow;
            break;
        }
    }
    runtime.autocmd_busy -= 1;
    flow
}

/// Fires `events` for one buffer lifecycle occurrence — a fresh `:edit`/`:new`
/// buffer, or the entry into one — bound to `buffer` as `<abuf>` and to the
/// buffer's name as `<afile>`/`<amatch>`, through the shared planner and
/// [`run_autocmd_plan`]. The first non-normal handler flow wins, so a caller
/// can abort the entry it is performing.
fn fire_buffer_lifecycle<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    events: &[Event],
    buffer: BufHandle,
) -> Flow {
    // `apply_autocmds` (`autocmd.c:1465-1468`): while autocommands are busy,
    // an event raised without `force` fires only through a `++nested` handler.
    if runtime.autocmd_busy > 0 && !runtime.active_autocmd.nested {
        return Flow::Normal;
    }
    let name = access.with_ex_editor(|editor| {
        editor
            .buffer(buffer)
            .ok()
            .map(|state| state.name().to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    for &event in events {
        let plan = access.with_ex_editor(|editor| {
            editor.autocmds_mut().plan(
                event,
                AutocmdContext {
                    buffer: Some(buffer),
                    file_name: Some(&name),
                    ..AutocmdContext::default()
                },
            )
        });
        if plan.ready.is_empty() {
            continue;
        }
        let flow = run_autocmd_plan(runtime, access, scope, lua, plan);
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
    }
    Flow::Normal
}

/// `getout` (`main.c`:753-882), the exit sequence: `VimLeavePre` runs first,
/// then the `ShaDa` write this port does not have, then `VimLeave`, and only
/// then does the process go away. Both events fire once per process, which is
/// what `apply_autocmds` guarantees upstream by never returning to the Ex loop
/// afterwards; here the flag on the runtime says the same thing, since a
/// `:quit` inside a `VimLeave` handler must not restart the sequence.
///
/// An autocmd that fails does not cancel the exit: upstream reports it through
/// `emsg` and carries on to `os_exit`, so the text is recorded as a message and
/// the requested status survives.
fn fire_exit_autocmds<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
) {
    if runtime.exiting {
        return;
    }
    runtime.exiting = true;
    for event in [Event::VimLeavePre, Event::VimLeave] {
        let plan = access
            .with_ex_editor(|editor| editor.autocmds_mut().plan(event, AutocmdContext::default()));
        if plan.ready.is_empty() {
            continue;
        }
        let flow = run_autocmd_plan(runtime, access, scope, lua, plan);
        if let Flow::Exception(exception) = flow {
            access.with_ex_editor(|editor| {
                push_text_message(editor, exception.message(), true, true);
            });
        }
    }
}

/// `:language` (`os/lang.c` `ex_language`): inspect or set the process
/// locale. The optional leading keyword `messages`, `ctype`, `time`, or
/// `collate` selects the category and accepts any unambiguous prefix of at
/// least three characters, case-insensitively; without one, the argument is
/// a locale name applied to `LC_ALL`, and an empty argument reports the
/// current setting as a message. Setting a locale that the C library
/// rejects raises E197. A successful set resets `$LC_ALL` to the empty
/// string so it cannot override the category variables, propagates
/// `LANG`/`LANGUAGE`/`LC_MESSAGES` exactly where upstream does, pins
/// `LC_NUMERIC` to "C", and republishes `v:lang`, `v:ctype`, `v:lc_time`,
/// and `v:collate` from the final locale state (`set_lang_var`). Upstream
/// also seeds the 'helplang' option default and bumps the gettext catalog
/// counter here; neither is observable in this port (no 'helplang' option
/// model entry, and translations are out of scope).
fn command_language<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    command: &ExCommand,
) -> Flow {
    let arg = command.args.trim();
    let token_end = arg.find([' ', '\t']).unwrap_or(arg.len());
    let token = &arg[..token_end];
    let mut what = LocaleCategory::All;
    let mut whatstr = "";
    let mut name = arg;
    // At least three characters, so a two-letter language name such as "me"
    // cannot be mistaken for the keyword prefix (upstream comment).
    if token.len() >= 3 {
        if is_command_prefix(token, "messages") {
            what = LocaleCategory::Messages;
            whatstr = "messages ";
            name = arg[token_end..].trim_start_matches([' ', '\t']);
        } else if is_command_prefix(token, "ctype") {
            what = LocaleCategory::CType;
            whatstr = "ctype ";
            name = arg[token_end..].trim_start_matches([' ', '\t']);
        } else if is_command_prefix(token, "time") {
            what = LocaleCategory::Time;
            whatstr = "time ";
            name = arg[token_end..].trim_start_matches([' ', '\t']);
        } else if is_command_prefix(token, "collate") {
            what = LocaleCategory::Collate;
            whatstr = "collate ";
            name = arg[token_end..].trim_start_matches([' ', '\t']);
        }
    }

    if name.is_empty() {
        // Upstream reports "Unknown" when the query yields NULL or "".
        let current = ox_sys::current_locale(what)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Unknown".to_owned());
        push_text_message(
            editor,
            format!("Current {whatstr}language: \"{current}\""),
            false,
            false,
        );
        return Flow::Normal;
    }

    if ox_sys::set_locale(what, name).is_none() {
        return error_flow(
            runtime,
            "E197",
            format!("Cannot set language to \"{name}\""),
        );
    }
    // Keep number parsing on decimal points, as upstream re-pins LC_NUMERIC
    // after every successful setlocale.
    ox_sys::set_locale(LocaleCategory::Numeric, "C");

    ox_sys::set_env("LC_ALL", "");
    if !matches!(what, LocaleCategory::Time | LocaleCategory::Collate) {
        if what == LocaleCategory::All {
            ox_sys::set_env("LANG", name);
            ox_sys::set_env("LANGUAGE", "");
        }
        if what != LocaleCategory::CType {
            ox_sys::set_env("LC_MESSAGES", name);
        }
    }
    refresh_lang_vars(scope);
    Flow::Normal
}

/// Case-insensitive prefix match of the whole argument token against a
/// command keyword, mirroring upstream's `STRNICMP(arg, keyword, len)` with
/// the token's length: a token longer than the keyword never matches because
/// the comparison would run past the keyword's terminator.
fn is_command_prefix(token: &str, keyword: &str) -> bool {
    token.len() <= keyword.len() && keyword[..token.len()].eq_ignore_ascii_case(token)
}

/// Republishes `v:ctype`, `v:lang`, `v:lc_time`, and `v:collate` from the
/// process's current locale state, mirroring `set_lang_var`. A missing
/// query becomes the empty string, as `set_vim_var_string` does with NULL.
fn refresh_lang_vars(scope: &mut Scope) {
    for (name, category) in [
        ("ctype", LocaleCategory::CType),
        ("lang", LocaleCategory::Messages),
        ("lc_time", LocaleCategory::Time),
        ("collate", LocaleCategory::Collate),
    ] {
        let value = ox_sys::current_locale(category).unwrap_or_default();
        scope.replace_pair(
            ScopeKind::Vim,
            name,
            Typval::String(OxStr::from(value.as_str())),
        );
    }
}

fn command_highlight<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let args = command.args.trim();
    if args.is_empty() {
        let messages = editor
            .highlights()
            .iter()
            .map(|(name, attributes)| {
                format!(
                    "{name} xxx {}",
                    attributes
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            })
            .collect::<Vec<_>>();
        for message in messages {
            push_text_message(editor, message, false, false);
        }
        return Flow::Normal;
    }
    let mut words = args.split_ascii_whitespace();
    let Some(first) = words.next() else {
        return Flow::Normal;
    };
    if first.eq_ignore_ascii_case("clear") {
        if let Some(name) = words.next() {
            editor.highlights_mut().remove(name);
        } else {
            editor.highlights_mut().clear();
        }
        return Flow::Normal;
    }

    let default = first.eq_ignore_ascii_case("default") || first.eq_ignore_ascii_case("def");
    let Some(group_or_link) = (if default { words.next() } else { Some(first) }) else {
        return error_flow(runtime, "E471", "Argument required");
    };
    let link = group_or_link.eq_ignore_ascii_case("link");
    let Some(group) = (if link {
        words.next()
    } else {
        Some(group_or_link)
    }) else {
        return error_flow(runtime, "E412", "Not enough arguments: highlight link");
    };
    if default && editor.highlights().contains_key(group) {
        return Flow::Normal;
    }

    let mut attributes = BTreeMap::new();
    if link {
        let Some(target) = words.next() else {
            return error_flow(runtime, "E412", "Not enough arguments: highlight link");
        };
        if words.next().is_some() {
            return error_flow(runtime, "E488", "Trailing characters");
        }
        attributes.insert("link".to_owned(), target.to_owned());
    } else {
        for word in words {
            let Some((key, value)) = word.split_once('=') else {
                return error_flow(runtime, "E416", format!("Missing equal sign: {word}"));
            };
            attributes.insert(key.to_ascii_lowercase(), value.to_owned());
        }
    }
    editor.highlights_mut().insert(group.to_owned(), attributes);
    Flow::Normal
}

fn canonical_sign_highlight(name: &str) -> String {
    const NAMES: &[&str] = &[
        "Title",
        "LineNr",
        "Normal",
        "CursorLine",
        "Statement",
        "Search",
        "Visual",
        "Macro",
        "Type",
        "String",
    ];
    NAMES
        .iter()
        .copied()
        .find(|known| known.eq_ignore_ascii_case(name))
        .unwrap_or(name)
        .to_owned()
}

#[expect(
    clippy::too_many_lines,
    reason = ":sign keeps Vim's five subcommand flows in one ordered interpreter"
)]
fn command_sign<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let mut words = command.args.split_whitespace();
    let Some(action) = words.next() else {
        return error_flow(runtime, "E471", "Argument required");
    };

    match action {
        "define" => {
            let Some(name) = words.next() else {
                return error_flow(runtime, "E471", "Argument required");
            };
            let mut definition = crate::editor::SignDefinition::default();
            for word in words {
                let Some((key, value)) = word.split_once('=') else {
                    return error_flow(runtime, "E475", format!("Invalid argument: {word}"));
                };
                let value = if key == "text" {
                    value.to_owned()
                } else {
                    canonical_sign_highlight(value)
                };
                let slot = match key {
                    "text" => &mut definition.text,
                    "texthl" => &mut definition.text_highlight,
                    "numhl" => &mut definition.number_highlight,
                    "linehl" => &mut definition.line_highlight,
                    "culhl" => &mut definition.cursorline_highlight,
                    _ => return error_flow(runtime, "E475", format!("Invalid argument: {word}")),
                };
                *slot = Some(value);
            }
            editor
                .sign_definitions_mut()
                .insert(name.to_owned(), definition);
            Flow::Normal
        }
        "list" => {
            let requested = words.next();
            if words.next().is_some() {
                return error_flow(runtime, "E488", "Trailing characters");
            }
            let definitions: Vec<_> = editor
                .sign_definitions()
                .iter()
                .filter(|(name, _)| requested.is_none_or(|requested| requested == name.as_str()))
                .map(|(name, definition)| {
                    let mut line = format!("sign {name}");
                    for (key, value) in [
                        ("text", definition.text.as_ref()),
                        ("texthl", definition.text_highlight.as_ref()),
                        ("linehl", definition.line_highlight.as_ref()),
                        ("numhl", definition.number_highlight.as_ref()),
                        ("culhl", definition.cursorline_highlight.as_ref()),
                    ] {
                        if let Some(value) = value {
                            let _ = write!(line, " {key}={value}");
                        }
                    }
                    line
                })
                .collect();
            if requested.is_some() && definitions.is_empty() {
                return error_flow(runtime, "E155", "Unknown sign");
            }
            for message in definitions {
                push_text_message(editor, message, false, false);
            }
            Flow::Normal
        }
        "place" => {
            let Some(raw_id) = words.next() else {
                return error_flow(runtime, "E471", "Argument required");
            };
            let Ok(raw_id) = raw_id.parse::<u32>() else {
                return error_flow(runtime, "E474", "Invalid argument");
            };
            let Ok(id) = ExtmarkId::new(raw_id) else {
                return error_flow(runtime, "E474", "Invalid argument");
            };
            let mut buffer = editor.current_buffer();
            let mut line = None;
            let mut name = None;
            let mut priority = 10_u32;
            let mut group = None;
            for word in words {
                let Some((key, value)) = word.split_once('=') else {
                    return error_flow(runtime, "E474", "Invalid argument");
                };
                match key {
                    "buffer" => {
                        buffer = value
                            .parse::<i64>()
                            .ok()
                            .and_then(|value| BufHandle::try_from(value).ok());
                    }
                    "line" => line = value.parse::<usize>().ok(),
                    "name" => name = Some(value),
                    "priority" => match value.parse::<u32>() {
                        Ok(value) => priority = value,
                        Err(_) => return error_flow(runtime, "E474", "Invalid argument"),
                    },
                    "group" => group = Some(value),
                    _ => return error_flow(runtime, "E474", "Invalid argument"),
                }
            }
            let (Some(buffer), Some(line), Some(name)) = (buffer, line, name) else {
                return error_flow(runtime, "E474", "Invalid argument");
            };
            if line == 0 {
                return error_flow(runtime, "E474", "Invalid argument");
            }
            let Some(definition) = editor.sign_definitions().get(name).cloned() else {
                return error_flow(runtime, "E155", format!("Unknown sign: {name}"));
            };
            let group = match group {
                None => SignGroup::default_group(),
                Some(name) if name.is_empty() || name.starts_with('*') => {
                    return error_flow(runtime, "E474", "Invalid argument");
                }
                Some(name) => editor.sign_group(name),
            };
            let namespace = group.namespace();
            let mut placement = ExtmarkPlacement::new(ExtmarkPosition::new(line - 1, 0));
            placement.attributes = ExtmarkAttributes {
                sign_text: definition.text,
                sign_highlight_group: definition.text_highlight,
                number_highlight_group: definition.number_highlight,
                line_highlight_group: definition.line_highlight,
                cursorline_highlight_group: definition.cursorline_highlight,
                sign_name: Some(name.to_owned()),
                priority,
                ..ExtmarkAttributes::default()
            };
            placement
                .attributes
                .flags
                .set(crate::ExtmarkFlags::PRIORITY_SET, true);
            placement
                .attributes
                .flags
                .set(crate::ExtmarkFlags::INVALIDATE, true);
            placement
                .attributes
                .flags
                .set(crate::ExtmarkFlags::UNDO_RESTORE, false);
            let result = editor.buffer_mut(buffer).and_then(|state| {
                state
                    .extmarks
                    .ensure_namespace(namespace)
                    .map_err(crate::BufferStateError::from)?;
                Ok(state
                    .extmarks
                    .set(namespace, Some(id), placement)
                    .map_err(crate::BufferStateError::from)?)
            });
            match result {
                Ok(_) => Flow::Normal,
                Err(error) => error_flow(runtime, "E474", error.to_string()),
            }
        }
        "unplace" => {
            let Some(raw_id) = words.next() else {
                return error_flow(runtime, "E471", "Argument required");
            };
            let Ok(raw_id) = raw_id.parse::<u32>() else {
                return error_flow(runtime, "E474", "Invalid argument");
            };
            let Ok(id) = ExtmarkId::new(raw_id) else {
                return error_flow(runtime, "E474", "Invalid argument");
            };
            let mut buffer = editor.current_buffer();
            let mut group = None;
            for word in words {
                let Some((key, value)) = word.split_once('=') else {
                    return error_flow(runtime, "E474", "Invalid argument");
                };
                match key {
                    "buffer" => {
                        buffer = value
                            .parse::<i64>()
                            .ok()
                            .and_then(|value| BufHandle::try_from(value).ok());
                    }
                    "group" => group = Some(value),
                    _ => return error_flow(runtime, "E474", "Invalid argument"),
                }
            }
            let Some(buffer) = buffer else {
                return error_flow(runtime, "E474", "Invalid argument");
            };
            let namespaces = match group {
                Some("") => return error_flow(runtime, "E474", "Invalid argument"),
                Some(name) if name.starts_with('*') => {
                    let mut sweep = vec![SignGroup::default_group().namespace()];
                    sweep.extend(editor.sign_groups().map(SignGroup::namespace));
                    sweep
                }
                Some(name) => editor
                    .sign_group_if_placed(name)
                    .map(|group| vec![group.namespace()])
                    .unwrap_or_default(),
                None => vec![SignGroup::default_group().namespace()],
            };
            let named = group.is_some();
            let result = editor.buffer_mut(buffer).and_then(|state| {
                for namespace in namespaces {
                    if named {
                        state
                            .extmarks
                            .ensure_namespace(namespace)
                            .map_err(crate::BufferStateError::from)?;
                    }
                    state
                        .extmarks
                        .delete(namespace, id)
                        .map_err(crate::BufferStateError::from)?;
                }
                Ok(())
            });
            match result {
                Ok(()) => Flow::Normal,
                Err(error) => error_flow(runtime, "E474", error.to_string()),
            }
        }
        _ => error_flow(runtime, "E160", format!("Unknown sign command: {action}")),
    }
}

fn command_augroup<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let name = command.args.trim();
    if name.eq_ignore_ascii_case("END") {
        runtime.current_augroup = AugroupId::default();
        return Flow::Normal;
    }
    if command.bang {
        // `:augroup! name` (`do_augroup(arg, TRUE)` → `augroup_del` in
        // legacy mode): the name is removed while the definitions keep their
        // group id and stay globally queryable, and recreating the name
        // allocates a fresh group id. The name must exist (E367) and must
        // not be the group the caller is standing in (E936), and neither
        // path selects a group.
        let Some(group) = editor.autocmds().group(name) else {
            return error_flow(runtime, "E367", format!("No such group: \"{name}\""));
        };
        if group == runtime.current_augroup {
            return error_flow(
                runtime,
                "E936",
                "Cannot delete the current group".to_owned(),
            );
        }
        if !editor
            .autocmds()
            .query(&AutocmdFilter {
                group: Some(group),
                ..AutocmdFilter::default()
            })
            .is_empty()
        {
            push_text_message(
                editor,
                "W19: Deleting augroup that is still in use".to_owned(),
                false,
                true,
            );
        }
        return match editor.autocmds_mut().delete_group_legacy(group) {
            Ok(()) => Flow::Normal,
            Err(error) => error_flow(runtime, "E367", error.to_string()),
        };
    }
    match editor.autocmds_mut().create_group(name, false) {
        Ok(group) => {
            runtime.current_augroup = group;
            Flow::Normal
        }
        Err(error) => error_flow(runtime, "E936", error.to_string()),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "autocmd parsing keeps group, event, pattern, and action validation in source order"
)]
fn command_autocmd<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let args = command.args.trim();
    if args.is_empty() {
        return Flow::Normal;
    }
    if (args == "nvim.terminal"
        || args
            .strip_prefix("nvim.terminal")
            .is_some_and(|tail| tail.starts_with(char::is_whitespace)))
        && access.with_ex_editor(|editor| editor.autocmds().group("nvim.terminal").is_none())
    {
        let created = access
            .with_ex_editor(|editor| editor.autocmds_mut().create_group("nvim.terminal", false));
        if let Err(error) = created {
            return error_flow(runtime, "E936", error.to_string());
        }
    }
    if command.bang
        && let Some(group) = access.with_ex_editor(|editor| editor.autocmds().group(args))
    {
        let removed = access.with_ex_editor(|editor| {
            editor.autocmds_mut().clear(&AutocmdFilter {
                group: Some(group),
                ..AutocmdFilter::default()
            })
        });
        return match release_removed_autocmds(access, lua, removed) {
            Ok(()) => Flow::Normal,
            Err(error) => lua_error_flow(runtime, error, "E5107", "E5108"),
        };
    }
    let (first, tail) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(first, tail)| (first, tail.trim_start()));
    let named_group = access.with_ex_editor(|editor| editor.autocmds().group(first));
    let (group, command_tail) =
        named_group.map_or((runtime.current_augroup, args), |group| (group, tail));
    let mut words = command_tail
        .splitn(3, char::is_whitespace)
        .filter(|word| !word.is_empty());
    let Some(event_names) = words.next() else {
        return Flow::Normal;
    };
    // `arg_event_skip` (`autocmd.c:2374-2392`) scans a comma-separated event
    // list and rejects the whole command if any name is unknown, so the names
    // are all resolved before anything is registered. `runtime/plugin/gzip.vim`
    // opens with `autocmd BufReadPre,FileReadPre`, so without this every
    // plain startup failed on the first bundled plugin it sourced.
    let mut events = Vec::new();
    for event_name in event_names.split(',').filter(|name| !name.is_empty()) {
        match Event::from_name(event_name) {
            Some(event) => events.push(event),
            None => {
                return error_flow(
                    runtime,
                    "E216",
                    format!("No such group or event: {event_name}"),
                );
            }
        }
    }
    let pattern_text = words.next();
    if command.bang
        && first == "nvim.terminal"
        && events.contains(&Event::TermClose)
        && pattern_text.is_none()
    {
        runtime.terminal_exit_message = false;
    }
    let body = words
        .next()
        .unwrap_or("")
        .trim_start_matches(char::is_whitespace);
    let patterns = pattern_text.map(split_autocmd_patterns).unwrap_or_default();
    let current_buffer = access.with_ex_editor(|editor| editor.current_buffer());
    if command.bang {
        let clear_patterns = pattern_text.map(|_| buffer_local_patterns(&patterns, current_buffer));
        // `do_autocmd` (`ex_docmd.c`) deletes the selected definitions before
        // a trailing `{cmd}` registers the replacement; a bodyless bang only
        // clears. `<buffer>` items address the current buffer while clearing
        // too, so a replacement of a buffer-local definition hits the old
        // `<buffer=N>` entry rather than a literal pattern that matches none.
        let removed = access.with_ex_editor(|editor| {
            editor.autocmds_mut().clear(&AutocmdFilter {
                group: Some(group),
                events: Some(&events),
                patterns: clear_patterns.as_deref(),
                ..AutocmdFilter::default()
            })
        });
        if let Err(error) = release_removed_autocmds(access, lua, removed) {
            return lua_error_flow(runtime, error, "E5107", "E5108");
        }
        if body.is_empty() {
            return Flow::Normal;
        }
    }
    if body.is_empty() {
        return Flow::Normal;
    }
    // `aucmd_span_pattern` (`autocmd.c:956`) walks the comma-separated pattern
    // list inside `register_legacy`, so one `:autocmd` stores one entry per
    // event-pattern pair, and none of them carries an API id.
    if let Err(error) = access.with_ex_editor(|editor| {
        editor.autocmds_mut().register_legacy(
            &events,
            pattern_text.unwrap_or("*"),
            &AutocmdKind::ExString(body.to_owned()),
            &AutocmdOptions {
                group,
                buffer: legacy_buffer_pattern(&patterns, current_buffer),
                ..AutocmdOptions::default()
            },
        )
    }) {
        return error_flow(runtime, "E216", error.to_string());
    }
    Flow::Normal
}

/// Whether a legacy `:autocmd` pattern list selects the current buffer with
/// `<buffer>` or `<buffer=0>` (`do_autocmd`'s `<buffer` scan): the registration
/// core then binds every entry to that buffer and canonicalizes the stored
/// pattern to `<buffer=N>`. Ordinary patterns bind nothing.
fn legacy_buffer_pattern(patterns: &[String], current: Option<BufHandle>) -> Option<BufHandle> {
    let binds = patterns
        .iter()
        .any(|pattern| pattern == "<buffer>" || pattern == "<buffer=0>");
    if binds { current } else { None }
}

/// A clearing pattern list with `<buffer>`/`<buffer=0>` items rewritten to the
/// current buffer's canonical `<buffer=N>`, so a bang clear (with or without a
/// replacement body) matches the stored buffer-local entries.
fn buffer_local_patterns(patterns: &[String], current: Option<BufHandle>) -> Vec<String> {
    let Some(buffer) = legacy_buffer_pattern(patterns, current) else {
        return patterns.to_vec();
    };
    let canonical = format!("<buffer={}>", i64::from(buffer));
    patterns
        .iter()
        .map(|pattern| {
            if pattern == "<buffer>" || pattern == "<buffer=0>" {
                canonical.clone()
            } else {
                pattern.clone()
            }
        })
        .collect()
}

/// Splits an `:autocmd` pattern list on its unescaped commas, upstream's
/// `aucmd_span_pattern` (`autocmd.c`). A `\,` belongs to the pattern.
fn split_autocmd_patterns(text: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in text.chars() {
        match character {
            '\\' if !escaped => {
                escaped = true;
                current.push('\\');
            }
            ',' if !escaped => {
                if !current.is_empty() {
                    patterns.push(std::mem::take(&mut current));
                }
            }
            _ => {
                escaped = false;
                current.push(character);
            }
        }
    }
    if !current.is_empty() {
        patterns.push(current);
    }
    if patterns.is_empty() {
        patterns.push("*".to_owned());
    }
    patterns
}

#[expect(
    clippy::too_many_lines,
    reason = ":command keeps Vim's list, define, and modifier flows in one interpreter"
)]
fn command_user_command<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let args = command.args.trim();
    if args.is_empty() {
        // `:command` lists global and current-buffer-local names.
        let mut names: Vec<String> = runtime
            .user_commands
            .borrow()
            .commands
            .keys()
            .cloned()
            .collect();
        if let Some(buffer) = editor.current_buffer()
            && let Some(local) = runtime.user_commands.borrow().buffer_commands.get(&buffer)
        {
            names.extend(local.keys().cloned());
        }
        names.sort();
        names.dedup();
        for name in names {
            push_text_message(editor, name, false, false);
        }
        return Flow::Normal;
    }
    let mut words = args.split_ascii_whitespace().peekable();
    let mut nargs = '0';
    let mut accepts_bang = false;
    let mut accepts_range = false;
    let mut accepts_register = false;
    let mut keepscript = false;
    let mut bar = false;
    let mut buffer_local = false;
    let mut accepts_count = false;
    let mut count_default: Option<i64> = None;
    let mut default_range: Option<UserCommandRange> = None;
    let mut completion: Option<UserCommandComplete> = None;
    let mut complete_arg: Option<String> = None;
    let mut addr_type = AddrType::Lines;
    while words.peek().is_some_and(|word| word.starts_with('-')) {
        let flag = words.next().unwrap_or_default();
        if let Some(value) = flag.strip_prefix("-nargs=") {
            let mut chars = value.chars();
            let Some(first) = chars.next() else {
                return error_flow(
                    runtime,
                    "E176",
                    format!("Invalid number of arguments: {value}"),
                );
            };
            if chars.next().is_some() || !matches!(first, '0' | '1' | '?' | '+' | '*') {
                return error_flow(
                    runtime,
                    "E176",
                    format!("Invalid number of arguments: {value}"),
                );
            }
            nargs = first;
        } else if flag == "-bang" {
            accepts_bang = true;
        } else if flag == "-register" {
            accepts_register = true;
        } else if flag == "-keepscript" {
            keepscript = true;
        } else if flag == "-bar" {
            bar = true;
        } else if flag == "-buffer" {
            buffer_local = true;
        } else if flag == "-range" {
            accepts_range = true;
            default_range = Some(UserCommandRange::Dot);
        } else if flag == "-range=%" {
            accepts_range = true;
            default_range = Some(UserCommandRange::Percent);
        } else if let Some(value) = flag.strip_prefix("-range=") {
            let Ok(default) = value.parse::<i64>() else {
                return error_flow(runtime, "E181", format!("Invalid attribute: {flag}"));
            };
            accepts_range = true;
            default_range = Some(UserCommandRange::Count(default));
        } else if flag == "-count" {
            // "-count is like -range with a count default" (`:h :command`).
            accepts_count = true;
            count_default = Some(0);
        } else if let Some(value) = flag.strip_prefix("-count=") {
            let Ok(default) = value.parse::<i64>() else {
                return error_flow(runtime, "E181", format!("Invalid attribute: {flag}"));
            };
            accepts_count = true;
            count_default = Some(default);
        } else if let Some(value) = flag.strip_prefix("-complete=") {
            if let Some((kind, argument)) = value.split_once(',') {
                if !matches!(kind, "custom" | "customlist") || argument.is_empty() {
                    return error_flow(runtime, "E179", format!("Invalid complete value: {value}"));
                }
                completion = Some(UserCommandComplete::Name(kind.to_owned()));
                complete_arg = Some(argument.to_owned());
            } else if is_named_completion(value) {
                completion = Some(UserCommandComplete::Name(value.to_owned()));
            } else {
                return error_flow(runtime, "E179", format!("Invalid complete value: {value}"));
            }
        } else if let Some(value) = flag.strip_prefix("-addr=") {
            let Some(parsed) = addr_type_from_name(value) else {
                return error_flow(runtime, "E181", format!("Invalid attribute: {flag}"));
            };
            addr_type = parsed;
        } else {
            return error_flow(runtime, "E181", format!("Invalid attribute: {flag}"));
        }
    }
    if accepts_count {
        accepts_range = true;
        if default_range.is_none() {
            default_range = Some(UserCommandRange::Dot);
        }
    }
    let Some(name) = words.next() else {
        return error_flow(runtime, "E183", "User defined commands must be capitalized");
    };
    if !valid_user_command_name(name) {
        return error_flow(runtime, "E183", "User defined commands must be capitalized");
    }
    let script_context = runtime.scripts.current_context();
    let target = if buffer_local {
        match editor.current_buffer() {
            Some(buffer) => Some(buffer),
            None => return error_flow(runtime, "E749", "Empty buffer"),
        }
    } else {
        None
    };
    {
        let mut registry = runtime.user_commands.borrow_mut();
        let scope = registry.scope_mut(target);
        if let Some(existing) = scope.get(name) {
            // "Command can be replaced with command! and when sourcing the same
            // script again, but only once" (`usercmd.c:940-948`): the same SID
            // with a *different* sequence number is a reload and replaces
            // silently; anything else is E174.
            let same_script_reload = existing.script_context.sid == script_context.sid
                && existing.script_context.seq != script_context.seq;
            if !command.bang && !same_script_reload {
                return error_flow(
                    runtime,
                    "E174",
                    "Command already exists: add ! to replace it",
                );
            }
        }
        let body = words.collect::<Vec<_>>().join(" ");
        // Resolve `<SID>` in the body to the script-local SNR prefix
        // (`keycodes.c` `replace_termcodes`): K_SPECIAL (0x80) + KS_EXTRA
        // (0xFD) + KE_SNR (0x52) + script_id + `_`. Outside a script (SID 0)
        // `<SID>` stays literal — upstream emits `e_usingsid` at runtime.
        let body = if script_context.sid != 0 {
            body.replace("<SID>", &format!("\u{80}\u{fd}R{}_", script_context.sid))
        } else {
            body
        };
        scope.insert(
            name.to_owned(),
            UserCommand {
                name: name.to_owned(),
                body,
                nargs,
                accepts_bang,
                accepts_range,
                accepts_register,
                bar,
                accepts_count,
                count_default,
                addr_type,
                default_range,
                desc: String::new(),
                completion,
                complete_arg,
                callback: None,
                preview: None,
                script_id: i64::try_from(script_context.sid).unwrap_or(i64::MAX),
                script_context,
                keepscript,
            },
        );
    }
    Flow::Normal
}

/// Whether `value` is one of `command_complete[]`'s named completion types
/// (`usercmd.c`). `custom`/`customlist` are handled by the caller.
fn is_named_completion(value: &str) -> bool {
    matches!(
        value,
        "arglist"
            | "augroup"
            | "behave"
            | "breakpoint"
            | "buffer"
            | "color"
            | "command"
            | "compiler"
            | "dir"
            | "dir_in_path"
            | "environment"
            | "event"
            | "expression"
            | "file"
            | "file_in_path"
            | "filetype"
            | "function"
            | "help"
            | "highlight"
            | "history"
            | "keymap"
            | "locale"
            | "lua"
            | "mapclear"
            | "mapping"
            | "menu"
            | "messages"
            | "packadd"
            | "runtime"
            | "scriptnames"
            | "shellcmd"
            | "sign"
            | "syntax"
            | "syntime"
            | "tag"
            | "tag_listfiles"
            | "user"
            | "var"
    )
}

/// Maps one `-addr=` value onto its address domain (`usercmd.c`'s
/// `addr_type` names).
fn addr_type_from_name(value: &str) -> Option<AddrType> {
    Some(match value {
        "lines" => AddrType::Lines,
        "windows" => AddrType::Windows,
        "arguments" => AddrType::Arguments,
        "buffers" => AddrType::Buffers,
        "loaded" => AddrType::LoadedBuffers,
        "tabs" => AddrType::Tabs,
        "quickfix" => AddrType::QuickFix,
        "other" => AddrType::Other,
        "none" => AddrType::None,
        _ => return None,
    })
}

fn command_delcommand<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let mut buffer_local = false;
    let mut name: Option<&str> = None;
    for word in command.args.split_ascii_whitespace() {
        if word == "-buffer" && name.is_none() {
            buffer_local = true;
        } else {
            name = Some(word);
            break;
        }
    }
    let Some(name) = name else {
        return error_flow(runtime, "E471", "Argument required");
    };
    // `-buffer` deletes from the current buffer's table only; the global
    // form never sees buffer-local names (`ex_docmd.c` `do_delcommand`).
    let target = if buffer_local {
        match editor.current_buffer() {
            Some(buffer) => Some(buffer),
            None => return error_flow(runtime, "E749", "Empty buffer"),
        }
    } else {
        None
    };
    let removed = {
        let mut registry = runtime.user_commands.borrow_mut();
        registry.scope_mut(target).remove(name).is_some()
    };
    if !removed && !command.bang {
        return error_flow(
            runtime,
            "E184",
            format!("No such user-defined command: {name}"),
        );
    }
    Flow::Normal
}

#[expect(
    clippy::too_many_lines,
    reason = "user-command invocation keeps Vim's expansion and dispatch order in one flow"
)]
fn command_invoke_user<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    command: &ExCommand,
) -> Flow {
    // The live view: the current buffer's table first, then the global one,
    // so a body redefined or made buffer-local since the line was parsed
    // still resolves here (`uc_find`'s order).
    let definition = {
        let registry = runtime.user_commands.borrow();
        let provider = UserCommandLookup {
            registry: &registry,
            buffer: access.with_ex_editor(|editor| editor.current_buffer()),
        };
        provider.get(name).cloned()
    };
    let Some(definition) = definition else {
        return error_flow(runtime, "E492", format!("Not an editor command: {name}"));
    };
    // `<args>`/`<q-args>` expand `ea.arg` as written (`uc_check_code`), so
    // the argument is not trimmed again here: the parser already skipped
    // leading space/tab and removed unescaped trailing space/tab, and a CR
    // upstream keeps must survive into the expansion.
    let args = command.args.as_str();
    let count = count_ex_arguments(args);
    let valid = match definition.nargs {
        '0' => count == 0,
        '1' => count == 1,
        '?' => count <= 1,
        '+' => count >= 1,
        '*' => true,
        _ => false,
    };
    // `-nargs=0` clears EX_EXTRA, so `do_one_cmd` rejects any argument text
    // before the command body runs (`ex_docmd.c:4542`). E471 belongs to the
    // forms that require an argument and did not get one.
    if definition.nargs == '0' && !args.is_empty() {
        return error_flow(runtime, "E488", format!("Trailing characters: {args}"));
    }
    if !valid {
        return error_flow(runtime, "E471", "Argument required");
    }
    if command.bang && !definition.accepts_bang {
        return error_flow(runtime, "E477", "No ! allowed");
    }
    if command.range.is_some() && !definition.accepts_range {
        return error_flow(runtime, "E481", "No range allowed");
    }
    if command.register.is_some() && !definition.accepts_register {
        return error_flow(runtime, "E488", "Trailing characters");
    }
    let (line1, line2, count) =
        access.with_ex_editor(|editor| user_command_addresses(editor, command, &definition));
    if let Some(reference) = definition.callback {
        let Some(lua) = lua else {
            let error = LuaExecError::Runtime("Lua callbacks are not installed".to_owned());
            return lua_error_flow(runtime, error, "E5107", "E5108");
        };
        if let Err(error) = access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
            return exec_error_flow(runtime, error);
        }
        let opts = user_command_opts(name, command, &definition, args, line1, line2, count);
        let result = lua.borrow_mut().invoke_callback(
            usize::try_from(reference).unwrap_or(usize::MAX),
            vec![Object::Dict(opts)],
        );
        let sync = access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
        return match (result, sync) {
            (Err(error), _) => lua_error_flow(runtime, error, "E5107", "E5108"),
            (Ok(_), Err(error)) => exec_error_flow(runtime, error),
            (Ok(_), Ok(())) => Flow::Normal,
        };
    }
    let expanded = definition
        .body
        .replace("<f-args>", &split_command_arguments(args))
        .replace("<args>", args)
        .replace("<q-args>", &format!("'{}'", args.replace('\'', "''")))
        .replace("<bang>", if command.bang { "!" } else { "" })
        .replace("<mods>", &render_command_mods(&command.modifiers))
        .replace("<line1>", &line1.to_string())
        .replace("<line2>", &line2.to_string())
        .replace("<count>", &count.to_string())
        .replace(
            "<reg>",
            &command
                .register
                .map_or(String::new(), |value| value.to_string()),
        );
    let first_line = runtime.scripts.current_line();
    let logical = vec![LogicalLine {
        text: expanded,
        first_line,
    }];
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    let sid = definition.script_context.sid;
    let switched = !definition.keepscript && sid != 0 && runtime.scripts.current_sid() != Some(sid);
    let caller_script = scope.script.clone();
    if !definition.keepscript && sid != 0 {
        let name = format!("command {name}");
        runtime
            .scripts
            .push_alias_source(sid, runtime.scripts.current_seq(), 0, name);
        if switched {
            runtime.scripts.load_script_scope(sid, scope);
        }
    }
    let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
    if !definition.keepscript && sid != 0 {
        if switched {
            runtime.scripts.store_script_scope(sid, scope);
            scope.script = caller_script;
        }
        runtime.scripts.pop_source();
    }
    flow
}

/// Resolves one user command's `-range`/`-count` defaults — upstream
/// `do_ucmd`'s line1/line2 selection and `<count>`. One helper shared by
/// execution and [`ExExecutor::parse_cmdline`] so both agree by construction.
pub(crate) fn user_command_addresses(
    editor: &Editor,
    command: &ExCommand,
    definition: &UserCommand,
) -> (usize, usize, i64) {
    if command.range.is_some() {
        // With -count the addresses fold into `<count>` unclamped
        // (`set_cmd_count` bounds the count, not the range).
        let (start, end) = if definition.accepts_count {
            resolve_range_raw(editor, command).unwrap_or_else(|_| current_line_pair(editor))
        } else {
            resolve_range(editor, command).unwrap_or_else(|_| current_line_pair(editor))
        };
        let count = if definition.accepts_count {
            command.count.map_or_else(
                || i64::try_from(end).unwrap_or(i64::MAX),
                |value| i64::try_from(value).unwrap_or(i64::MAX),
            )
        } else {
            0
        };
        (start, end, count)
    } else {
        // No explicit range. A post-command count on a `-count` command
        // (`:Cmd 42 h`) folds into `<count>` via `set_cmd_count`: for
        // ADDR_OTHER `line2 = count`; for ADDR_LINES `line1 = old_line2`,
        // `line2 = old_line2 + count - 1`. In both cases `line1` stays at
        // its default (1 for Other, cursor for Lines) and `line2` becomes
        // the count (Lines: cursor + count - 1 = count when cursor = 1).
        if definition.accepts_count && command.count.is_some() {
            let count = i64::try_from(command.count.unwrap_or(0)).unwrap_or(i64::MAX);
            let (current, _) = current_line_pair(editor);
            let line2 = if matches!(definition.addr_type, AddrType::Lines) {
                current.saturating_add(
                    usize::try_from(count)
                        .unwrap_or(usize::MAX)
                        .saturating_sub(1),
                )
            } else {
                usize::try_from(count.max(0)).unwrap_or(usize::MAX)
            };
            return (current.max(1), line2, count);
        }
        let (start, end) = match definition.default_range {
            Some(UserCommandRange::Dot) | None => current_line_pair(editor),
            Some(UserCommandRange::Percent) => (1, buffer_last_line(editor)),
            Some(UserCommandRange::Count(value)) => {
                let line = usize::try_from(value.max(0)).unwrap_or(0);
                (line, line)
            }
        };
        let count = if definition.accepts_count {
            definition.count_default.unwrap_or(0)
        } else {
            0
        };
        (start, end, count)
    }
}

/// The `opts` table upstream passes to a Lua command callback
/// (`nlua_do_ucmd` + `nlua_push_eap`): args, fargs, bang, line1/line2,
/// range, count, reg, mods, smods, nargs, and name.
#[expect(
    clippy::too_many_lines,
    reason = "the opts table mirrors upstream nlua_push_eap's full modifier construction"
)]
fn user_command_opts(
    name: &str,
    command: &ExCommand,
    definition: &UserCommand,
    args: &str,
    line1: usize,
    line2: usize,
    count: i64,
) -> Dict {
    // `fargs`: NOSPC (nargs=1/?) keeps the whole argument text as one element;
    // otherwise split on unescaped whitespace (`uc_split_args_iter`).
    let fargs = if definition.flags().contains(CommandFlags::NOSPC) {
        if args.is_empty() {
            Vec::new()
        } else {
            vec![args.to_owned()]
        }
    } else {
        split_fargs(args)
    };
    // `range` (`eap->addr_count`): 0 when no range/count, 1 for a single
    // address or post-command count, 2 for a pair or `%`.
    let range = match &command.range {
        Some(range) => match range.kind {
            RangeKind::Single => 1,
            RangeKind::WholeBuffer | RangeKind::Pair { .. } => 2,
        },
        None => i64::from(command.count.is_some()),
    };
    let boolean = |kind: ModifierKind| {
        command
            .modifiers
            .iter()
            .any(|modifier| modifier.kind == kind)
    };
    // `tab`/`verbose` default to -1 when absent (upstream stores count+1,
    // subtracts 1; absent → 0-1 = -1).
    let modifier_count_or = |kind: ModifierKind, default: i64| {
        command
            .modifiers
            .iter()
            .find(|modifier| modifier.kind == kind)
            .and_then(|modifier| modifier.count)
            .map_or(default, |count| i64::try_from(count).unwrap_or(i64::MAX))
    };
    let split = command.modifiers.iter().find_map(|modifier| {
        let name = match modifier.kind {
            ModifierKind::TopLeft => "topleft",
            ModifierKind::BotRight => "botright",
            ModifierKind::AboveLeft | ModifierKind::LeftAbove => "aboveleft",
            ModifierKind::BelowRight | ModifierKind::RightBelow => "belowright",
            _ => return None,
        };
        Some(OxStr::from(name))
    });
    let filter_mod = command
        .modifiers
        .iter()
        .find(|modifier| modifier.kind == ModifierKind::Filter);
    let entries =
        vec![
            (OxStr::from("args"), Object::String(OxStr::from(args))),
            (
                OxStr::from("fargs"),
                Object::Array(
                    fargs
                        .into_iter()
                        .map(|piece| Object::String(OxStr(piece.into_bytes())))
                        .collect(),
                ),
            ),
            (OxStr::from("bang"), Object::Boolean(command.bang)),
            (
                OxStr::from("line1"),
                Object::Integer(i64::try_from(line1).unwrap_or(i64::MAX)),
            ),
            (
                OxStr::from("line2"),
                Object::Integer(i64::try_from(line2).unwrap_or(i64::MAX)),
            ),
            (OxStr::from("range"), Object::Integer(range)),
            (OxStr::from("count"), Object::Integer(count)),
            (
                OxStr::from("reg"),
                Object::String(command.register.map_or_else(
                    || OxStr(Vec::new()),
                    |value| OxStr(value.to_string().into_bytes()),
                )),
            ),
            (
                OxStr::from("mods"),
                Object::String(OxStr(render_command_mods(&command.modifiers).into_bytes())),
            ),
            (
                OxStr::from("smods"),
                Object::Dict(Dict(vec![
                    (
                        OxStr::from("browse"),
                        Object::Boolean(boolean(ModifierKind::Browse)),
                    ),
                    (
                        OxStr::from("confirm"),
                        Object::Boolean(boolean(ModifierKind::Confirm)),
                    ),
                    (
                        OxStr::from("emsg_silent"),
                        Object::Boolean(command.modifiers.iter().any(|modifier| {
                            modifier.kind == ModifierKind::Silent && modifier.bang
                        })),
                    ),
                    (
                        OxStr::from("hide"),
                        Object::Boolean(boolean(ModifierKind::Hide)),
                    ),
                    (
                        OxStr::from("horizontal"),
                        Object::Boolean(boolean(ModifierKind::Horizontal)),
                    ),
                    (
                        OxStr::from("keepalt"),
                        Object::Boolean(boolean(ModifierKind::KeepAlt)),
                    ),
                    (
                        OxStr::from("keepjumps"),
                        Object::Boolean(boolean(ModifierKind::KeepJumps)),
                    ),
                    (
                        OxStr::from("keepmarks"),
                        Object::Boolean(boolean(ModifierKind::KeepMarks)),
                    ),
                    (
                        OxStr::from("keeppatterns"),
                        Object::Boolean(boolean(ModifierKind::KeepPatterns)),
                    ),
                    (
                        OxStr::from("lockmarks"),
                        Object::Boolean(boolean(ModifierKind::LockMarks)),
                    ),
                    (
                        OxStr::from("noautocmd"),
                        Object::Boolean(boolean(ModifierKind::NoAutocmd)),
                    ),
                    (
                        OxStr::from("noswapfile"),
                        Object::Boolean(boolean(ModifierKind::NoSwapfile)),
                    ),
                    (
                        OxStr::from("sandbox"),
                        Object::Boolean(boolean(ModifierKind::Sandbox)),
                    ),
                    (
                        OxStr::from("silent"),
                        Object::Boolean(
                            command
                                .modifiers
                                .iter()
                                .any(|modifier| modifier.kind == ModifierKind::Silent),
                        ),
                    ),
                    (
                        OxStr::from("split"),
                        Object::String(split.unwrap_or_else(|| OxStr::from(""))),
                    ),
                    (
                        OxStr::from("tab"),
                        Object::Integer(modifier_count_or(ModifierKind::Tab, -1)),
                    ),
                    (
                        OxStr::from("unsilent"),
                        Object::Boolean(boolean(ModifierKind::Unsilent)),
                    ),
                    (
                        OxStr::from("verbose"),
                        Object::Integer(modifier_count_or(ModifierKind::Verbose, -1)),
                    ),
                    (
                        OxStr::from("vertical"),
                        Object::Boolean(boolean(ModifierKind::Vertical)),
                    ),
                    (
                        OxStr::from("filter"),
                        Object::Dict(Dict(vec![
                            (
                                OxStr::from("pattern"),
                                Object::String(OxStr(
                                    filter_mod
                                        .and_then(|m| m.pattern.as_ref())
                                        .map_or(String::new(), String::clone)
                                        .into_bytes(),
                                )),
                            ),
                            (
                                OxStr::from("force"),
                                Object::Boolean(filter_mod.is_some_and(|m| m.bang)),
                            ),
                        ])),
                    ),
                ])),
            ),
            (
                OxStr::from("nargs"),
                Object::String(OxStr(definition.nargs.to_string().into_bytes())),
            ),
            (OxStr::from("name"), Object::String(OxStr::from(name))),
        ];
    Dict(entries)
}

/// `uc_split_args_iter`: whitespace split honoring `\ ` and `\\` escapes,
/// dropping empty segments. Returns the fargs list for a Lua command callback.
fn split_fargs(arg: &str) -> Vec<String> {
    let bytes = arg.as_bytes();
    let len = bytes.len();
    let mut values: Vec<String> = Vec::new();
    if len == 0 {
        return values;
    }
    let mut end = 0usize;
    loop {
        let mut pos = end;
        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let mut chunk: Vec<u8> = Vec::new();
        let mut done = true;
        while pos < len.saturating_sub(1) {
            if bytes[pos] == b'\\'
                && (bytes[pos + 1] == b'\\' || bytes[pos + 1].is_ascii_whitespace())
            {
                pos += 1;
                chunk.push(bytes[pos]);
            } else {
                chunk.push(bytes[pos]);
            }
            if bytes[pos + 1].is_ascii_whitespace() {
                end = pos + 1;
                done = false;
                break;
            }
            pos += 1;
        }
        if done && pos < len && !bytes[pos].is_ascii_whitespace() {
            chunk.push(bytes[pos]);
        }
        if !chunk.is_empty() {
            values.push(String::from_utf8_lossy(&chunk).into_owned());
        }
        if done {
            return values;
        }
    }
}

/// `<mods>` (`uc_check_code` / `uc_mods`): the modifiers in upstream's
/// canonical order — simple flags, then `silent`/`silent!`, then `verbose`,
/// then the window-split group — separated by single spaces, no trailing
/// space. Empty when the invocation had none, so `<mods>cexpr` becomes `cexpr`.
fn render_command_mods(modifiers: &[CommandModifier]) -> String {
    let has = |kind: ModifierKind| modifiers.iter().any(|modifier| modifier.kind == kind);
    let count_of = |kind: ModifierKind| {
        modifiers
            .iter()
            .find(|modifier| modifier.kind == kind)
            .and_then(|modifier| modifier.count)
    };
    let bang_of = |kind: ModifierKind| {
        modifiers
            .iter()
            .find(|modifier| modifier.kind == kind)
            .is_some_and(|modifier| modifier.bang)
    };
    let filter_mod = modifiers.iter().find(|m| m.kind == ModifierKind::Filter);
    let mut parts: Vec<String> = Vec::new();
    // Simple flags in upstream's `mod_entries` table order.
    for (kind, name) in [
        (ModifierKind::Browse, "browse"),
        (ModifierKind::Confirm, "confirm"),
        (ModifierKind::Hide, "hide"),
        (ModifierKind::KeepAlt, "keepalt"),
        (ModifierKind::KeepJumps, "keepjumps"),
        (ModifierKind::KeepMarks, "keepmarks"),
        (ModifierKind::KeepPatterns, "keeppatterns"),
        (ModifierKind::LockMarks, "lockmarks"),
        (ModifierKind::NoSwapfile, "noswapfile"),
        (ModifierKind::Unsilent, "unsilent"),
        (ModifierKind::NoAutocmd, "noautocmd"),
        (ModifierKind::Sandbox, "sandbox"),
    ] {
        if has(kind) {
            parts.push(name.to_owned());
        }
    }
    // `:silent` / `:silent!`.
    if has(ModifierKind::Silent) {
        parts.push(if bang_of(ModifierKind::Silent) {
            "silent!".to_owned()
        } else {
            "silent".to_owned()
        });
    }
    // `:verbose` / `:Nverbose`.
    if has(ModifierKind::Verbose) {
        parts.push(match count_of(ModifierKind::Verbose) {
            Some(count) if count > 1 => format!("{count}verbose"),
            _ => "verbose".to_owned(),
        });
    }
    // `:filter` / `:filter! /pat/`.
    if let Some(filter) = filter_mod {
        let mut s = String::from("filter");
        if filter.bang {
            s.push('!');
        }
        if let Some(pattern) = &filter.pattern {
            s.push(' ');
            s.push_str(pattern);
        }
        parts.push(s);
    }
    // Window-split group in `add_win_cmd_modifiers` order.
    if has(ModifierKind::AboveLeft) || has(ModifierKind::LeftAbove) {
        parts.push("aboveleft".to_owned());
    }
    if has(ModifierKind::BelowRight) || has(ModifierKind::RightBelow) {
        parts.push("belowright".to_owned());
    }
    if has(ModifierKind::BotRight) {
        parts.push("botright".to_owned());
    }
    if has(ModifierKind::Tab) {
        parts.push(match count_of(ModifierKind::Tab) {
            Some(count) if count > 0 => format!("{count}tab"),
            _ => "tab".to_owned(),
        });
    }
    if has(ModifierKind::TopLeft) {
        parts.push("topleft".to_owned());
    }
    if has(ModifierKind::Vertical) {
        parts.push("vertical".to_owned());
    }
    if has(ModifierKind::Horizontal) {
        parts.push("horizontal".to_owned());
    }
    let joined = parts.join(" ");
    if joined.is_empty() {
        joined
    } else {
        format!("{joined} ")
    }
}

/// Expand `<f-args>`: split the argument text on unescaped whitespace and emit
/// each piece as a double-quoted string, the way `uc_split_args()` does in
/// `usercmd.c`. An empty argument list expands to nothing, not to `""`.
pub(crate) fn split_command_arguments(args: &str) -> String {
    if args.is_empty() {
        return String::new();
    }
    let bytes = args.as_bytes();
    let mut expanded = String::with_capacity(args.len() + 2);
    expanded.push('"');
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match (byte, bytes.get(index + 1).copied()) {
            (b'\\', Some(b'\\')) => {
                expanded.push_str("\\\\");
                index += 2;
            }
            // A backslash-escaped space or tab joins two words into one argument.
            (b'\\', Some(white @ (b' ' | b'\t'))) => {
                expanded.push(char::from(white));
                index += 2;
            }
            (b'\\' | b'"', _) => {
                expanded.push('\\');
                expanded.push(char::from(byte));
                index += 1;
            }
            (b' ' | b'\t', _) => {
                while matches!(bytes.get(index), Some(b' ' | b'\t')) {
                    index += 1;
                }
                if index == bytes.len() {
                    break;
                }
                expanded.push_str("\", \"");
            }
            _ => {
                let start = index;
                index += 1;
                while index < bytes.len() && bytes[index] & 0xc0 == 0x80 {
                    index += 1;
                }
                expanded.push_str(&args[start..index]);
            }
        }
    }
    expanded.push('"');
    expanded
}

fn count_ex_arguments(args: &str) -> usize {
    let mut count = 0usize;
    let mut in_argument = false;
    let mut quote = None;
    let mut escaped = false;
    let mut delimiters = Vec::new();
    let mut characters = args.chars().peekable();

    while let Some(character) = characters.next() {
        if escaped {
            escaped = false;
            in_argument = true;
            continue;
        }
        if character == '\\' {
            escaped = true;
            in_argument = true;
            continue;
        }
        if let Some(active) = quote {
            in_argument = true;
            if character == active {
                if characters.peek() == Some(&active) {
                    characters.next();
                } else {
                    quote = None;
                }
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            in_argument = true;
            continue;
        }
        match character {
            '(' => delimiters.push(')'),
            '[' => delimiters.push(']'),
            '{' => delimiters.push('}'),
            ')' | ']' | '}' if delimiters.last() == Some(&character) => {
                delimiters.pop();
            }
            _ => {}
        }
        if character.is_whitespace() && delimiters.is_empty() {
            if in_argument {
                count += 1;
                in_argument = false;
            }
        } else {
            in_argument = true;
        }
    }
    count + usize::from(in_argument)
}

/// The `<buffer>`/`<nowait>`/`<silent>`/`<special>`/`<script>`/`<expr>`/`<unique>`
/// prefix of a `:map` argument (`str_to_mapargs`, `mapping.c:400-451`).
///
/// They are only recognized at the *front*, in any order, each followed by
/// optional whitespace, and they are removed before the left-hand side is
/// read. Scanning the whole argument for them instead — and leaving them in
/// place — made the modifier itself the left-hand side, so
/// `nnoremap <silent> ,x :cmd<CR>` registered `<silent>`.
#[derive(Clone, Copy, Default)]
struct MapModifiers(u8);

impl MapModifiers {
    const BUFFER: Self = Self(1 << 0);
    const NOWAIT: Self = Self(1 << 1);
    const SILENT: Self = Self(1 << 2);
    const SCRIPT: Self = Self(1 << 3);
    const EXPR: Self = Self(1 << 4);
    const UNIQUE: Self = Self(1 << 5);

    const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
}

fn split_map_modifiers(args: &str) -> (MapModifiers, &str) {
    let mut flags = MapModifiers::default();
    let mut rest = args.trim_start();
    loop {
        let matched = [
            ("<buffer>", MapModifiers::BUFFER),
            ("<nowait>", MapModifiers::NOWAIT),
            ("<silent>", MapModifiers::SILENT),
            ("<script>", MapModifiers::SCRIPT),
            ("<expr>", MapModifiers::EXPR),
            ("<unique>", MapModifiers::UNIQUE),
        ]
        .into_iter()
        .find_map(|(name, flag)| rest.strip_prefix(name).map(|tail| (flag, tail)))
        // "<special>" is accepted and ignored, as upstream does.
        .or_else(|| {
            rest.strip_prefix("<special>")
                .map(|tail| (MapModifiers::default(), tail))
        });
        match matched {
            Some((flag, tail)) => {
                flags = flags.with(flag);
                rest = tail.trim_start();
            }
            None => return (flags, rest),
        }
    }
}

/// `mapleader`/`maplocalleader`, defaulting to a backslash
/// (`replace_termcodes`, `keycodes.c`).
///
/// Read from the live `Scope` rather than the editor's `g:` dictionary,
/// because the two are only synced at the end of a program: a script that sets
/// `g:mapleader` and then defines a `<Leader>` mapping on the next line would
/// otherwise see the value it had on entry.
pub(crate) fn map_leader(scope: &Scope, name: &str) -> String {
    scope
        .get_scoped(ScopeKind::Global, name.as_bytes(), 0)
        .map_or_else(|_| "\\".to_owned(), typval_to_text)
}

fn command_map<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &Scope,
    command: &ExCommand,
) -> Flow {
    let name = command.command.name();
    let modes = map_modes(name, command.bang);
    let (flags, args) = split_map_modifiers(&command.args);
    let leader = map_leader(scope, "mapleader");
    let local_leader = map_leader(scope, "maplocalleader");
    let map_scope = if flags.contains(MapModifiers::BUFFER) {
        MapScope::Buffer(editor.current_buffer().unwrap_or(BufHandle::CURRENT))
    } else {
        MapScope::Global
    };
    if name.ends_with("clear") {
        editor.mappings_mut().mapclear(modes, map_scope);
        return Flow::Normal;
    }
    // `str_to_mapargs` (`mapping.c:463-475`): the lhs runs to the next space
    // or tab, with a CTRL-V or backslash pulling the following byte in even
    // when it is whitespace; `:unmap` takes literal whitespace so the whole
    // rest is its lhs. The rhs is `skipwhite(lhs_end)` to the end of the
    // argument — never trimmed, so a trailing space or CR is part of it.
    let is_unmap = name.ends_with("unmap");
    let bytes = args.as_bytes();
    let mut lhs_end = 0;
    while lhs_end < bytes.len() && (is_unmap || !matches!(bytes[lhs_end], b' ' | b'\t')) {
        if matches!(bytes[lhs_end], 0x16 | b'\\') && lhs_end + 1 < bytes.len() {
            lhs_end += 1;
        }
        lhs_end += 1;
    }
    let lhs = Keys::parse_notation(&args[..lhs_end], &leader, &local_leader);
    if is_unmap {
        if lhs_end == 0 {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        // `do_map`'s `retval = 2` (`mapping.c`): unmapping something that is
        // not mapped is an error, not a silent no-op.
        return if editor.mappings_mut().unmap(&lhs, modes, map_scope) == 0 {
            error_flow(runtime, "E31", "No such mapping")
        } else {
            Flow::Normal
        };
    }
    let rhs = &args[lhs_end
        + args[lhs_end..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count()..];
    // `do_map` prints instead of defining whenever either half is missing
    // (`mapping.c:873-883`), so a bare `:nmap` lists every mapping and
    // `:nmap ,a` lists the ones whose lhs and `,a` share a prefix.
    if rhs.is_empty() {
        return list_mappings(editor, &lhs, modes, flags.contains(MapModifiers::BUFFER));
    }
    if flags.contains(MapModifiers::UNIQUE) && editor.mappings().conflicts(&lhs, modes, map_scope) {
        return error_flow(
            runtime,
            "E227",
            format!(
                "Mapping already exists for {}",
                String::from_utf8_lossy(lhs.as_bytes())
            ),
        );
    }
    let action = if flags.contains(MapModifiers::EXPR) {
        MappingAction::Expr(rhs.to_owned())
    } else {
        match MappingAction::parse_rhs(rhs, &leader, &local_leader) {
            Ok(action) => action,
            Err(error) => return error_flow(runtime, "E474", error.to_string()),
        }
    };
    // `<script>` is upstream's `REMAP_SCRIPT`: only `<SID>` mappings may be
    // used in the right-hand side. There are no script-local mappings in this
    // port, so the reachable set is empty and no-remap is the same behavior —
    // but the flag is recorded, because `maparg()`'s `script` key is the only
    // thing that distinguishes `<script>` from `:noremap`.
    let remap = !name.contains("nore") && !flags.contains(MapModifiers::SCRIPT);
    // `map_add` receives sid 0 and lnum 0 and copies `current_sctx`, adding
    // `SOURCING_LNUM` to its line (`mapping.c:501-505,530-537,890-894`).
    // Inside a function body that is the `:function`'s own line plus the body
    // line; at script level it is the physical line being executed.
    let script_context = runtime.scripts.current_context();
    let mut options = MappingOptions {
        modes,
        scope: map_scope,
        description: None,
        orig_rhs: rhs.to_owned(),
        script_context,
        ..MappingOptions::default()
    };
    options
        .flags
        .set(MapFlags::NOWAIT, flags.contains(MapModifiers::NOWAIT));
    options
        .flags
        .set(MapFlags::SILENT, flags.contains(MapModifiers::SILENT));
    options
        .flags
        .set(MapFlags::SCRIPT, flags.contains(MapModifiers::SCRIPT));
    let result = if remap {
        editor.mappings_mut().map(lhs, action, options)
    } else {
        editor.mappings_mut().noremap(lhs, action, options)
    };
    match result {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E474", error.to_string()),
    }
}

/// `do_map`'s listing passes and `showmap` (`mapping.c:698-793,211-275`).
///
/// `showmap` starts each row with a newline whenever the message column is
/// past zero or output is being captured, which is why a captured listing
/// begins with a blank line: `msg_start` supplies one and the first row
/// supplies another.
fn list_mappings(editor: &mut Editor, lhs: &Keys, modes: MapModes, buffer_only: bool) -> Flow {
    let buffer = editor.current_buffer();
    let rows: Vec<String> = editor
        .mappings()
        .matching(lhs.as_bytes(), modes, buffer)
        .into_iter()
        .filter(|(mapping, local)| {
            !buffer_only || (*local && matches!(mapping.options.scope, MapScope::Buffer(_)))
        })
        .map(|(mapping, local)| showmap_row(mapping, local))
        .collect();
    push_info_text_message(editor, String::new());
    if rows.is_empty() {
        // `msg`, not `emsg` (`mapping.c:879`): nothing to report is not an error.
        push_info_text_message(editor, "No mapping found".to_owned());
        return Flow::Normal;
    }
    for row in rows {
        push_info_text_message(editor, row);
    }
    Flow::Normal
}

/// Marker prefix of the pseudo funcref names wrapping Lua registry refs; a
/// call on such a name invokes the registry slot instead of Vimscript code.
const LUA_REF_FUNCTION_PREFIX: &str = "\u{1}oxvim_luaref:";

/// Wraps one Lua registry reference as a callable funcref name so a callback
/// returned by an API call (or `function()` on one) stays invocable.
fn lua_ref_function_name(reference: u64) -> OxStr {
    let name = format!("{LUA_REF_FUNCTION_PREFIX}{reference}");
    OxStr::from(name.as_bytes())
}
/// One `showmap` row (`mapping.c:220-266`): the mode characters padded to
/// three columns, the lhs padded to at least twelve with one trailing blank
/// guaranteed, the `*`/`&`/blank remapker, the `@`/blank buffer-local
///ker, then the right-hand side.
fn showmap_row(mapping: &Mapping, local: bool) -> String {
    let mut row = mapping.options.modes.to_chars();
    while row.len() < 3 {
        row.push(' ');
    }
    let shown = special_notation(mapping.lhs.as_bytes(), true, false);
    let mut width = shown.chars().count();
    row.push_str(&shown);
    loop {
        row.push(' ');
        width += 1;
        if width >= 12 {
            break;
        }
    }
    row.push(if mapping.options.flags.contains(MapFlags::SCRIPT) {
        '&'
    } else if mapping.options.flags.contains(MapFlags::REMAP) {
        ' '
    } else {
        '*'
    });
    row.push(if local { '@' } else { ' ' });
    match mapping.action {
        MappingAction::Callback(id) => {
            let _ = write!(row, "<Callback {id}>");
        }
        ref action => match action.replaced_keys().unwrap_or_default() {
            [] => row.push_str("<Nop>"),
            keys => row.push_str(&special_notation(keys, false, false)),
        },
    }
    if let Some(description) = &mapping.options.description {
        row.push_str("\n                 ");
        row.push_str(description);
    }
    row
}

/// Rebuilds or refreshes the `b:` mirror. Gated on buffer identity AND the
/// buffer's variable-map version: a `current_buffer()` switch invalidates the
/// cached map even when two buffers share a version. The fast path refreshes
/// only the live text counter in place — a read-side mirror is not a scope
/// mutation, so it sets no dirty flag.
fn sync_buffer_scope(editor: &Editor, scope: &mut Scope) -> Result<(), ExecError> {
    let Some(buffer) = editor.current_buffer() else {
        return Ok(());
    };
    let vars_version = editor
        .buffer_variables_version(buffer)
        .map_err(|error| ExecError::Editor(error.to_string()))?;
    let cache_expired = scope.synced.buffer_identity() != Some(buffer)
        || scope.synced.buffer_version() != vars_version;
    if cache_expired {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ExecError::Editor(error.to_string()))?;
        let mut pairs = dict_to_scope(state.variables());
        // `b:changedtick` is owned by the buffer, not the variable dict:
        // materialize the live counter so `b:` reads observe every mutation.
        pairs.retain(|(key, _)| key.as_bytes() != b"changedtick");
        pairs.push((
            OxStr::from("changedtick"),
            Typval::Number(i64::try_from(state.script_changedtick()).unwrap_or(i64::MAX)),
        ));
        scope.buffer = pairs;
        scope.synced.set_buffer_identity(Some(buffer));
        scope.synced.set_buffer_version(vars_version);
        scope.synced.clear_dirty(ScopeKind::Buffer);
        return Ok(());
    }
    // Fast path: same buffer, unchanged variable map. Only the live text
    // counter can have moved, so refresh it in place — no rebuild, no
    // `assign`, and no dirty mark.
    let state = editor
        .buffer(buffer)
        .map_err(|error| ExecError::Editor(error.to_string()))?;
    let new_tick = i64::try_from(state.script_changedtick()).unwrap_or(i64::MAX);
    match scope
        .buffer
        .iter_mut()
        .find(|(key, _)| key.as_bytes() == b"changedtick")
    {
        Some((_, Typval::Number(tick))) => *tick = new_tick,
        Some((_, slot)) => *slot = Typval::Number(new_tick),
        None => scope
            .buffer
            .push((OxStr::from("changedtick"), Typval::Number(new_tick))),
    }
    Ok(())
}

pub(crate) fn sync_editor_into_scope(editor: &Editor, scope: &mut Scope) -> Result<(), ExecError> {
    // Differential sync: a map whose stamp still matches the editor's
    // version is kept as-is, so shared Typval values — and the mutability
    // metadata attached to their dictionary entries — survive across
    // commands. `b:` is gated on the buffer's variable-map version plus its
    // identity: a `current_buffer()` switch invalidates the cached map even
    // when two buffers share a version. A rebuilt map mirrors the editor, so
    // its dirty flag is cleared; a scope-side write sets it again.
    let global_version = editor.gvars_version();
    if scope.synced.get(ScopeKind::Global) != global_version {
        scope.global = dict_to_scope(editor.gvars());
        scope.synced.set(ScopeKind::Global, global_version);
        scope.synced.clear_dirty(ScopeKind::Global);
    }
    sync_buffer_scope(editor, scope)?;
    if let Some(window) = editor.current_window() {
        let window_version = editor
            .window_variables_version(window)
            .map_err(|error| ExecError::Editor(error.to_string()))?;
        if scope.synced.get(ScopeKind::Window) != window_version {
            scope.window = dict_to_scope(
                editor
                    .window_variables(window)
                    .map_err(|error| ExecError::Editor(error.to_string()))?,
            );
            scope.synced.set(ScopeKind::Window, window_version);
            scope.synced.clear_dirty(ScopeKind::Window);
        }
    }
    if let Some(tab) = editor.current_tabpage() {
        let tab_version = editor
            .tabpage_variables_version(tab)
            .map_err(|error| ExecError::Editor(error.to_string()))?;
        if scope.synced.get(ScopeKind::Tab) != tab_version {
            scope.tab = dict_to_scope(
                editor
                    .tabpage_variables(tab)
                    .map_err(|error| ExecError::Editor(error.to_string()))?,
            );
            scope.synced.set(ScopeKind::Tab, tab_version);
            scope.synced.clear_dirty(ScopeKind::Tab);
        }
    }
    let vim_version = editor.vvars_version();
    if scope.synced.get(ScopeKind::Vim) != vim_version {
        scope.vim = dict_to_scope(editor.vvars());
        scope.synced.set(ScopeKind::Vim, vim_version);
        scope.synced.clear_dirty(ScopeKind::Vim);
    }
    // The `v:_null_blob` repair is the one post-read scope mutation: the
    // editor round-trips a Blob as an empty Array, so a rebuilt `v:` map
    // needs the real Blob put back and written out again. When the cached
    // map already carries it (no rebuild happened) the repair is a no-op and
    // `v:` stays clean, which keeps the steady-state sync write-free.
    if !matches!(
        scope.vim.iter().find(|(key, _)| key.as_bytes() == b"_null_blob").map(|(_, value)| value),
        Some(Typval::Blob(bytes)) if bytes.is_empty()
    ) {
        scope.replace_pair(ScopeKind::Vim, "_null_blob", Typval::Blob(Vec::new()));
    }
    scope.registers.clear();
    for name in "0123456789abcdefghijklmnopqrstuvwxyz\"-:.%#=*+_/@".chars() {
        if let Ok(Some(content)) = editor.registers().get(name) {
            scope.set_register(&[name as u8], Typval::String(OxStr(content.to_bytes())));
        }
    }
    refresh_special_registers(editor, scope);
    scope.options_global.clear();
    scope.options_local.clear();
    for metadata in OPTION_METADATA {
        let mut global = None;
        let mut local = None;
        if metadata.scopes.contains(&OptionScope::Global)
            && let Ok(value) = editor.options().get_global(metadata.name)
        {
            global = Some(option_to_typval(value));
        }
        if let (Some(buffer), true) = (
            editor.current_buffer(),
            metadata.scopes.contains(&OptionScope::Buffer),
        ) && let Ok(value) = editor.options().get_buffer(buffer, metadata.name)
        {
            local = Some(option_to_typval(value));
        }
        if let (Some(window), true) = (
            editor.current_window(),
            metadata.scopes.contains(&OptionScope::Window),
        ) && let Ok(value) = editor.options().get_window(window, metadata.name)
        {
            local = local.or_else(|| Some(option_to_typval(value)));
        }
        if let Some(value) = global.clone() {
            scope.set_option(EvalOptionScope::Global, metadata.name.as_bytes(), value);
        }
        if let Some(value) = local.clone() {
            scope.set_option(EvalOptionScope::Local, metadata.name.as_bytes(), value);
        }
        // `&opt` expressions keep the spelling as written, so every accepted
        // name (canonical, short, historical alias) must reach the value.
        for name in metadata
            .short_name
            .iter()
            .copied()
            .chain(metadata.aliases.iter().copied())
        {
            if let Some(value) = global.as_ref() {
                scope.set_option(EvalOptionScope::Global, name.as_bytes(), value.clone());
            }
            if let Some(value) = local.as_ref() {
                scope.set_option(EvalOptionScope::Local, name.as_bytes(), value.clone());
            }
        }
    }
    Ok(())
}

fn refresh_special_registers(editor: &Editor, scope: &mut Scope) {
    let current = editor
        .current_buffer()
        .and_then(|buffer| editor.buffer(buffer).ok())
        .map_or_else(|| OxStr::from(""), |state| state.name().clone());
    let alternate = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .and_then(|window| window.alternate_buffer)
        .and_then(|buffer| editor.buffer(buffer).ok())
        .map_or_else(|| OxStr::from(""), |state| state.name().clone());
    scope.set_register(b"%", Typval::String(current));
    scope.set_register(b"#", Typval::String(alternate));
}

fn refresh_local_options(editor: &Editor, scope: &mut Scope) {
    // Diff-update instead of clear+rebuild. `options_local` holds its
    // entries in `OPTION_METADATA` order: the first refresh pushes each
    // local-scoped name in loop order, and every later write (`assign`,
    // `set_and_mirror`, the `nvim_set_option_value` seam) finds existing
    // entries in place, so a cursor advances in lockstep with this loop.
    // That keeps the per-command cost O(n) name compares instead of the
    // rebuild's O(n²/2) `find_index` scans, and skips the `OxStr::from`
    // key clone and `option_to_typval` payload clone for every option
    // whose editor value did not change.
    let buffer = editor.current_buffer();
    let window = editor.current_window();
    let local = &mut scope.options_local;
    let mut cursor = 0usize;
    for metadata in OPTION_METADATA {
        let name_bytes = metadata.name.as_bytes();
        // The window pass ran after the buffer pass in the clear+rebuild
        // form and unconditionally overwrote it, so the window value wins
        // when both scopes apply and buffer is the fallback otherwise.
        let mut target: Option<&OptionValue> = None;
        if let Some(window) = window
            && metadata.scopes.contains(&OptionScope::Window)
            && let Ok(value) = editor.options().get_window(window, metadata.name)
        {
            target = Some(value);
        }
        if target.is_none()
            && let Some(buffer) = buffer
            && metadata.scopes.contains(&OptionScope::Buffer)
            && let Ok(value) = editor.options().get_buffer(buffer, metadata.name)
        {
            target = Some(value);
        }
        match target {
            Some(value) => {
                if cursor < local.len() && local[cursor].0.as_bytes() == name_bytes {
                    if !option_matches(value, &local[cursor].1) {
                        local[cursor].1 = option_to_typval(value);
                    }
                    cursor += 1;
                } else if let Some(index) = local
                    .iter()
                    .position(|(key, _)| key.as_bytes() == name_bytes)
                {
                    // Out of order (an entry first written before any refresh
                    // ran). Update in place, then heal it to the cursor so
                    // one misplaced entry cannot permanently derail the
                    // lockstep walk.
                    if !option_matches(value, &local[index].1) {
                        local[index].1 = option_to_typval(value);
                    }
                    let entry = local.remove(index);
                    local.insert(cursor, entry);
                    cursor += 1;
                } else {
                    local.insert(cursor, (OxStr::from(name_bytes), option_to_typval(value)));
                    cursor += 1;
                }
            }
            None if metadata.scopes.contains(&OptionScope::Buffer)
                || metadata.scopes.contains(&OptionScope::Window) =>
            {
                // No value to mirror (no current buffer/window): the
                // clear+rebuild form would absence the entry, so remove it.
                // Global-only names are guarded out above by `target` and
                // were never added to the map, so only local-scoped names
                // reach this branch.
                if cursor < local.len() && local[cursor].0.as_bytes() == name_bytes {
                    local.remove(cursor);
                } else if let Some(index) = local
                    .iter()
                    .position(|(key, _)| key.as_bytes() == name_bytes)
                {
                    local.remove(index);
                }
            }
            None => {}
        }
    }
}

/// Whether `existing` already equals what [`option_to_typval`] would build
/// for `value`, compared without allocating: Boolean mirrors as Number,
/// and strings compare as raw bytes.
fn option_matches(value: &OptionValue, existing: &Typval) -> bool {
    match (value, existing) {
        (OptionValue::Boolean(b), Typval::Number(n)) => *n == i64::from(*b),
        (OptionValue::Number(n), Typval::Number(m)) => n == m,
        (OptionValue::String(s), Typval::String(stored)) => {
            s.as_str().as_bytes() == stored.as_bytes()
        }
        _ => false,
    }
}

/// Writes scope-side variable changes back into the editor.
///
/// Each kind is gated on its dirty flag: a map the script never wrote is
/// not converted, so the steady-state host call performs no `scope_to_dict`
/// and no `assign`. A flag can only be set by a scope-side mutation that
/// happened after the last read sync, so skipping a clean map cannot drop a
/// write.
pub(crate) fn sync_scope_into_editor(editor: &mut Editor, scope: &Scope) -> Result<(), ExecError> {
    if scope.synced.is_dirty(ScopeKind::Global) {
        *editor.gvars_mut() = scope_to_dict(&scope.global);
        scope.synced.set(ScopeKind::Global, editor.gvars_version());
        scope.synced.clear_dirty(ScopeKind::Global);
    }
    // The cached `b:` map belongs to the buffer the read sync mirrored. When
    // the current buffer has since moved (a Lua callback switched buffers
    // mid-command) writing it would land in the wrong buffer, so the write is
    // skipped and the flag stays set: the next read sync sees the identity
    // change, rebuilds `b:` from the live buffer, and clears it.
    if scope.synced.is_dirty(ScopeKind::Buffer)
        && let Some(buffer) = editor
            .current_buffer()
            .filter(|buffer| scope.synced.buffer_identity() == Some(*buffer))
    {
        let mut variables = scope_to_dict(&scope.buffer);
        // The materialized `b:changedtick` never lands in ordinary
        // variables: the buffer owns the live counter and a stored copy
        // would go stale.
        variables
            .0
            .retain(|(key, _)| key.as_bytes() != b"changedtick");
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ExecError::Editor(error.to_string()))?;
        *state.variables_mut() = variables;
        scope.synced.set_buffer_version(state.variables_version());
        scope.synced.clear_dirty(ScopeKind::Buffer);
    }
    // Persist `:lockvar` marks for buffer-scoped variables into editor-owned
    // storage so the API (`nvim_buf_set_var` / `nvim_buf_del_var`) can reject
    // mutations with "Key is locked".  This is hoisted out of the dirty
    // branch because `:lockvar` modifies `Scope::locked` without marking the
    // buffer variable map dirty — the lock state must propagate even when
    // the variable dict itself is unchanged.
    if let Some(buffer) = editor
        .current_buffer()
        .filter(|buffer| scope.synced.buffer_identity() == Some(*buffer))
    {
        let locked_names: Vec<OxStr> = scope
            .locked
            .iter()
            .filter(|mark| mark.scope == ScopeKind::Buffer)
            .map(|mark| mark.name.clone())
            .collect();
        if let Ok(state) = editor.buffer_mut(buffer) {
            state.set_locked_vars(locked_names);
        }
    }
    if scope.synced.is_dirty(ScopeKind::Window)
        && let Some(window) = editor.current_window()
    {
        *editor
            .window_variables_mut(window)
            .map_err(|error| ExecError::Editor(error.to_string()))? = scope_to_dict(&scope.window);
        scope.synced.set(
            ScopeKind::Window,
            editor
                .window_variables_version(window)
                .map_err(|error| ExecError::Editor(error.to_string()))?,
        );
        scope.synced.clear_dirty(ScopeKind::Window);
    }
    if scope.synced.is_dirty(ScopeKind::Tab)
        && let Some(tab) = editor.current_tabpage()
    {
        *editor
            .tabpage_variables_mut(tab)
            .map_err(|error| ExecError::Editor(error.to_string()))? = scope_to_dict(&scope.tab);
        scope.synced.set(
            ScopeKind::Tab,
            editor
                .tabpage_variables_version(tab)
                .map_err(|error| ExecError::Editor(error.to_string()))?,
        );
        scope.synced.clear_dirty(ScopeKind::Tab);
    }
    if scope.synced.is_dirty(ScopeKind::Vim) {
        *editor.vvars_mut() = scope_to_dict(&scope.vim);
        scope.synced.set(ScopeKind::Vim, editor.vvars_version());
        scope.synced.clear_dirty(ScopeKind::Vim);
    }
    Ok(())
}

#[expect(
    clippy::only_used_in_recursion,
    reason = "list-unpack forwards the lock flag to nested targets unchanged"
)]
fn assign_target<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    target: &str,
    value: Typval,
    constant: bool,
) -> Result<(), Flow> {
    let target = target.trim();
    if let Some(inner) = target
        .strip_prefix('[')
        .and_then(|target| target.strip_suffix(']'))
    {
        let targets = split_comma_args(inner);
        let Typval::List(values) = value else {
            return Err(error_flow(runtime, "E714", "List required"));
        };
        let values = values.borrow().items.clone();
        if targets.len() < values.len() {
            return Err(error_flow(runtime, "E687", "Less targets than List items"));
        }
        if targets.len() > values.len() {
            return Err(error_flow(runtime, "E688", "More targets than List items"));
        }
        for (target, value) in targets.into_iter().zip(values) {
            assign_target(runtime, access, scope, target, value, constant)?;
        }
        return Ok(());
    }
    if let Some(register) = target
        .strip_prefix('@')
        .and_then(|name| name.chars().next())
    {
        let content = RegisterContent::from_text(typval_to_text(&value).as_bytes())
            .map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
        access
            .with_ex_editor(|editor| editor.registers_mut().set(register, content))
            .map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
        scope.set_register(&[register as u8], value);
        return Ok(());
    }
    if let Some(environment) = target.strip_prefix('$') {
        // `ex_let_env` (`eval/vars.c`:1349-1351) hands the value straight to
        // `vim_setenv_ext`, which is `os_setenv`: the assignment *is* a change
        // to the process environment, so every child inherits it and every
        // `$VAR` reader observes it.
        let text = typval_to_text(&value);
        ox_sys::set_env(environment, &text);
        return Ok(());
    }
    if let Some(option) = target.strip_prefix('&') {
        return assign_option(runtime, access, scope, option, &value);
    }
    let (kind, name) = resolve_scope_name(target);
    if let Some(kind) = kind
        && let Some((flags, _)) = scoped_target_flags(kind, name.as_bytes())
        && flags.intersects(DictEntryFlags::READ_ONLY)
    {
        // `set_var_const` (vars.c:2873): read-only is checked before both
        // lock checks, and the message names the target verbatim — plain,
        // `b:["changedtick"]`, or `b:.changedtick` alike.
        return Err(error_flow(
            runtime,
            "E46",
            format!("Cannot change read-only variable \"{target}\""),
        ));
    }
    if kind == Some(ScopeKind::Vim) {
        if !vim_variable_is_writable(name.as_bytes()) {
            // `var_check_ro` (`eval/vars.c:2965-2971`) runs before
            // `before_set_vvar`; the message keeps the name as written.
            return Err(error_flow(
                runtime,
                "E46",
                format!("Cannot change read-only variable \"{target}\""),
            ));
        }
        if vim_variable_type(name.as_bytes()).is_some() {
            // `before_set_vvar` (`eval/vars.c:2760-2810`): a typed `v:`
            // variable keeps its type — Strings and Numbers coerce the
            // value, anything else must match or refuse with E963.
            assign_vim_variable(runtime, scope, name.as_bytes(), value)?;
        } else {
            // A writable name outside the upstream type table (`v:testing`)
            // has no startup type to preserve; the null-blob contract keeps
            // creating it writable.
            scope.replace_pair(ScopeKind::Vim, &name, value);
        }
        return Ok(());
    }
    if let Some(kind) = kind {
        scope
            .set_scoped(kind, name.as_bytes(), 0, value)
            .map_err(|error| eval_error_flow(runtime, error))?;
    } else if runtime.can_add_defer() {
        // `get_var_ht_dict` (eval/vars.c:2505-2512): inside a function a
        // bare name is function-local; outside one it is global.
        scope
            .set(name.as_bytes(), value)
            .map_err(|error| eval_error_flow(runtime, error))?;
    } else {
        scope
            .set_scoped(ScopeKind::Global, name.as_bytes(), 0, value)
            .map_err(|error| eval_error_flow(runtime, error))?;
    }
    Ok(())
}

/// `find_var_ht_dict` (`eval/vars.c:2528-2534`): "version" is "v:version"
/// in every scope — the compat lookup precedes the local scopes, so a bare
/// `version` reads and refuses like the `v:` entry in this layer too.
#[must_use]
fn resolve_scope_name(target: &str) -> (Option<ScopeKind>, String) {
    let (kind, name) = parse_scope_name(target);
    if kind.is_none() && name == "version" {
        return (Some(ScopeKind::Vim), name);
    }
    (kind, name)
}

/// Whether upstream's `vimvars` table leaves a `v:` item writable (flags 0).
#[must_use]
pub fn vim_variable_is_writable(name: &[u8]) -> bool {
    matches!(
        name,
        b"errmsg"
            | b"warningmsg"
            | b"statusmsg"
            | b"this_session"
            | b"fcs_choice"
            | b"scrollstart"
            | b"swapchoice"
            | b"char"
            | b"mouse_win"
            | b"mouse_winid"
            | b"mouse_lnum"
            | b"mouse_col"
            | b"searchforward"
            | b"hlsearch"
            | b"oldfiles"
            | b"completed_item"
            | b"errors"
            | b"testing"
    )
}

/// The mutability metadata a scope-qualified target's dict item carries
/// (`scope_entry_flags`), plus whether the target reaches that item through
/// a subscript (`b:.k` or `b:["k"]`) instead of naming it plain. Upstream's
/// equivalent is the split between `do_unlet`/`do_lock_var` on a plain name
/// and `get_lval` dict-item resolution for subscripts.
#[must_use]
fn scoped_target_flags(kind: ScopeKind, name: &[u8]) -> Option<(DictEntryFlags, bool)> {
    let (key, subscripted) = if let Some(rest) = name.strip_prefix(b".") {
        (rest, true)
    } else if (name.starts_with(b"[\"") && name.ends_with(b"\"]"))
        || (name.starts_with(b"['") && name.ends_with(b"']"))
    {
        (&name[2..name.len() - 2], true)
    } else {
        (name, false)
    };
    ox_eval::scope::scope_entry_flags(kind, key).map(|(flags, _)| (flags, subscripted))
}

fn read_target<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &Scope,
    target: &str,
) -> Result<Typval, Flow> {
    let target = target.trim();
    if let Some(register) = target
        .strip_prefix('@')
        .and_then(|name| name.chars().next())
    {
        return Ok(scope.get_register(&[register as u8]));
    }
    if let Some(environment) = target.strip_prefix('$') {
        // `vim_getenv` reads the live process environment, so a variable set
        // this session -- `setenv()`, `let $VAR`, a locale change -- is
        // visible here, and an unset one is the empty string, as upstream's
        // expression evaluation gives.
        return Ok(Typval::String(std::env::var_os(environment).map_or_else(
            || OxStr::from(""),
            |value| OxStr::from(value.to_string_lossy().as_ref()),
        )));
    }
    if let Some(option) = target.strip_prefix('&') {
        return Ok(access.with_ex_editor(|editor| read_option(editor, option)));
    }
    let (kind, name) = resolve_scope_name(target);
    let value = if let Some(kind) = kind {
        scope.get_scoped(kind, name.as_bytes(), 0)
    } else {
        scope.get(name.as_bytes(), 0)
    };
    value
        .cloned()
        .map_err(|error| eval_error_flow(runtime, error))
}

/// Length of the leading environment-variable name in `text`, upstream
/// `get_env_len` (`eval.c` 5569-5575) scanning `vim_isIDc` bytes. The default
/// `'isident'` accepts ASCII letters, digits and `_`; the 192-255 range it also
/// lists cannot be expressed byte-wise over a UTF-8 `str`, so this is that
/// default's ASCII subset.
fn env_name_len(text: &str) -> usize {
    text.bytes()
        .position(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
        .unwrap_or(text.len())
}

fn remove_target(scope: &mut Scope, target: &str) -> bool {
    if let Some(environment) = target.strip_prefix('$') {
        // `do_unlet_var` (`eval/vars.c`:1653-1654) is `vim_unsetenv_ext`, the
        // process-wide unset, for the same reason the assignment is a
        // process-wide set. It reports no failure upstream: a name that was
        // never set is still "removed". `@reg`/`&opt` targets never reach
        // here: `unlet_name_garbage` refuses them with E488 first, the same
        // way upstream `get_lval` stops at the non-name start byte.
        ox_sys::unset_env(environment);
        return true;
    }
    let (kind, name) = parse_scope_name(target);
    match kind {
        Some(ScopeKind::Global) => scope.remove_pair(ScopeKind::Global, name.as_bytes()),
        Some(ScopeKind::Buffer) => scope.remove_pair(ScopeKind::Buffer, name.as_bytes()),
        Some(ScopeKind::Window) => scope.remove_pair(ScopeKind::Window, name.as_bytes()),
        Some(ScopeKind::Tab) => scope.remove_pair(ScopeKind::Tab, name.as_bytes()),
        Some(ScopeKind::Script) => scope.remove_pair(ScopeKind::Script, name.as_bytes()),
        Some(ScopeKind::Local) => scope.remove_pair(ScopeKind::Local, name.as_bytes()),
        Some(ScopeKind::Argument | ScopeKind::Vim) => false,
        None => {
            scope.remove_pair(ScopeKind::Local, name.as_bytes())
                || scope.remove_pair(ScopeKind::Global, name.as_bytes())
        }
    }
}

fn eval_option_scope(name: &str, layer: SetLayer) -> EvalOptionScope {
    let has_local_scope = crate::option_metadata(name).is_some_and(|metadata| {
        metadata
            .scopes
            .iter()
            .any(|scope| matches!(scope, OptionScope::Buffer | OptionScope::Window))
    });
    if matches!(layer, SetLayer::Global) || !has_local_scope {
        EvalOptionScope::Global
    } else {
        EvalOptionScope::Local
    }
}

pub(crate) fn assign_option<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    option: &str,
    value: &Typval,
) -> Result<(), Flow> {
    let (prefix, name) = if let Some(name) = option.strip_prefix("g:") {
        (SetLayer::Global, name)
    } else if let Some(name) = option.strip_prefix("l:") {
        (SetLayer::Local, name)
    } else {
        (SetLayer::Effective, option)
    };
    let metadata = crate::option_metadata(name)
        .ok_or_else(|| error_flow(runtime, "E355", format!("Unknown option: {name}")))?;
    let converted = typval_to_option(value, metadata.value_type)
        .map_err(|message| error_flow(runtime, "E474", message))?;
    access
        .with_ex_editor(|editor| set_option_value(editor, metadata.name, converted, prefix))
        .map_err(|(code, message)| error_flow(runtime, code, message))?;
    // `&opt` reads keep the spelling as written; without the alias mirror a
    // `let &ts=8` inside one executed unit still reads `&ts` stale until the
    // next editor→scope sync.
    let eval_scope = eval_option_scope(metadata.name, prefix);
    let accepted = std::iter::once(metadata.name)
        .chain(metadata.short_name.iter().copied())
        .chain(metadata.aliases.iter().copied());
    for name in accepted {
        scope.set_option(eval_scope, name.as_bytes(), value.clone());
    }
    if metadata.name == "runtimepath" {
        access.with_ex_editor(|editor| sync_runtime_roots(runtime, editor));
    }
    Ok(())
}

pub(crate) fn read_option(editor: &Editor, option: &str) -> Typval {
    let (layer, name) = if let Some(name) = option.strip_prefix("g:") {
        (SetLayer::Global, name)
    } else if let Some(name) = option.strip_prefix("l:") {
        (SetLayer::Local, name)
    } else {
        (SetLayer::Effective, option)
    };
    option_value(editor, name, layer).map_or(Typval::Number(0), option_to_typval)
}

/// Trim the white space `skipwhite` trims, and nothing else.
///
/// `str::trim` removes CR, VT, FF, NL and every Unicode space. Upstream's
/// `skipwhite` and `del_trailing_spaces` (`strings.c:429-436`,
/// `ascii_defs.h:84-87`) remove ASCII space and tab only, so everything else
/// stays part of the argument -- and for an expression argument that means
/// `eval0` sees it and answers `E488: Trailing characters` (`eval.c:1251`).
fn skipwhite_trim(text: &str) -> &str {
    text.trim_matches([' ', '\t'])
}

/// The end of one `:unlet` target name, and whatever garbage follows it.
///
/// `ex_unletlock` (`eval/vars.c:1600-1617`) takes the name `get_lval`
/// accepted and requires the next byte to be white space or `ends_excmd` --
/// NUL, `|`, `"` or newline. Anything else is trailing garbage and raises
/// E488 with the remainder. `eval_isnamec` is alphanumeric, `_`, `:` and `#`;
/// `find_name_end` walks past a `[...]` index and a `.` key on top of that.
/// The `$ENV` form never reaches here: `ex_unletlock` measures that one with
/// `get_env_len` first.
fn unlet_name_garbage(target: &str) -> Option<&str> {
    let bytes = target.as_bytes();
    let mut index = 0usize;
    let mut depth = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if depth > 0 {
            match byte {
                b'[' => depth += 1,
                b']' => depth -= 1,
                _ => {}
            }
            index += 1;
            continue;
        }
        if byte == b'[' {
            depth += 1;
            index += 1;
            continue;
        }
        let name_char = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'#');
        if !name_char && (byte != b'.' || index + 1 >= bytes.len()) {
            break;
        }
        index += 1;
    }
    let rest = &target[index..];
    if rest.is_empty() || rest.starts_with(['|', '"', '\n']) {
        None
    } else {
        Some(rest)
    }
}

fn strip_expression_comment(expression: &str) -> &str {
    let bytes = expression.as_bytes();
    let mut quote = None;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if let Some(active) = quote {
            if byte == active && (index == 0 || bytes[index - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        if byte == b'\'' {
            quote = Some(byte);
            continue;
        }
        if byte != b'"' {
            continue;
        }
        let previous = bytes[..index]
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace());
        if index > 0
            && bytes[index - 1].is_ascii_whitespace()
            && previous.is_some_and(|previous| {
                bytes[previous].is_ascii_alphanumeric()
                    || matches!(bytes[previous], b'\'' | b'"' | b']' | b')' | b'}')
            })
        {
            return expression[..index].trim_end_matches([' ', '\t']);
        }
        quote = Some(byte);
    }
    expression
}

pub(crate) fn split_assignment(args: &str) -> Option<(&str, &str, &str)> {
    let bytes = args.as_bytes();
    let mut quote = None;
    let mut depth = 0usize;
    for index in 0..bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if byte == active && (index == 0 || bytes[index - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"') && !matches!(bytes.get(index.wrapping_sub(1)), Some(b'@')) {
            quote = Some(byte);
            continue;
        }
        if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
            continue;
        }
        if matches!(byte, b')' | b']' | b'}') {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 0 && byte == b'=' && index > 0 && bytes[index - 1] == b'@' {
            // `@=` is the expression register name, not an assignment
            // operator. Skip this `=` so the real `=` later is found.
            continue;
        }
        if depth == 0 && byte == b'=' {
            // eval.c ex_let: the compound operators are `+=`, `-=`, `.=`, and
            // `..=` (string concat). `..=` must claim both dots, otherwise the
            // stray first dot stays in the target name.
            let start = if index >= 2 && bytes[index - 1] == b'.' && bytes[index - 2] == b'.' {
                index - 2
            } else if index > 0 && matches!(bytes[index - 1], b'+' | b'-' | b'.') {
                index - 1
            } else {
                index
            };
            return Some((
                skipwhite_trim(&args[..start]),
                skipwhite_trim(&args[start..=index]),
                skipwhite_trim(&args[index + 1..]),
            ));
        }
    }
    None
}

fn split_for(args: &str) -> Option<(&str, &str)> {
    args.split_once(" in ")
        .map(|(target, expression)| (skipwhite_trim(target), skipwhite_trim(expression)))
}

fn parse_scope_name(target: &str) -> (Option<ScopeKind>, String) {
    let bytes = target.as_bytes();
    if bytes.len() > 2
        && bytes[1] == b':'
        && let Some(kind) = ScopeKind::from_byte(bytes[0])
    {
        return (Some(kind), target[2..].to_owned());
    }
    (None, target.to_owned())
}

fn canonical_target(target: &str) -> String {
    target.trim().to_owned()
}

fn apply_assignment_operator<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    left: Typval,
    right: &Typval,
    operator: &str,
) -> Result<Typval, Flow> {
    if operator == "+="
        && let (Typval::List(left_items), Typval::List(right_items)) = (&left, right)
    {
        let appended = right_items.borrow().items.clone();
        left_items.borrow_mut().items.extend(appended);
        return Ok(left);
    }
    if matches!(operator, "+=" | "-=")
        && (matches!(left, Typval::Float(_)) || matches!(right, Typval::Float(_)))
    {
        let Some(left) = assignment_float(&left) else {
            return Err(error_flow(
                runtime,
                "E734",
                format!("Wrong variable type for {operator}"),
            ));
        };
        let Some(right) = assignment_float(right) else {
            return Err(error_flow(
                runtime,
                "E734",
                format!("Wrong variable type for {operator}"),
            ));
        };
        return Ok(Typval::Float(if operator == "+=" {
            left + right
        } else {
            left - right
        }));
    }
    match operator {
        "+=" => Ok(Typval::Number(
            typval_number(&left)
                .unwrap_or(0)
                .saturating_add(typval_number(right).unwrap_or(0)),
        )),
        "-=" => Ok(Typval::Number(
            typval_number(&left)
                .unwrap_or(0)
                .saturating_sub(typval_number(right).unwrap_or(0)),
        )),
        ".=" | "..=" => Ok(Typval::String(OxStr(
            format!("{}{}", typval_to_text(&left), typval_to_text(right)).into_bytes(),
        ))),
        _ => Err(error_flow(
            runtime,
            "E734",
            format!("Wrong variable type for {operator}"),
        )),
    }
}

/// Compound assignment on an option reference, per eval/vars.c
/// `ex_let_option`: `.`/`..` operators concatenate and are rejected on
/// number and boolean options, while `+`/`-` do arithmetic and are
/// rejected on string options — both rejections raise E734.
fn apply_option_assignment_operator<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    current: Typval,
    operand: &Typval,
    operator: &str,
) -> Result<Typval, Flow> {
    let concatenate = operator.starts_with('.');
    match (current, concatenate) {
        (Typval::String(current), true) => Ok(Typval::String(OxStr(
            format!("{}{}", current.to_string_lossy(), typval_to_text(operand)).into_bytes(),
        ))),
        (Typval::Number(current), false) => match operator {
            "+=" => Ok(Typval::Number(
                current.saturating_add(typval_number(operand).unwrap_or(0)),
            )),
            "-=" => Ok(Typval::Number(
                current.saturating_sub(typval_number(operand).unwrap_or(0)),
            )),
            _ => Err(error_flow(
                runtime,
                "E734",
                format!("Wrong variable type for {operator}"),
            )),
        },
        _ => Err(error_flow(
            runtime,
            "E734",
            format!("Wrong variable type for {operator}"),
        )),
    }
}

fn iterable_values(value: Typval) -> Result<Vec<Typval>, (&'static str, &'static str)> {
    match value {
        Typval::List(list) => list
            .try_borrow()
            .map(|data| data.items.clone())
            .map_err(|_| ("E714", "List is locked")),
        Typval::Blob(bytes) => Ok(bytes
            .into_iter()
            .map(|byte| Typval::Number(i64::from(byte)))
            .collect()),
        Typval::String(text) => {
            // `next_for_item` (eval.c:1562-1575) hands out one UTF-8
            // character per iteration. `str::from_utf8` validates the
            // scalar width: a complete prefix is drained scalar by scalar,
            // and a malformed or truncated leading sequence contributes a
            // single byte instead of swallowing arbitrary continuation
            // runs.
            let bytes = text.as_bytes();
            let mut items = Vec::new();
            let mut index = 0;
            while index < bytes.len() {
                match std::str::from_utf8(&bytes[index..]) {
                    Ok(valid) => {
                        for character in valid.chars() {
                            let mut encoded = [0; 4];
                            items.push(Typval::String(OxStr::from(
                                character.encode_utf8(&mut encoded).as_bytes(),
                            )));
                        }
                        index = bytes.len();
                    }
                    Err(error) if error.valid_up_to() > 0 => {
                        let valid = std::str::from_utf8(&bytes[index..index + error.valid_up_to()])
                            .unwrap_or_default();
                        for character in valid.chars() {
                            let mut encoded = [0; 4];
                            items.push(Typval::String(OxStr::from(
                                character.encode_utf8(&mut encoded).as_bytes(),
                            )));
                        }
                        index += error.valid_up_to();
                    }
                    Err(_) => {
                        items.push(Typval::String(OxStr(bytes[index..=index].to_vec())));
                        index += 1;
                    }
                }
            }
            Ok(items)
        }
        _ => Err(("E1098", "String, List or Blob required")),
    }
}

fn resolve_range(editor: &Editor, command: &ExCommand) -> Result<(usize, usize), String> {
    let (_, last) = address_domain_bounds(editor, effective_addr_type(&command.command));
    let (start, end) = resolve_range_raw(editor, command)?;
    Ok((start.max(1), end.min(last)))
}

/// Resolves a command's addresses without `resolve_range`'s lower clamp, so a
/// ZEROR command (`:0read`, `:0put`) can address line 0 — upstream's "before
/// the first line" position (`ex_docmd.c` `EX_ZEROR`).
fn resolve_range_raw(editor: &Editor, command: &ExCommand) -> Result<(usize, usize), String> {
    let (current, last) = address_domain_bounds(editor, effective_addr_type(&command.command));
    let Some(range) = &command.range else {
        // EX_DFLALL: no address means the whole buffer for these commands,
        // not the cursor line (ex_docmd.c:2100-2107 "default is 1,$").
        if effective_flags(&command.command).contains(CommandFlags::DFLALL) {
            return Ok((1, last));
        }
        return Ok((current, current));
    };
    let start = range.start.as_ref().map_or(Ok(current), |address| {
        resolve_address(editor, address, current, last)
    })?;
    let end = range.end.as_ref().map_or(Ok(start), |address| {
        resolve_address(editor, address, current, last)
    })?;
    if start > end {
        return Err("Invalid range".to_owned());
    }
    Ok((start, end))
}

/// The `.` and `$` values for one address domain.
///
/// `get_address` resolves both per `addr_type` (`ex_docmd.c:3435-3470` for
/// `$`, and the `.` case just above it), so `:$tabnew` means the last tabpage
/// and not the last buffer line. `invalid_range` bounds the same domains
/// against the same upper value, so both callers read it from here.
///
/// `LoadedBuffers`, `QuickFix` and `QuickFixValid` fall back to lines: this
/// port has neither a number-ordered buffer load state nor a quickfix list.
fn address_domain_bounds(editor: &Editor, addr_type: AddrType) -> (usize, usize) {
    match addr_type {
        AddrType::Windows => {
            let windows = editor
                .current_tabpage()
                .and_then(|tab| editor.tabpage_windows(tab).ok())
                .unwrap_or_default();
            let current = editor
                .current_window()
                .and_then(|window| windows.iter().position(|entry| *entry == window))
                .map_or(1, |index| index + 1);
            (current, windows.len())
        }
        AddrType::Tabs => {
            let current = editor
                .current_tabpage()
                .and_then(|tab| editor.tabpage_index(tab))
                .unwrap_or(1);
            (current, editor.tabpages().len())
        }
        AddrType::Arguments => {
            let arglist = editor.arglist();
            // "add 1 if ARGCOUNT is 0", so ":0argdelete" on an empty list is
            // not an error (ex_docmd.c:3751-3756).
            let count = arglist.len();
            (
                arglist.index().saturating_add(1),
                if count == 0 { 1 } else { count },
            )
        }
        AddrType::Buffers => {
            let current = editor
                .current_buffer()
                .map_or(1, |buffer| usize::try_from(i64::from(buffer)).unwrap_or(1));
            let last = editor
                .buffers()
                .into_iter()
                .map(i64::from)
                .max()
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0);
            (current, last)
        }
        AddrType::Lines
        | AddrType::Other
        | AddrType::Unsigned
        | AddrType::None
        | AddrType::TabsRelative
        | AddrType::LoadedBuffers
        | AddrType::QuickFix
        | AddrType::QuickFixValid => {
            let current = editor
                .current_window()
                .and_then(|window| editor.window(window).ok())
                .map_or(1, |window| window.cursor.lnum);
            (current, buffer_last_line(editor))
        }
    }
}

/// Bounds a resolved range against its address domain, upstream's
/// `invalid_range` (`ex_docmd.c:3735-3820`).
///
/// Upstream rejects an out-of-domain address instead of clamping it, and does
/// so before a post-command count is folded in — `set_cmd_count`
/// (`ex_docmd.c:1372-1393`) clamps a count silently, "be vi compatible: no
/// error message for out of range". So `:99read f` in a three-line buffer is
/// `E16` while `:1print 99` still prints to the end.
///
/// `Other`, `TabsRelative` and `Unsigned` accept any range upstream.
/// `LoadedBuffers`, `QuickFix` and `QuickFixValid` are unchecked here: this
/// port has neither a buffer load state ordered by number nor a quickfix
/// list, so there is no limit to compare against.
fn check_address_domain(editor: &Editor, command: &ExCommand) -> Result<(), String> {
    if !effective_flags(&command.command).contains(CommandFlags::RANGE) {
        return Ok(());
    }
    if command.range.is_none() {
        return Ok(());
    }
    // A `-count` user command folds the leading number into `<count>`
    // (`ex_docmd.c:1372-1393` clamps silently), so it is never a domain
    // error even when it exceeds the buffer.
    if let ResolvedCommand::User(info) = &command.command
        && info.flags.contains(CommandFlags::COUNT)
    {
        return Ok(());
    }
    let addr_type = effective_addr_type(&command.command);
    if matches!(
        addr_type,
        AddrType::Other | AddrType::TabsRelative | AddrType::Unsigned | AddrType::None
    ) {
        return Ok(());
    }
    // Search addresses are not numeric bounds; a miss is E486 from
    // resolve_range, not E16 from this check.
    if command.range.as_ref().is_some_and(|range| {
        [&range.start, &range.end]
            .into_iter()
            .flatten()
            .any(|address| {
                matches!(
                    address.base,
                    AddressBase::ForwardSearch(_) | AddressBase::BackwardSearch(_)
                )
            })
    }) {
        return Ok(());
    }
    let (start, end) = resolve_range_raw(editor, command)?;
    if matches!(addr_type, AddrType::Buffers) && start < 1 {
        return Err("Invalid range".to_owned());
    }
    let (_, limit) = address_domain_bounds(editor, addr_type);
    if end > limit {
        return Err("Invalid range".to_owned());
    }
    Ok(())
}

fn buffer_last_line(editor: &Editor) -> usize {
    editor
        .current_buffer()
        .and_then(|buffer| editor.buffer(buffer).ok())
        .and_then(|state| state.text().ok())
        .map_or(1, Buffer::line_count)
}

fn resolve_address(
    editor: &Editor,
    address: &Address,
    current: usize,
    last: usize,
) -> Result<usize, String> {
    let mut value = match &address.base {
        AddressBase::Current => current,
        AddressBase::Last => last,
        AddressBase::Line(line) => {
            usize::try_from(*line).map_err(|_| "Invalid range".to_owned())?
        }
        AddressBase::Mark(name) => {
            let buffer = editor
                .current_buffer()
                .ok_or_else(|| "Mark not set".to_owned())?;
            editor
                .local_mark(buffer, *name)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Mark not set".to_owned())?
                .lnum
        }
        AddressBase::ForwardSearch(pattern) => search_address(editor, pattern, true, current)?.lnum,
        AddressBase::BackwardSearch(pattern) => {
            search_address(editor, pattern, false, current)?.lnum
        }
    };
    for offset in &address.offsets {
        value = if *offset >= 0 {
            let magnitude = usize::try_from(*offset).map_err(|_| "Invalid range".to_owned())?;
            value.saturating_add(magnitude)
        } else {
            let magnitude =
                usize::try_from(offset.unsigned_abs()).map_err(|_| "Invalid range".to_owned())?;
            value.saturating_sub(magnitude)
        };
    }
    Ok(value)
}

/// `/pat/` and `?pat?` addresses search from the current cursor
/// (`ex_docmd.c` case `'/'`/`'?'`, via `do_search`). A miss is E486, not E16.
/// The match position is the cursor `do_search` would leave, including column.
fn search_address(
    editor: &Editor,
    pattern: &str,
    forward: bool,
    current: usize,
) -> Result<Position, String> {
    let buffer = editor
        .current_buffer()
        .ok_or_else(|| "E749: Empty buffer".to_owned())?;
    let lines = buffer_lines(editor, buffer)?;
    let wrapscan = matches!(
        editor.options().get_global("wrapscan"),
        Ok(OptionValue::Boolean(true))
    );
    let cursor = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .map_or(
            Position {
                lnum: current,
                col: 0,
            },
            |window| window.cursor,
        );
    let direction = if forward {
        SearchDirection::Forward
    } else {
        SearchDirection::Backward
    };
    let mut state = SearchState::default();
    match state.search(&lines, cursor, pattern, direction, 1, wrapscan) {
        Ok(result) => Ok(result.target),
        Err(SearchError::PatternNotFound(pattern)) => {
            Err(format!("E486: Pattern not found: {pattern}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

fn current_line_pair(editor: &Editor) -> (usize, usize) {
    let current = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .map_or(1, |window| window.cursor.lnum);
    (current, current)
}

struct IfBlock {
    branches: Vec<IfBranch>,
    end: usize,
}
struct IfBranch {
    condition: Option<String>,
    start: usize,
    end: usize,
}

fn find_if(program: &[Instruction], open: usize, limit: usize) -> Option<IfBlock> {
    let mut depth = 0usize;
    let mut markers = Vec::new();
    let mut index = open + 1;
    while index < limit {
        match program[index].name() {
            "if" => depth += 1,
            "endif" if depth == 0 => {
                let mut branches = Vec::new();
                let mut condition =
                    Some(skipwhite_trim(&program[open].command.as_ref()?.args).to_owned());
                let mut start = open + 1;
                for marker in markers {
                    branches.push(IfBranch {
                        condition,
                        start,
                        end: marker,
                    });
                    condition = match program[marker].name() {
                        "elseif" => {
                            Some(skipwhite_trim(&program[marker].command.as_ref()?.args).to_owned())
                        }
                        _ => None,
                    };
                    start = marker + 1;
                }
                branches.push(IfBranch {
                    condition,
                    start,
                    end: index,
                });
                return Some(IfBlock {
                    branches,
                    end: index,
                });
            }
            "endif" => depth = depth.saturating_sub(1),
            "elseif" | "else" if depth == 0 => markers.push(index),
            _ => {}
        }
        index += 1;
    }
    None
}

struct TryBlock {
    try_end: usize,
    catches: Vec<CatchBlock>,
    finally: Option<(usize, usize)>,
    end: usize,
}
struct CatchBlock {
    pattern: Option<String>,
    start: usize,
    end: usize,
}

fn find_try(program: &[Instruction], open: usize, limit: usize) -> Option<TryBlock> {
    let mut depth = 0usize;
    let mut markers = Vec::new();
    let mut index = open + 1;
    while index < limit {
        match program[index].name() {
            "try" => depth += 1,
            "endtry" if depth == 0 => {
                let try_end = markers.first().copied().unwrap_or(index);
                let mut catches = Vec::new();
                let mut finally = None;
                for (position, marker) in markers.iter().enumerate() {
                    let next = markers.get(position + 1).copied().unwrap_or(index);
                    match program[*marker].name() {
                        "catch" => catches.push(CatchBlock {
                            pattern: parse_catch_pattern(&program[*marker].command.as_ref()?.args),
                            start: marker + 1,
                            end: next,
                        }),
                        "finally" => finally = Some((marker + 1, index)),
                        _ => {}
                    }
                }
                return Some(TryBlock {
                    try_end,
                    catches,
                    finally,
                    end: index,
                });
            }
            "endtry" => depth = depth.saturating_sub(1),
            "catch" | "finally" if depth == 0 => markers.push(index),
            _ => {}
        }
        index += 1;
    }
    None
}

fn find_matching(
    program: &[Instruction],
    open: usize,
    limit: usize,
    opener: &str,
    closer: &str,
) -> Option<usize> {
    let mut depth = 0usize;
    for (index, instruction) in program.iter().enumerate().take(limit).skip(open + 1) {
        let name = instruction.name();
        if name == opener {
            depth += 1;
        } else if name == closer {
            if depth == 0 {
                return Some(index);
            }
            depth -= 1;
        }
    }
    None
}

fn parse_catch_pattern(args: &str) -> Option<String> {
    let args = args.trim();
    if args.is_empty() {
        return None;
    }
    let delimiter = args.chars().next()?;
    take_delimited(args, delimiter).map(|(pattern, _)| pattern)
}

fn regex_matches_catch_pattern(pattern: &str, text: &str) -> Result<bool, String> {
    let program = compile_regex(pattern, Magic::Magic).map_err(|error| error.to_string())?;
    Ok(ox_regex::exec(&program, &RegexText::new(text.to_owned())).is_some())
}

pub(crate) fn render_command(command: &ExCommand) -> String {
    let mut text = render_command_mods(&command.modifiers);
    if let Some(range) = &command.range {
        text.push_str(&render_range(range));
    }
    text.push_str(command.command.name());
    if command.bang {
        text.push('!');
    }
    if !command.args.is_empty() {
        text.push(' ');
        text.push_str(&command.args);
    }
    text
}

fn render_range(range: &Range) -> String {
    if matches!(range.kind, RangeKind::WholeBuffer) {
        return "%".to_owned();
    }
    let mut text = range.start.as_ref().map(render_address).unwrap_or_default();
    if let Some(end) = &range.end {
        text.push(',');
        text.push_str(&render_address(end));
    }
    text
}

fn render_address(address: &Address) -> String {
    let mut text = match &address.base {
        AddressBase::Current => ".".to_owned(),
        AddressBase::Last => "$".to_owned(),
        AddressBase::Line(line) => line.to_string(),
        AddressBase::Mark(mark) => format!("'{mark}"),
        AddressBase::ForwardSearch(pattern) => format!("/{pattern}/"),
        AddressBase::BackwardSearch(pattern) => format!("?{pattern}?"),
    };
    for offset in &address.offsets {
        if *offset >= 0 {
            text.push('+');
            text.push_str(&offset.to_string());
        } else {
            text.push_str(&offset.to_string());
        }
    }
    text
}

fn split_comma_args(source: &str) -> Vec<&str> {
    split_top_level(source, b',', true)
}

fn split_top_level(source: &str, delimiter: u8, exact: bool) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut start = 0usize;
    let mut quote = None;
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if byte == active && (index == 0 || bytes[index - 1] != b'\\') {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(byte, b')' | b']' | b'}') {
            depth = depth.saturating_sub(1);
        } else if depth == 0
            && (byte == delimiter || (!exact && delimiter == b' ' && byte.is_ascii_whitespace()))
        {
            if start < index {
                result.push(source[start..index].trim());
            }
            while index + 1 < bytes.len() && bytes[index + 1].is_ascii_whitespace() {
                index += 1;
            }
            start = index + 1;
        }
        index += 1;
    }
    if start < source.len() {
        result.push(source[start..].trim());
    }
    result
}

fn take_delimited(source: &str, delimiter: char) -> Option<(String, &str)> {
    let mut escaped = false;
    let start = delimiter.len_utf8();
    for (relative, character) in source[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == delimiter {
            let end = start + relative;
            return Some((
                source[start..end].to_owned(),
                &source[end + delimiter.len_utf8()..],
            ));
        }
    }
    None
}

fn expand_replacement(replacement: &str, groups: &[String]) -> String {
    let mut output = String::new();
    let mut chars = replacement.chars();
    while let Some(character) = chars.next() {
        if character == '&' {
            output.push_str(groups.first().map_or("", String::as_str));
            continue;
        }
        if character != '\\' {
            output.push(character);
            continue;
        }
        match chars.next() {
            Some('0' | '&') => output.push_str(groups.first().map_or("", String::as_str)),
            Some(digit @ '1'..='9') => {
                let index = digit.to_digit(10).unwrap_or(0) as usize;
                output.push_str(groups.get(index).map_or("", String::as_str));
            }
            Some('r') => output.push('\r'),
            Some('n') => output.push('\n'),
            Some('t') => output.push('\t'),
            Some(other) => output.push(other),
            None => output.push('\\'),
        }
    }
    output
}

fn substitute_plain(
    source: &str,
    pattern: &str,
    replacement: &str,
    global: bool,
) -> Result<String, String> {
    let program = compile_regex(pattern, Magic::Magic).map_err(|error| error.to_string())?;
    let text = RegexText::new(source.to_owned());
    let mut output = String::new();
    let mut previous = 0usize;
    let mut cursor = 0usize;
    while cursor <= source.len() {
        let Some(position) = text.position(cursor) else {
            break;
        };
        let Some(matched) = regex_exec_at(&program, &text, position) else {
            break;
        };
        output.push_str(&source[previous..matched.start.byte]);
        let mut groups = vec![source[matched.start.byte..matched.end.byte].to_owned()];
        for capture in &matched.captures {
            groups.push(capture.as_ref().map_or_else(String::new, |capture| {
                source[capture.start.byte..capture.end.byte].to_owned()
            }));
        }
        output.push_str(&expand_replacement(replacement, &groups));
        previous = matched.end.byte;
        if !global {
            break;
        }
        cursor = if matched.start.byte == matched.end.byte {
            next_boundary(source, matched.end.byte)
        } else {
            matched.end.byte
        };
        if cursor > source.len() {
            break;
        }
    }
    output.push_str(&source[previous..]);
    Ok(output)
}

fn next_boundary(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len().saturating_add(1);
    }
    at + text[at..].chars().next().map_or(1, char::len_utf8)
}

pub(crate) fn typval_to_text(value: &Typval) -> String {
    match value {
        Typval::String(value) => value.to_string_lossy().into_owned(),
        _ => typval_to_display(value, false),
    }
}

fn vim_float_text(value: f64) -> String {
    let mut text = value.to_string();
    if value.is_finite() && !text.contains(['.', 'e', 'E']) {
        text.push_str(".0");
    }
    text
}

fn typval_to_display(value: &Typval, quoted_strings: bool) -> String {
    typval_to_display_inner(value, quoted_strings, &mut Vec::new())
}

fn typval_to_display_inner(
    value: &Typval,
    quoted_strings: bool,
    containers: &mut Vec<(u8, *const ())>,
) -> String {
    let identity = match value {
        Typval::List(list) => Some((0, Rc::as_ptr(list).cast())),
        Typval::Dict(dict) => Some((1, Rc::as_ptr(dict).cast())),
        _ => None,
    };
    if let Some(identity) = identity {
        if containers.contains(&identity) {
            return if identity.0 == 0 {
                "[...]".to_owned()
            } else {
                "{...}".to_owned()
            };
        }
        containers.push(identity);
    }
    let display = match value {
        Typval::Number(value) => value.to_string(),
        Typval::Float(value) => vim_float_text(*value),
        Typval::String(value) => {
            let text = value.to_string_lossy();
            if quoted_strings {
                format!("'{}'", text.replace('\'', "''"))
            } else {
                text.into_owned()
            }
        }
        Typval::Blob(bytes) => format!(
            "0z{}",
            bytes.iter().fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02X}");
                hex
            })
        ),
        Typval::List(list) => list.try_borrow().map_or("[]".to_owned(), |data| {
            format!(
                "[{}]",
                data.items
                    .iter()
                    .map(|item| typval_to_display_inner(item, true, containers))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        Typval::Dict(dict) => dict.try_borrow().map_or("{}".to_owned(), |data| {
            format!(
                "{{{}}}",
                data.entries
                    .iter()
                    .map(|entry| {
                        format!(
                            "'{}': {}",
                            entry.key.to_string_lossy().replace('\'', "''"),
                            typval_to_display_inner(&entry.value, true, containers)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        Typval::Funcref(function) | Typval::Partial(function) => {
            format!("function('{}')", function.name.to_string_lossy())
        }
        Typval::Bool(value) => {
            if *value {
                "v:true".to_owned()
            } else {
                "v:false".to_owned()
            }
        }
        Typval::Special(Special::Null) => "v:null".to_owned(),
        Typval::Channel(id) | Typval::Job(id) => id.to_string(),
    };
    if identity.is_some() {
        containers.pop();
    }
    display
}

#[expect(
    clippy::cast_precision_loss,
    reason = "Vim promotes Number operands to its double-precision Float type"
)]
fn assignment_float(value: &Typval) -> Option<f64> {
    match value {
        Typval::Float(value) => Some(*value),
        value => typval_number(value).map(|value| value as f64),
    }
}

pub(crate) fn typval_number(value: &Typval) -> Option<i64> {
    match value {
        Typval::Number(value) => Some(*value),
        Typval::Bool(value) => Some(i64::from(*value)),
        Typval::String(value) => value.to_string_lossy().parse().ok(),
        Typval::Channel(value) | Typval::Job(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}
fn parse_number_prefix(text: &str) -> i64 {
    let bytes = text.trim_start().as_bytes();
    let mut end = 0;
    if bytes
        .first()
        .is_some_and(|byte| matches!(byte, b'+' | b'-'))
    {
        end = 1;
    }
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == 0 || (end == 1 && matches!(bytes.first(), Some(b'+' | b'-'))) {
        return 0;
    }
    std::str::from_utf8(&bytes[..end])
        .ok()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

pub(crate) fn option_to_typval(value: &OptionValue) -> Typval {
    match value {
        OptionValue::Boolean(value) => Typval::Number(i64::from(*value)),
        OptionValue::Number(value) => Typval::Number(*value),
        OptionValue::String(value) => Typval::String(OxStr::from(value.as_str())),
    }
}

fn dictionary_function_target(
    scope: &Scope,
    name: &str,
) -> Result<Option<(DictRef, OxStr)>, (&'static str, String)> {
    let Some((dictionary, member)) = name.rsplit_once('.') else {
        return Ok(None);
    };
    let mut path = dictionary.split('.');
    let Some(root) = path.next() else {
        // `split` always yields at least one item, so this arm is dead;
        // a name without a dictionary simply is not a dictionary function.
        return Ok(None);
    };
    let root_value = if root.as_bytes().get(1) == Some(&b':') {
        let kind = ScopeKind::from_byte(root.as_bytes()[0])
            .ok_or_else(|| ("E128", format!("Invalid function name: {name}")))?;
        if kind == ScopeKind::Global {
            return Err(("E862", "Cannot use g: here".to_owned()));
        }
        scope.get_scoped(kind, &root.as_bytes()[2..], 0)
    } else {
        scope.get(root.as_bytes(), 0)
    }
    .map_err(|error| (error.code, error.message))?
    .clone();

    let Typval::Dict(mut current) = root_value else {
        return Err(("E715", "Dictionary required".to_owned()));
    };
    for key in path {
        let value = current
            .try_borrow()
            .map_err(|_| {
                (
                    "E724",
                    "Unable to correctly dump variable with self-referencing container".to_owned(),
                )
            })?
            .entries
            .iter()
            .find(|entry| entry.key.as_bytes() == key.as_bytes())
            .map(|entry| entry.value.clone())
            .ok_or_else(|| ("E716", format!("Key not present in Dictionary: {key}")))?;
        current = match value {
            Typval::Dict(dictionary) => dictionary,
            _ => return Err(("E715", "Dictionary required".to_owned())),
        };
    }
    Ok(Some((current, OxStr::from(member))))
}

pub(crate) fn typval_to_option(
    value: &Typval,
    value_type: OptionType,
) -> Result<OptionValue, String> {
    match value_type {
        OptionType::Boolean => typval_number(value)
            .map(|value| OptionValue::Boolean(value != 0))
            .ok_or_else(|| "Number required".to_owned()),
        OptionType::Number => typval_number(value)
            .map(OptionValue::Number)
            .ok_or_else(|| "Number required".to_owned()),
        OptionType::String => Ok(OptionValue::String(typval_to_text(value))),
    }
}

pub(crate) fn option_value<'a>(
    editor: &'a Editor,
    name: &str,
    layer: SetLayer,
) -> Option<&'a OptionValue> {
    let metadata = crate::option_metadata(name)?;
    match layer {
        SetLayer::Global => editor.options().get_global(metadata.name).ok(),
        SetLayer::Local | SetLayer::Effective => {
            if metadata.scopes.contains(&OptionScope::Window) {
                editor
                    .current_window()
                    .and_then(|window| editor.options().get_window(window, metadata.name).ok())
            } else if metadata.scopes.contains(&OptionScope::Buffer) {
                editor
                    .current_buffer()
                    .and_then(|buffer| editor.options().get_buffer(buffer, metadata.name).ok())
            } else {
                editor.options().get_global(metadata.name).ok()
            }
        }
    }
}

/// Writes one option and reports the committed assignment. The effective
/// value is read *before* the write so `changed` reflects old-vs-new
/// (upstream `os_value_changed`), not whether the store mutated: a first
/// local overlay equal to the fallback default is unchanged even though the
/// overlay map gains an entry.
fn set_option_value(
    editor: &mut Editor,
    name: &str,
    value: OptionValue,
    layer: SetLayer,
) -> Result<OptionAssignment, (&'static str, String)> {
    let metadata =
        crate::option_metadata(name).ok_or_else(|| ("E355", format!("Unknown option: {name}")))?;
    let old = option_value(editor, metadata.name, layer).cloned();
    let result = match layer {
        SetLayer::Global => {
            if metadata.scopes.contains(&OptionScope::Global) {
                editor
                    .options_mut()
                    .set_global(metadata.name, value.clone())
                    .map(|()| None)
            } else {
                editor
                    .options_mut()
                    .set_global_default(metadata.name, value.clone())
                    .map(|()| None)
            }
        }
        SetLayer::Local => {
            if metadata.scopes.contains(&OptionScope::Window) {
                let window = editor
                    .current_window()
                    .ok_or_else(|| ("E355", "No current window".to_owned()))?;
                editor
                    .options_mut()
                    .set_window(window, metadata.name, value.clone())
                    .map(|()| None)
            } else if metadata.scopes.contains(&OptionScope::Buffer) {
                let buffer = editor
                    .current_buffer()
                    .ok_or_else(|| ("E355", "No current buffer".to_owned()))?;
                editor
                    .options_mut()
                    .set_buffer(buffer, metadata.name, value.clone())
                    .map(|()| Some(buffer))
            } else {
                editor
                    .options_mut()
                    .set_global(metadata.name, value.clone())
                    .map(|()| None)
            }
        }
        SetLayer::Effective => {
            // `:set` writes the global value and, for global-local options,
            // the current buffer/window overlay (`option.c` `set_option_value`).
            if metadata.scopes.contains(&OptionScope::Global) {
                editor
                    .options_mut()
                    .set_global(metadata.name, value.clone())
                    .map_err(|error| ("E474", error.to_string()))?;
            } else if metadata.scopes.contains(&OptionScope::Buffer) {
                // Buffer-only options (no global scope) still have a global
                // baseline that new buffers inherit. `:set` updates it so
                // subsequently created buffers pick up the change, then
                // writes the current buffer's overlay.
                editor
                    .options_mut()
                    .set_global_default(metadata.name, value.clone())
                    .map_err(|error| ("E474", error.to_string()))?;
            }
            if metadata.scopes.contains(&OptionScope::Window) {
                if let Some(window) = editor.current_window() {
                    editor
                        .options_mut()
                        .set_window(window, metadata.name, value.clone())
                        .map(|()| None)
                } else if metadata.scopes.contains(&OptionScope::Global) {
                    Ok(None)
                } else {
                    Err(crate::OptionError::UnknownOption(metadata.name.to_owned()))
                }
            } else if metadata.scopes.contains(&OptionScope::Buffer) {
                if let Some(buffer) = editor.current_buffer() {
                    editor
                        .options_mut()
                        .set_buffer(buffer, metadata.name, value.clone())
                        .map(|()| Some(buffer))
                } else if metadata.scopes.contains(&OptionScope::Global) {
                    Ok(None)
                } else {
                    Err(crate::OptionError::UnknownOption(metadata.name.to_owned()))
                }
            } else {
                Ok(None)
            }
        }
    };
    let buffer = result.map_err(|error| ("E474", error.to_string()))?;
    if metadata.name == "modified"
        && let Some(buffer) = buffer
        && let OptionValue::Boolean(modified) = &value
    {
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ("E474", error.to_string()))?;
        if *modified {
            state.mark_modified();
        } else {
            state.mark_saved();
        }
    }
    Ok(OptionAssignment {
        name: metadata.name,
        buffer,
        changed: old.as_ref() != Some(&value),
        value,
    })
}

/// Applies one `:set` argument. Returns the committed assignment for writing
/// forms; query and display forms commit nothing and report `None`.
fn set_one<E: ExEditorAccess>(
    access: &E,
    scope: &mut Scope,
    raw: &str,
    layer: SetLayer,
) -> Result<Option<OptionAssignment>, (&'static str, String)> {
    let raw = raw.trim();
    if let Some(name) = raw.strip_suffix('?') {
        if let Some(text) = access.with_ex_editor(|editor| display_option(editor, name, layer)) {
            access.with_ex_editor(|editor| push_info_text_message(editor, text));
            return Ok(None);
        }
        return Err(("E518", format!("Unknown option: {name}")));
    }
    if let Some(name) = raw.strip_suffix("&vim").or_else(|| raw.strip_suffix('&')) {
        let metadata = crate::option_metadata(name)
            .ok_or_else(|| ("E518", format!("Unknown option: {name}")))?;
        let value = metadata
            .default
            .value
            .map(OptionValue::from)
            .ok_or_else(|| ("E474", format!("No literal default for {name}")))?;
        return access
            .with_ex_editor(|editor| set_and_mirror(editor, scope, metadata.name, &value, layer))
            .map(Some);
    }
    for operator in ["+=", "-=", "^=", "="] {
        if let Some((name, value)) = raw.split_once(operator) {
            let metadata = crate::option_metadata(name)
                .ok_or_else(|| ("E518", format!("Unknown option: {name}")))?;
            let mut next = match metadata.value_type {
                OptionType::Boolean => OptionValue::Boolean(matches!(value, "1" | "true" | "on")),
                OptionType::Number => OptionValue::Number(
                    value
                        .parse()
                        .map_err(|_| ("E521", format!("Number required after =: {value}")))?,
                ),
                OptionType::String => OptionValue::String(if metadata.expand {
                    expand_env_esc(value)
                } else {
                    value.to_owned()
                }),
            };
            if operator != "=" {
                let current = access
                    .with_ex_editor(|editor| option_value(editor, metadata.name, layer).cloned())
                    .unwrap_or_else(|| {
                        metadata
                            .default
                            .value
                            .map_or(OptionValue::String(String::new()), OptionValue::from)
                    });
                next = modify_option(current, next, operator, metadata.list)?;
            }
            return access
                .with_ex_editor(|editor| set_and_mirror(editor, scope, metadata.name, &next, layer))
                .map(Some);
        }
    }
    let (name, value) = if let Some(name) = raw.strip_prefix("no") {
        (name, false)
    } else if let Some(name) = raw.strip_prefix("inv") {
        let current = access
            .with_ex_editor(|editor| option_value(editor, name, layer).cloned())
            .and_then(|value| match value {
                OptionValue::Boolean(value) => Some(value),
                _ => None,
            })
            .unwrap_or(false);
        (name, !current)
    } else {
        (raw, true)
    };
    let metadata =
        crate::option_metadata(name).ok_or_else(|| ("E518", format!("Unknown option: {name}")))?;
    if metadata.value_type != OptionType::Boolean
        && let Some(text) = access.with_ex_editor(|editor| display_option(editor, name, layer))
    {
        access.with_ex_editor(|editor| push_info_text_message(editor, text));
        return Ok(None);
    }
    access
        .with_ex_editor(|editor| {
            set_and_mirror(
                editor,
                scope,
                metadata.name,
                &OptionValue::Boolean(value),
                layer,
            )
        })
        .map(Some)
}

/// Writes one option to the editor and mirrors it into the eval scope, the
/// same dual write `:let &opt` performs through `assign_option`. Without the
/// mirror, `&opt` reads inside the same command batch would keep observing
/// the pre-`:set` snapshot until the next editor→scope sync.
fn set_and_mirror(
    editor: &mut Editor,
    scope: &mut Scope,
    name: &'static str,
    value: &OptionValue,
    layer: SetLayer,
) -> Result<OptionAssignment, (&'static str, String)> {
    let assignment = set_option_value(editor, name, value.clone(), layer)?;
    let eval_scope = eval_option_scope(name, layer);
    // Same alias mirror as `assign_option`: `&opt` reads keep the written
    // spelling, so every accepted name must see the new value immediately.
    // `set_option_value` above already rejected unknown names, so a missing
    // metadata cannot occur; `if let` keeps the mirror inert rather than
    // inventing a fallback for that impossible state.
    if let Some(metadata) = crate::option_metadata(name) {
        let rendered = option_to_typval(value);
        let accepted = std::iter::once(metadata.name)
            .chain(metadata.short_name.iter().copied())
            .chain(metadata.aliases.iter().copied());
        for entry in accepted {
            scope.set_option(eval_scope, entry.as_bytes(), rendered.clone());
        }
    }
    Ok(assignment)
}

/// `expand_env_esc` (`os/env.c`), reached from `:set` value expansion for
/// `expand`-flag options (`option.c` `stropt_expand_envvar`) and from
/// `expand()`'s file path (`ExpandOne`): a leading `~` resolves through
/// `$HOME`, and each `$NAME`/`${NAME}` resolves through the process
/// environment. An unset variable stays literal, matching upstream
/// `vim_getenv` returning NULL; substituted text is never rescanned.
pub(crate) fn expand_env_esc(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    if bytes.first() == Some(&b'~')
        && (bytes.len() == 1 || bytes[1] == b'/')
        && let Some(home) = std::env::var_os("HOME")
    {
        output.extend_from_slice(home.to_string_lossy().as_bytes());
        index = 1;
    }
    while index < bytes.len() {
        if bytes[index] != b'$' || index + 1 >= bytes.len() {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        let (name, next) = if bytes[index + 1] == b'{' {
            if let Some(close) = bytes[index + 2..].iter().position(|&byte| byte == b'}') {
                (&value[index + 2..index + 2 + close], index + 2 + close + 1)
            } else {
                output.push(b'$');
                index += 1;
                continue;
            }
        } else {
            let end = bytes[index + 1..]
                .iter()
                .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
                .map_or(bytes.len(), |offset| index + 1 + offset);
            (&value[index + 1..end], end)
        };
        if name.is_empty() {
            output.push(b'$');
            index += 1;
            continue;
        }
        if let Some(text) = std::env::var_os(name) {
            output.extend_from_slice(text.to_string_lossy().as_bytes());
        } else {
            // Unset stays literal, like upstream `vim_getenv` returning NULL.
            output.push(b'$');
            output.extend_from_slice(
                if bytes[index + 1] == b'{' {
                    format!("{{{name}}}")
                } else {
                    name.to_owned()
                }
                .as_bytes(),
            );
        }
        index = next;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn modify_option(
    current: OptionValue,
    next: OptionValue,
    operator: &str,
    list: Option<OptionListKind>,
) -> Result<OptionValue, (&'static str, String)> {
    match (current, next) {
        (OptionValue::Number(left), OptionValue::Number(right)) => {
            Ok(OptionValue::Number(match operator {
                "+=" => left.saturating_add(right),
                "-=" => left.saturating_sub(right),
                "^=" => right.saturating_mul(10).saturating_add(left),
                _ => right,
            }))
        }
        (OptionValue::String(mut left), OptionValue::String(right)) => {
            if let Some(
                kind @ (OptionListKind::Comma
                | OptionListKind::OneComma
                | OptionListKind::CommaColon
                | OptionListKind::OneCommaColon
                | OptionListKind::FlagsComma),
            ) = list
            {
                left = modify_comma_list(&left, &right, operator, kind);
            } else {
                match operator {
                    "+=" => left.push_str(&right),
                    "^=" => left.insert_str(0, &right),
                    "-=" => left = left.replace(&right, ""),
                    _ => {}
                }
            }
            Ok(OptionValue::String(left))
        }
        _ => Err(("E734", format!("Wrong variable type for {operator}"))),
    }
}

fn modify_comma_list(left: &str, right: &str, operator: &str, kind: OptionListKind) -> String {
    if right.is_empty() {
        return left.to_owned();
    }
    let colon = matches!(
        kind,
        OptionListKind::CommaColon | OptionListKind::OneCommaColon
    );
    let reject_empty = matches!(
        kind,
        OptionListKind::OneComma | OptionListKind::OneCommaColon
    );
    let mut items = CommaItems::new(left)
        .filter(|item| !reject_empty || !item.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for operand in CommaItems::new(right) {
        let matches = |item: &str| {
            if colon && let Some(offset) = find_unescaped(operand, ':') {
                return item.get(..=offset) == operand.get(..=offset);
            }
            item == operand
        };
        match operator {
            "-=" => items.retain(|item| !matches(item)),
            "+=" => {
                if colon {
                    items.retain(|item| !matches(item));
                }
                if !items.iter().any(|item| item == operand) {
                    items.push(operand.to_owned());
                }
            }
            "^=" => {
                if colon {
                    items.retain(|item| !matches(item));
                }
                if !items.iter().any(|item| item == operand) {
                    items.insert(0, operand.to_owned());
                }
            }
            _ => {}
        }
    }
    items.join(",")
}

fn split_set_args(args: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in args.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
            current.push(character);
        } else if character.is_whitespace() {
            if !current.is_empty() {
                output.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        output.push(current);
    }
    output
}

fn display_option(editor: &Editor, name: &str, layer: SetLayer) -> Option<String> {
    let metadata = crate::option_metadata(name)?;
    let value = option_value(editor, metadata.name, layer)?;
    Some(match value {
        OptionValue::Boolean(true) => metadata.name.to_owned(),
        OptionValue::Boolean(false) => format!("no{}", metadata.name),
        OptionValue::Number(value) => format!("{}={value}", metadata.name),
        OptionValue::String(value) => format!("{}={value}", metadata.name),
    })
}

fn option_is_default(editor: &Editor, name: &str) -> bool {
    let Some(metadata) = crate::option_metadata(name) else {
        return true;
    };
    let Some(default) = metadata.default.value.map(OptionValue::from) else {
        return false;
    };
    editor
        .options()
        .get_global(metadata.name)
        .is_ok_and(|value| value == &default)
}

fn map_modes(name: &str, bang: bool) -> MapModes {
    if bang {
        return MapModes::MAP_BANG;
    }
    match name.chars().next() {
        Some('n') if name != "noremap" => MapModes::one(MapMode::Normal),
        Some('v') => MapModes::one(MapMode::Visual) | MapModes::one(MapMode::Select),
        Some('x') => MapModes::one(MapMode::Visual),
        Some('s') => MapModes::one(MapMode::Select),
        Some('o') => MapModes::one(MapMode::OperatorPending),
        Some('i') => MapModes::one(MapMode::Insert),
        Some('c') => MapModes::one(MapMode::CommandLine),
        Some('l') => MapModes::one(MapMode::LangArg),
        Some('t') => MapModes::one(MapMode::Terminal),
        _ => MapModes::MAP,
    }
}

fn valid_user_command_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

pub(crate) fn buffer_lines(editor: &Editor, buffer: BufHandle) -> Result<Vec<Vec<u8>>, String> {
    let state = editor.buffer(buffer).map_err(|error| error.to_string())?;
    let text = state.text().map_err(|error| error.to_string())?;
    (1..=text.line_count())
        .map(|line| text.line(line).map_err(|error| error.to_string()))
        .collect()
}

fn dict_to_scope(dict: &Dict) -> ScopeMap {
    dict.0
        .iter()
        .map(|(key, value)| (key.clone(), object_to_typval(value)))
        .collect()
}
fn scope_to_dict(scope: &ScopeMap) -> Dict {
    Dict(
        scope
            .iter()
            .map(|(key, value)| (key.clone(), typval_to_object(value)))
            .collect(),
    )
}
pub(crate) fn object_to_typval(value: &Object) -> Typval {
    match value {
        Object::Nil => Typval::Special(Special::Null),
        Object::Boolean(value) => Typval::Bool(*value),
        Object::Integer(value) => Typval::Number(*value),
        Object::Float(value) => Typval::Float(*value),
        Object::String(value) => Typval::String(value.clone()),
        Object::Array(values) => Typval::list(values.iter().map(object_to_typval).collect()),
        Object::LuaRef(value) => Typval::Funcref(Funcref {
            name: lua_ref_function_name(u64::try_from(*value).unwrap_or(u64::MAX)),
            args: Vec::new(),
            dict: None,
            registry: None,
        }),
        Object::Dict(values) => decode_funcref_dict(values).unwrap_or_else(|| {
            Typval::dict(
                values
                    .0
                    .iter()
                    .map(|(key, value)| (key.clone(), object_to_typval(value)))
                    .collect(),
            )
        }),
        Object::Buffer(value) => Typval::Number(i64::from(*value)),
        Object::Window(value) => Typval::Number(i64::from(*value)),
        Object::Tabpage(value) => Typval::Number(i64::from(*value)),
    }
}
pub(crate) fn typval_to_object(value: &Typval) -> Object {
    match value {
        Typval::Number(value) => Object::Integer(*value),
        Typval::Float(value) => Object::Float(*value),
        Typval::String(value) => Object::String(value.clone()),
        Typval::Blob(value) => Object::Array(
            value
                .iter()
                .map(|byte| Object::Integer(i64::from(*byte)))
                .collect(),
        ),
        Typval::List(value) => value.try_borrow().map_or(Object::Nil, |data| {
            Object::Array(data.items.iter().map(typval_to_object).collect())
        }),
        Typval::Dict(value) => value.try_borrow().map_or(Object::Nil, |data| {
            Object::Dict(Dict(
                data.entries
                    .iter()
                    .map(|entry| (entry.key.clone(), typval_to_object(&entry.value)))
                    .collect(),
            ))
        }),
        Typval::Funcref(value) => encode_funcref(value, false),
        Typval::Partial(value) => encode_funcref(value, true),
        Typval::Bool(value) => Object::Boolean(*value),
        Typval::Special(Special::Null) => Object::Nil,
        Typval::Channel(value) | Typval::Job(value) => {
            Object::Integer(i64::try_from(*value).unwrap_or(i64::MAX))
        }
    }
}

/// Marker key so a Funcref survives `g:` round-trips through [`Object`]
/// (`typval_to_object` used to keep only the name as a String, so
/// `let g:F = function('setqflist')` became type 1).
const FUNCREF_MARK: &[u8] = b"\x01oxvim_funcref";

fn encode_funcref(function: &Funcref, partial: bool) -> Object {
    Object::Dict(Dict(vec![
        (
            OxStr::from(FUNCREF_MARK),
            Object::String(function.name.clone()),
        ),
        (OxStr::from("partial"), Object::Boolean(partial)),
        (
            OxStr::from("registry"),
            Object::Integer(
                function
                    .registry
                    .and_then(|id| i64::try_from(id).ok())
                    .unwrap_or(-1),
            ),
        ),
        (
            OxStr::from("args"),
            Object::Array(function.args.iter().map(typval_to_object).collect()),
        ),
    ]))
}

fn decode_funcref_dict(values: &Dict) -> Option<Typval> {
    let Object::String(name) = values.get(&OxStr::from(FUNCREF_MARK))? else {
        return None;
    };
    let partial = matches!(
        values.get(&OxStr::from("partial")),
        Some(Object::Boolean(true))
    );
    let registry = match values.get(&OxStr::from("registry")) {
        Some(Object::Integer(id)) if *id >= 0 => usize::try_from(*id).ok(),
        _ => None,
    };
    let args = match values.get(&OxStr::from("args")) {
        Some(Object::Array(items)) => items.iter().map(object_to_typval).collect(),
        _ => Vec::new(),
    };
    let function = Funcref {
        name: name.clone(),
        args,
        dict: None,
        registry,
    };
    Some(if partial {
        Typval::Partial(function)
    } else {
        Typval::Funcref(function)
    })
}

pub(crate) fn push_text_message(editor: &mut Editor, text: String, error: bool, history: bool) {
    editor.push_message(Message {
        kind: if error {
            MessageKind::Error
        } else {
            MessageKind::Echo
        },
        content: Object::String(OxStr(text.into_bytes())),
        history,
        leading_newline: true,
    });
}

/// Output of an informative listing command (`:print`, `:number`, `:list`,
/// `:set` display), upstream's `info_message` messages: `print_line`
/// (`ex_cmds.c` line 1701) and `showoneopt` (`option.c` line 4851) clear
/// `silent_mode` and write to stdout instead of stderr.
pub(crate) fn push_info_text_message(editor: &mut Editor, text: String) {
    editor.push_info_message(Message {
        kind: MessageKind::Echo,
        content: Object::String(OxStr(text.into_bytes())),
        history: false,
        leading_newline: true,
    });
}

/// E1513 for buffer switches out of a 'winfixbuf' window unless the
/// command's bang overrides (upstream `check_can_set_curbuf_forceit`,
/// window.c:216-224).
fn winfixbuf_blocks<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &Editor,
    forceit: bool,
) -> Option<Flow> {
    if forceit || !editor.current_window_fixed_to_buffer() {
        return None;
    }
    Some(error_flow(
        runtime,
        "E1513",
        "Cannot switch buffer. 'winfixbuf' is enabled",
    ))
}

fn error_flow<F: FileIO>(
    runtime: &ExRuntime<F>,
    code: &'static str,
    message: impl Into<String>,
) -> Flow {
    Flow::Exception(runtime.exception(code, message))
}
fn userfunc_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: UserFuncError) -> Flow {
    error_flow(runtime, error.code, error.message)
}
fn eval_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: EvalError) -> Flow {
    match error.kind {
        EvalErrorKind::NotImplemented(name) => {
            Flow::NotImplemented(name.to_string_lossy().into_owned())
        }
        EvalErrorKind::Vim => error_flow(runtime, error.code, error.message),
    }
}
pub(crate) fn exec_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: ExecError) -> Flow {
    match error {
        ExecError::Vim(exception) => Flow::Exception(exception),
        ExecError::NotImplemented(name) => Flow::NotImplemented(name),
        ExecError::Eval(error) => eval_error_flow(runtime, error),
        ExecError::Parse(error) => Flow::Exception(runtime.parse_exception(&error)),
        ExecError::Io { path, message } => {
            error_flow(runtime, "E484", format!("{}: {message}", path.display()))
        }
        ExecError::Editor(message) => error_flow(runtime, "E605", message),
        ExecError::DuplicateCommand { name } => error_flow(
            runtime,
            "E174",
            format!("Command already exists: add ! to replace it ({name})"),
        ),
    }
}
pub(crate) fn flow_to_eval_error(flow: Flow, name: &str) -> EvalError {
    match flow {
        Flow::Exception(exception) => EvalError::new("E605", 0, exception.message()),
        Flow::NotImplemented(name) => EvalError::not_implemented(OxStr(name.into_bytes())),
        _ => EvalError::new("E117", 0, format!("Unknown function: {name}")),
    }
}

fn command_lua<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let Some(lua) = lua else {
        return Flow::NotImplemented("lua".to_owned());
    };
    let mut code = command.args.trim_start().to_owned();
    let mut heredoc = false;
    if let Some((header, body)) = code.split_once('\n')
        && header.starts_with("<<")
    {
        heredoc = true;
        code = body.to_owned();
    }
    if code.is_empty() && !heredoc {
        if command.range.is_none() {
            return error_flow(runtime, "E471", "Argument required");
        }
        let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
            return error_flow(runtime, "E749", "Empty buffer");
        };
        let lines = match access.with_ex_editor(|editor| buffer_lines(editor, buffer)) {
            Ok(lines) => lines,
            Err(message) => return error_flow(runtime, "E749", message),
        };
        let (first, last) = match access.with_ex_editor(|editor| resolve_range(editor, command)) {
            Ok(range) => range,
            Err(message) => return error_flow(runtime, "E16", message),
        };
        code = lines[first.saturating_sub(1)..last.min(lines.len())]
            .iter()
            .map(|line| String::from_utf8_lossy(line))
            .collect::<Vec<_>>()
            .join("\n");
    } else if let Some(expression) = code.strip_prefix('=') {
        code = format!("vim._print(true, {expression})");
    }
    if let Err(error) = access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
        return exec_error_flow(runtime, error);
    }
    let result = lua.borrow_mut().execute_chunk(&code, Vec::new());
    let sync = access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
    match result {
        Err(error) => lua_error_flow(runtime, error, "E5107", "E5108"),
        Ok(result) => {
            lua.borrow_mut().discard_result(result);
            match sync {
                Ok(()) => Flow::Normal,
                Err(error) => exec_error_flow(runtime, error),
            }
        }
    }
}

fn command_luafile<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let Some(lua) = lua else {
        return Flow::NotImplemented("luafile".to_owned());
    };
    let path = command.args.trim();
    if path.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    if let Err(error) = access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
        return exec_error_flow(runtime, error);
    }
    let result = lua.borrow_mut().execute_file(Path::new(path));
    let sync = access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope));
    match (result, sync) {
        (Err(error), _) => lua_error_flow(runtime, error, "E5112", "E5113"),
        (Ok(()), Err(error)) => exec_error_flow(runtime, error),
        (Ok(()), Ok(())) => Flow::Normal,
    }
}

fn command_luado<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let Some(lua) = lua else {
        return Flow::NotImplemented("luado".to_owned());
    };
    let body = command.args.trim_start();
    if body.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let Some(buffer) = access.with_ex_editor(|editor| editor.current_buffer()) else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let (first, last) = if command.range.is_none() {
        match access.with_ex_editor(|editor| buffer_lines(editor, buffer)) {
            Ok(lines) => (1, lines.len()),
            Err(message) => return error_flow(runtime, "E749", message),
        }
    } else {
        match access.with_ex_editor(|editor| resolve_range(editor, command)) {
            Ok(range) => range,
            Err(message) => return error_flow(runtime, "E16", message),
        }
    };
    let chunk = format!("return (function(line, linenr) {body} end)(...)");
    if let Err(error) = access.with_ex_editor(|editor| sync_scope_into_editor(editor, scope)) {
        return exec_error_flow(runtime, error);
    }
    for lnum in first..=last {
        let Ok(lines) = access.with_ex_editor(|editor| buffer_lines(editor, buffer)) else {
            break;
        };
        let Some(line) = lines.get(lnum.saturating_sub(1)).cloned() else {
            break;
        };
        let result = match lua.borrow_mut().execute_chunk(
            &chunk,
            vec![
                Object::String(OxStr(line)),
                Object::Integer(match i64::try_from(lnum) {
                    Ok(lnum) => lnum,
                    Err(_) => return error_flow(runtime, "E475", "Invalid argument"),
                }),
            ],
        ) {
            Ok(result) => result,
            Err(error) => return lua_error_flow(runtime, error, "E5109", "E5111"),
        };
        let replacement = match &result {
            Object::String(value) => Some(value.as_bytes().to_vec()),
            Object::Integer(value) => Some(value.to_string().into_bytes()),
            Object::Float(value) => Some(value.to_string().into_bytes()),
            _ => None,
        };
        lua.borrow_mut().discard_result(result);
        if access.with_ex_editor(|editor| editor.current_buffer()) != Some(buffer) {
            break;
        }
        if let Some(replacement) = replacement {
            let cursor = access
                .with_ex_editor(|editor| {
                    editor
                        .current_window()
                        .and_then(|window| editor.window(window).ok().map(|state| state.cursor))
                })
                .unwrap_or(Position { lnum, col: 0 });
            if let Err(error) = access.with_ex_editor(|editor| {
                editor.replace_buffer_lines(crate::LineReplaceRequest {
                    buffer,
                    start: lnum,
                    end: lnum,
                    lines: &[replacement],
                    cursor_before: cursor,
                    cursor_after: cursor,
                    timestamp: 0,
                })
            }) {
                return error_flow(runtime, "E16", error.to_string());
            }
        }
    }
    match access.with_ex_editor(|editor| sync_editor_into_scope(editor, scope)) {
        Ok(()) => Flow::Normal,
        Err(error) => exec_error_flow(runtime, error),
    }
}

fn lua_error_flow<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    error: LuaExecError,
    load_code: &'static str,
    runtime_code: &'static str,
) -> Flow {
    match error {
        LuaExecError::Load(message) => error_flow(runtime, load_code, message),
        LuaExecError::Runtime(message) | LuaExecError::Conversion(message) => {
            error_flow(runtime, runtime_code, message)
        }
    }
}

/// `:cc[!] [nr]` and `:ll[!] [nr]`: display quickfix entry `nr` (default:
/// the current one) and jump to its file/line (`ex_cc`, quickfix.c).
fn command_quickfix_jump<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    _location: bool,
) -> Flow {
    let number = command.args.trim().parse::<usize>().ok();
    let target = match number {
        Some(number) => QuickfixMove::Absolute(number.saturating_sub(1)),
        None => QuickfixMove::Absolute(
            editor
                .quickfix()
                .current()
                .map_or(0, |list| list.idx().saturating_sub(1)),
        ),
    };
    match crate::quickfix::jump(editor, target, command.bang) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

/// `:cn[ext] [count]`, `:cp[revious] [count]`, `:clf[irst]`: move forward or
/// backward through the list by `count` entries (`ex_cnext`).
fn command_quickfix_next<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    direction: i64,
) -> Flow {
    let count = match command.count.map_or(Ok(1), |count| {
        usize::try_from(count).map_err(|_| error_flow(runtime, "E475", "Invalid argument"))
    }) {
        Ok(count) => count,
        Err(flow) => return flow,
    };
    let movement = match direction {
        1 => QuickfixMove::Next(count),
        -1 => QuickfixMove::Previous(count),
        _ => QuickfixMove::First,
    };
    match crate::quickfix::jump(editor, movement, command.bang) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

/// `:cla[st] [count]`: jump to the last entry in the list.
fn command_quickfix_last<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    match crate::quickfix::jump(editor, QuickfixMove::Last, command.bang) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

/// `:cope[n] [height]`: open the quickfix window.
fn command_quickfix_open<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    _command: &ExCommand,
) -> Flow {
    match crate::quickfix::open(editor) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

/// `:ccl[ose]`: close the quickfix window (`ex_cclose`, quickfix.c).
fn command_quickfix_close<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    match crate::quickfix::close(editor) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

fn command_quickfix_list<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(list) = editor.quickfix().current() else {
        return error_flow(runtime, "E42", "No Errors");
    };
    let lines: Vec<String> = list
        .items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            format!(
                "{:>2} {}: {}",
                index + 1,
                item.lnum,
                item.text.to_string_lossy()
            )
        })
        .collect();
    for line in lines {
        push_info_text_message(editor, line);
    }
    Flow::Normal
}

fn command_quickfix_age<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    delta: i32,
) -> Flow {
    match editor.quickfix_mut().shift_history(delta) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

/// `:cwin[dow]`: open the quickfix window only when the list has entries.
fn command_quickfix_window<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let empty = editor
        .quickfix()
        .current()
        .is_none_or(|list| list.items().is_empty());
    if empty && editor.quickfix().window().is_none() {
        return Flow::Normal;
    }
    match crate::quickfix::open(editor) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

/// `:cexpr`, `:cgetexpr`, `:caddexpr`: load quickfix entries from an
/// expression (normally a list of dicts).
fn command_quickfix_expr<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let value = match eval_text(runtime, access, scope, lua, skipwhite_trim(&command.args)) {
        Ok(value) => value,
        Err(flow) => return flow,
    };
    let items = match &value {
        Typval::List(reference) => reference
            .try_borrow()
            .map(|list| list.items.clone())
            .unwrap_or_default(),
        Typval::String(text) => vec![Typval::String(text.clone())],
        _ => return error_flow(runtime, "E777", "String or List expected"),
    };
    access.with_ex_editor(|editor| {
        command_quickfix_apply(runtime, editor, &items, command.command.name())
    })
}

/// `:cbuffer`, `:cgetbuffer`, `:caddbuffer`: load entries from lines of a
/// buffer (`qf_init_from_buffer`).
fn command_quickfix_buffer<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    _scope: &mut Scope,
    _lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let args = command.args.trim();
    let buffer = if args.is_empty() {
        editor.current_buffer()
    } else {
        BufHandle::try_from(args.parse::<i64>().unwrap_or(0))
            .ok()
            .filter(|handle| editor.buffer(*handle).is_ok())
    };
    let Some(buffer) = buffer else {
        return error_flow(runtime, "E681", "Buffer is not loaded");
    };
    let Ok(lines) = buffer_lines(editor, buffer) else {
        return error_flow(runtime, "E681", "Buffer is not loaded");
    };
    let items: Vec<Typval> = lines
        .iter()
        .map(|line| Typval::String(OxStr::from(line.as_slice())))
        .collect();
    command_quickfix_apply(runtime, editor, &items, command.command.name())
}

fn command_quickfix_file<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
) -> Flow {
    let path = command.args.trim();
    if path.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let Ok(text) = runtime.scripts.read_script(Path::new(path)) else {
        return error_flow(runtime, "E40", format!("Can't open file {path}"));
    };
    let items: Vec<Typval> = text
        .lines()
        .map(|line| Typval::String(OxStr::from(line)))
        .collect();
    command_quickfix_apply(runtime, editor, &items, command.command.name())
}

/// Applies parsed text or dict entries to the quickfix list: `:cexpr`-family
/// replaces the list, `:caddexpr`/`:cgetexpr` keep the history slot.
fn command_quickfix_apply<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    items: &[Typval],
    command_name: &str,
) -> Flow {
    let add = command_name.contains("add");
    let action = if add { 'a' } else { ' ' };
    let parsed = match crate::quickfix::parse_items(editor, items) {
        Ok(parsed) => parsed,
        Err(error) => return error_flow(runtime, error.code, error.message),
    };
    let stack = editor.quickfix_mut();
    if action == ' ' {
        stack.push(OxStr::from(":cexpr"));
    }
    if let Some(list) = stack.current_mut() {
        if action == 'a' {
            list.append_items(parsed);
        } else {
            list.set_items(parsed);
        }
    }
    Flow::Normal
}
