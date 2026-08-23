//! Ex command execution against the single-writer [`Editor`] model.
//!
//! Parsing remains in `ox-excmd`; this module owns command/control state,
//! script and function frames, exception transfer, user commands, and the
//! narrow host adapters needed by `ox-eval` and `ox-regex`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::cell::RefCell;
use std::rc::Rc;

use ox_eval::scope::{OptionScope as EvalOptionScope, ScopeMap};
use ox_eval::{
    BufferHost, BuiltinHost, Builtins, EvalError, EvalErrorKind, Evaluator, Parser as ExprParser,
    RegexEngine, Scope, ScopeKind,
};
use ox_excmd::{
    AddrType, Address, AddressBase, CommandFlags, ExCommand, ModifierKind, ParseError, Parser as ExParser, Range, RangeKind,
    effective_addr_type, effective_flags,
    ResolvedCommand, UserCommandMatch, UserCommandProvider,
};
use ox_regex::{
    compile as compile_regex, exec_at as regex_exec_at, CompileError as RegexCompileError, Magic, Text as RegexText,
};
use ox_sys::LocaleCategory;
use ox_text::{Buffer, Position};
use ox_types::{BufHandle, Dict, DictRef, Funcref, Object, OxStr, Special, TabHandle, Typval, WinHandle};

use crate::autocmd::{AutocmdContext, AutocmdKind, AutocmdOptions, AugroupId, DeleteAutocmds, Event, FiringPlan};
use crate::extmark::{ExtmarkAttributes, ExtmarkId, ExtmarkPlacement, ExtmarkPosition, NamespaceId};
use crate::mapping::{MapMode, MapModes, MapScope, MappingAction, MappingOptions};
use crate::options::{find_unescaped, CommaItems, OptionListKind, OptionScope, OptionType, OptionValue, OPTION_METADATA};
use crate::builtins::position::cell_width;
use crate::fold::{FoldMethod, Position as FoldPosition};
use crate::register::RegisterContent;
use crate::script::{FileIO, LogicalLine, RealFileIO, ScriptCtx, Sid};
use crate::typeahead::Keys;
use crate::userfunc::{UserFuncError, UserFunctions};
use crate::builtins::process::call_job_builtin;
use crate::{
    BufferRelease, ChannelIds, Editor, Geometry, JobManager, Message, MessageKind, ModeMachine,
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
    /// Thrown/evaluated value.
    pub value: Typval,
    /// Upstream-style source/call chain.
    pub throwpoint: String,
}

impl VimException {
    /// String matched by a `:catch` pattern.
    #[must_use]
    pub fn message(&self) -> String {
        let value = typval_to_display(&self.value, false);
        match &self.kind {
            VimExceptionKind::Throw => value,
            VimExceptionKind::Error(code) => format!("{code}: {value}"),
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

/// Host seam used by Ex Lua commands without coupling `ox-editor` to `ox-lua`.
pub trait LuaExec {
    /// Compile and execute one Lua chunk with varargs.
    fn execute_chunk(
        &mut self,
        editor: &mut Editor,
        code: &str,
        args: Vec<Object>,
    ) -> Result<Object, LuaExecError>;

    /// Load and execute one Lua file.
    fn execute_file(&mut self, editor: &mut Editor, path: &Path) -> Result<(), LuaExecError>;
    /// Evaluate one Lua expression with `_A` bound to `arg` (`luaeval()`).
    ///
    /// Hosts wrap the expression exactly like upstream `nlua_call_luaeval`
    /// (`local _A=select(1,...) return (<expr>)`) and convert the argument
    /// and result with typval semantics; hosts without a typval bridge
    /// report the missing capability.
    fn eval_expression(
        &mut self,
        _editor: &mut Editor,
        _expression: &str,
        _arg: Option<&Typval>,
    ) -> Result<Typval, LuaExecError> {
        Err(LuaExecError::Runtime("luaeval host is not installed".to_owned()))
    }

    /// Invoke a Lua registry callback with values converted by the host.
    fn invoke_callback(
        &mut self,
        _editor: &mut Editor,
        _reference: usize,
        _args: Vec<Object>,
    ) -> Result<(), LuaExecError> {
        Err(LuaExecError::Runtime("Lua callbacks are not installed".to_owned()))
    }
}

/// Definition created by `:command`.
#[derive(Clone, Debug)]
pub struct UserCommand {
    /// Canonical uppercase command name.
    pub name: String,
    /// Ex body with `<args>`/`<bang>` placeholders.
    pub body: String,
    /// Accepted argument count (`0`, `1`, `?`, `+`, or `*`).
    pub nargs: char,
    /// Whether invocation accepts `!`.
    pub accepts_bang: bool,
    /// Whether invocation accepts a range.
    pub accepts_range: bool,
    /// Whether invocation accepts a register.
    pub accepts_register: bool,
    /// Canonical source script and sourcing SID that defined this command.
    source: Option<(String, Sid)>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct UserCommandRegistry {
    commands: BTreeMap<String, UserCommand>,
}

impl UserCommandProvider for UserCommandRegistry {
    fn resolve_user_command(&self, typed: &str) -> UserCommandMatch {
        if !typed.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
            return UserCommandMatch::None;
        }
        if self.commands.contains_key(typed) {
            return UserCommandMatch::Match(typed.to_owned());
        }
        let matches = self
            .commands
            .keys()
            .filter(|name| name.starts_with(typed))
            .take(2)
            .cloned()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => UserCommandMatch::None,
            [name] => UserCommandMatch::Match(name.clone()),
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

pub(crate) struct ExRuntime<F: FileIO> {
    pub(crate) scripts: ScriptCtx<F>,
    pub(crate) functions: UserFunctions,
    pub(crate) user_commands: UserCommandRegistry,
    pub(crate) const_vars: BTreeSet<String>,
    pub(crate) channel_ids: ChannelIds,
    pub(crate) jobs: Option<JobManager>,
    pub(crate) current_augroup: AugroupId,
    pub(crate) redirection: Option<Redirection>,
    pub(crate) previous_dir: Option<PathBuf>,
    pub(crate) local_dir: Option<(WinHandle, PathBuf)>,
    /// `filetype_detect`/`filetype_plugin`/`filetype_indent`
    /// (`ex_docmd.c:7860-7884`): unset, enabled, or explicitly disabled.
    pub(crate) filetype: FiletypeState,
    /// `getout` (`main.c`:753) has begun, so `VimLeavePre`/`VimLeave` are done.
    pub(crate) exiting: bool,
}

impl<F: FileIO> ExRuntime<F> {
    fn new(io: F) -> Self {
        Self {
            scripts: ScriptCtx::new(io),
            functions: UserFunctions::new(),
            user_commands: UserCommandRegistry::default(),
            const_vars: BTreeSet::new(),
            channel_ids: ChannelIds::new(),
            jobs: None,
            current_augroup: AugroupId::default(),
            redirection: None,
            previous_dir: None,
            local_dir: None,
            filetype: FiletypeState::default(),
            exiting: false,
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

    fn exception(&self, code: &'static str, message: impl Into<String>) -> VimException {
        let message = message.into();
        VimException {
            kind: VimExceptionKind::Error(code.to_owned()),
            value: Typval::String(OxStr(message.into_bytes())),
            throwpoint: self.throwpoint(),
        }
    }
}

/// Stateful Ex execution host.
pub struct ExExecutor<F: FileIO = RealFileIO> {
    runtime: ExRuntime<F>,
    scope: Scope,
    lua: Option<Rc<RefCell<dyn LuaExec>>>,
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

impl<F: FileIO> ExExecutor<F> {
    /// Creates an executor using an injected IO seam.
    #[must_use]
    pub fn with_io(io: F) -> Self {
        let mut scope = Scope::new();
        for (name, value) in std::env::vars_os() {
            scope.set_env(
                name.to_string_lossy().as_bytes(),
                Typval::String(OxStr::from(value.to_string_lossy().as_ref())),
            );
        }
        Self {
            runtime: ExRuntime::new(io),
            scope,
            lua: None,
        }
    }

    /// Installs the Lua host used by `:lua`, `:luafile`, and `:luado`.
    pub fn set_lua_exec(&mut self, lua: Rc<RefCell<dyn LuaExec>>) {
        self.lua = Some(lua);
    }

    /// Share the host editor's dynamic channel key space with jobs.
    pub fn set_channel_ids(&mut self, channel_ids: ChannelIds) {
        self.runtime.channel_ids = channel_ids;
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

    /// Persistent Vimscript scope (notably `g:` and `$`).
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Call a stateful builtin through this executor's persistent runtime.
    pub fn call_builtin(
        &mut self,
        editor: &mut Editor,
        name: &OxStr,
        args: Vec<Typval>,
    ) -> Result<Typval, ExecError> {
        let lua = self.lua.clone();
        call_job_builtin(
            &mut self.runtime, editor, &mut self.scope, lua.as_ref(),
            &name.to_string_lossy(), args,
        )
        .map_err(ExecError::Eval)
    }

    /// Executes one command line, including bar-separated commands.
    pub fn execute_line(
        &mut self,
        editor: &mut Editor,
        line: &str,
    ) -> Result<ExecOutcome, ExecError> {
        let logical = vec![LogicalLine {
            text: line.to_owned(),
            first_line: 1,
        }];
        let program = parse_program(&self.runtime.user_commands, &logical)?;
        sync_editor_into_scope(editor, &mut self.scope)?;
        let flow = run_program(&mut self.runtime, editor, &mut self.scope, self.lua.as_ref(), &program, 0, program.len());
        self.finish_quit(editor, &flow);
        sync_scope_into_editor(editor, &self.scope)?;
        flow_to_result(flow)
    }
    /// Executes an already parsed command stream against `editor`.
    pub fn execute_commands(
        &mut self,
        editor: &mut Editor,
        commands: &[ExCommand],
    ) -> Result<ExecOutcome, ExecError> {
        let line = self.runtime.scripts.current_line().max(1);
        let program = commands
            .iter()
            .cloned()
            .map(|command| Instruction {
                source: render_command(&command),
                command: Some(command),
                parse_error: None,
                line,
            })
            .collect::<Vec<_>>();
        sync_editor_into_scope(editor, &mut self.scope)?;
        let flow = run_program(
            &mut self.runtime,
            editor,
            &mut self.scope,
            self.lua.as_ref(),
            &program,
            0,
            program.len(),
        );
        self.finish_quit(editor, &flow);
        sync_scope_into_editor(editor, &self.scope)?;
        flow_to_result(flow)
    }


    /// Executes source text with a fresh stable SID and isolated `s:` scope.
    pub fn execute_script(
        &mut self,
        editor: &mut Editor,
        source_name: &str,
        text: &str,
    ) -> Result<ExecOutcome, ExecError> {
        let lines = self.runtime.scripts.join_logical_lines(text).map_err(|error| {
            ExecError::Vim(self.runtime.exception(error.code, error.message))
        })?;
        let caller_script = self.scope.script.clone();
        let sid = self.runtime.scripts.push_source(source_name.to_owned());
        let lines = expand_script_lines(&self.runtime.scripts, lines, sid);
        self.runtime.scripts.load_script_scope(sid, &mut self.scope);
        let parsed = parse_program(&self.runtime.user_commands, &lines);
        let result = match parsed {
            Ok(program) => match sync_editor_into_scope(editor, &mut self.scope) {
                Ok(()) => {
                    let flow = run_program(
                        &mut self.runtime,
                        editor,
                        &mut self.scope,
                        self.lua.as_ref(),
                        &program,
                        0,
                        program.len(),
                    );
                    self.finish_quit(editor, &flow);
                    match sync_scope_into_editor(editor, &self.scope) {
                        Ok(()) => flow_to_result(flow),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        self.runtime.scripts.store_script_scope(sid, &self.scope);
        self.runtime.scripts.pop_source();
        self.scope.script = caller_script;
        result
    }

    /// Sources a file through [`FileIO`]. Plain `:source` executes each time.
    pub fn source_file(
        &mut self,
        editor: &mut Editor,
        path: &Path,
    ) -> Result<ExecOutcome, ExecError> {
        let flow = source_path(&mut self.runtime, editor, &mut self.scope, self.lua.as_ref(), path, false)?;
        self.finish_quit(editor, &flow);
        flow_to_result(flow)
    }

    /// `getout` (`main.c`:753): the exit sequence, run when a flow ends the
    /// process. It happens before the scope is synced back, so a `VimLeave`
    /// handler sees the state the quitting command left behind.
    fn finish_quit(&mut self, editor: &mut Editor, flow: &Flow) {
        if !matches!(flow, Flow::Quit(_)) {
            return;
        }
        let lua = self.lua.clone();
        fire_exit_autocmds(&mut self.runtime, editor, &mut self.scope, lua.as_ref());
    }

    /// Runs `getout`'s autocommands for an exit the host decided on rather
    /// than a command: the Ex loop reaching the end of its input, which
    /// `main.c` also finishes through `getout(0)`. Idempotent, so a host that
    /// calls it after a `:quit` has already exited fires nothing twice.
    pub fn run_exit_sequence(&mut self, editor: &mut Editor) -> Result<(), ExecError> {
        sync_editor_into_scope(editor, &mut self.scope)?;
        let lua = self.lua.clone();
        fire_exit_autocmds(&mut self.runtime, editor, &mut self.scope, lua.as_ref());
        sync_scope_into_editor(editor, &self.scope)
    }
}

#[derive(Clone)]
pub(crate) struct Instruction {
    command: Option<ExCommand>,
    parse_error: Option<ParseError>,
    source: String,
    line: usize,
}

impl Instruction {
    fn command(&self) -> Result<&ExCommand, &ParseError> {
        self.command.as_ref().ok_or_else(|| self.parse_error.as_ref().expect("deferred instruction has a parse error"))
    }

    fn name(&self) -> &str {
        self.command.as_ref().map_or("", |command| command.command.name())
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
        Flow::Break => Err(ExecError::Editor("E587: :break without :while or :for".to_owned())),
        Flow::Continue => Err(ExecError::Editor("E586: :continue without :while or :for".to_owned())),
        Flow::Return(_) => Err(ExecError::Editor("E133: :return not inside a function".to_owned())),
    }
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

pub(crate) fn parse_program(
    users: &UserCommandRegistry,
    logical: &[LogicalLine],
) -> Result<Vec<Instruction>, ExecError> {
    let parser = ExParser::with_user_commands(users);
    let mut program = Vec::new();
    for line in logical {
        let (command_text, heredoc_body) = line
            .text
            .split_once('\n')
            .map_or((line.text.as_str(), None), |(command, body)| (command, Some(body)));
        let commands = match parser.parse(command_text) {
            Ok(commands) => commands,
            Err(error) => {
                if let Some(commands) = parse_put_expression(&parser, command_text) {
                    commands
                } else {
                    program.push(Instruction {
                        command: None,
                        parse_error: Some(error),
                        source: command_text.to_owned(),
                        line: line.first_line,
                    });
                    continue;
                }
            }
        };
        for mut command in commands {
            if let Some(body) = heredoc_body {
                command.args.push('\n');
                command.args.push_str(body);
            }
            program.push(Instruction {
                source: render_command(&command),
                command: Some(command),
                parse_error: None,
                line: line.first_line,
            });
        }
    }
    Ok(program)
}

fn parse_put_expression(
    parser: &ExParser<'_, UserCommandRegistry>,
    line: &str,
) -> Option<Vec<ExCommand>> {
    for (offset, _) in line.match_indices('=') {
        let Ok(mut commands) = parser.parse(&line[..=offset]) else { continue };
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
        commands[0].args = expression.to_owned();
        commands[0].span.end = line.len();
        return Some(commands);
    }
    None
}

pub(crate) fn run_program<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
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
        let command = match instruction.command() {
            Ok(command) => command,
            Err(error) => return exec_error_flow(runtime, ExecError::Parse(error.clone())),
        };
        let name = command.command.name();
        match name {
            "if" => {
                let Some(block) = find_if(program, pc, end) else {
                    return error_flow(runtime, "E171", "Missing :endif");
                };
                let mut chosen = None;
                for branch in &block.branches {
                    let take = match branch.condition.as_deref() {
                        Some(condition) => match eval_condition(runtime, editor, scope, lua, condition) {
                            Ok(value) => value,
                            Err(flow) => return flow,
                        },
                        None => true,
                    };
                    if take {
                        chosen = Some((branch.start, branch.end));
                        break;
                    }
                }
                if let Some((branch_start, branch_end)) = chosen {
                    let flow = run_program(runtime, editor, scope, lua, program, branch_start, branch_end);
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                }
                pc = block.end + 1;
                continue;
            }
            "while" => {
                let Some(block_end) = find_matching(program, pc, end, "while", "endwhile") else {
                    return error_flow(runtime, "E170", "Missing :endwhile");
                };
                loop {
                    match eval_condition(runtime, editor, scope, lua, command.args.trim()) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(flow) => return flow,
                    }
                    match run_program(runtime, editor, scope, lua, program, pc + 1, block_end) {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        flow => return flow,
                    }
                }
                pc = block_end + 1;
                continue;
            }
            "for" => {
                let Some(block_end) = find_matching(program, pc, end, "for", "endfor") else {
                    return error_flow(runtime, "E170", "Missing :endfor");
                };
                let Some((target, expression)) = split_for(&command.args) else {
                    return error_flow(runtime, "E690", "Missing \"in\" after :for");
                };
                let value = match eval_text(runtime, editor, scope, lua, expression) {
                    Ok(value) => value,
                    Err(flow) => return flow,
                };
                let values = match iterable_values(value) {
                    Ok(values) => values,
                    Err(message) => return error_flow(runtime, "E714", message),
                };
                for value in values {
                    if let Err(flow) = assign_target(runtime, editor, scope, target, value, false) {
                        return flow;
                    }
                    match run_program(runtime, editor, scope, lua, program, pc + 1, block_end) {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        flow => return flow,
                    }
                }
                pc = block_end + 1;
                continue;
            }
            "try" => {
                let Some(block) = find_try(program, pc, end) else {
                    return error_flow(runtime, "E600", "Missing :endtry");
                };
                let mut pending = run_program(runtime, editor, scope, lua, program, pc + 1, block.try_end);
                if let Flow::Exception(exception) = &pending {
                    let message = exception.message();
                    let throwpoint = exception.throwpoint.clone();
                    for catch in &block.catches {
                        let matched = match catch.pattern.as_deref() {
                            None => true,
                            Some(pattern) => match regex_matches_catch_pattern(pattern, &message) {
                                Ok(value) => value,
                                Err(detail) => return error_flow(runtime, "E54", detail),
                            },
                        };
                        if matched {
                            let saved_exception = replace_scope_pair(
                                &mut scope.vim,
                                "exception",
                                Typval::String(OxStr::from(message.as_str())),
                            );
                            let saved_throwpoint = replace_scope_pair(
                                &mut scope.vim,
                                "throwpoint",
                                Typval::String(OxStr::from(throwpoint.as_str())),
                            );
                            pending = run_program(runtime, editor, scope, lua, program, catch.start, catch.end);
                            restore_scope_pair(&mut scope.vim, "exception", saved_exception);
                            restore_scope_pair(&mut scope.vim, "throwpoint", saved_throwpoint);
                            break;
                        }
                    }
                }
                if let Some((finally_start, finally_end)) = block.finally {
                    let final_flow = run_program(runtime, editor, scope, lua, program, finally_start, finally_end);
                    if !matches!(final_flow, Flow::Normal) {
                        pending = final_flow;
                    }
                }
                if !matches!(pending, Flow::Normal) {
                    return pending;
                }
                pc = block.end + 1;
                continue;
            }
            "function" => {
                let listed = command.args.trim().is_empty() || command.args.trim_start().starts_with('/');
                if listed {
                    let message_start = editor.messages().len();
                    let flow = command_function_list(runtime, editor, command);
                    if let Err(capture_flow) = capture_command_messages(runtime, editor, scope, command, message_start) {
                        return capture_flow;
                    }
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                    pc += 1;
                    continue;
                }
                let Some(block_end) = find_matching(program, pc, end, "function", "endfunction") else {
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
                let sid = runtime.scripts.current_sid().unwrap_or(0);
                let same_script_reload = runtime.functions.get(&signature.name, sid).is_some_and(|existing| {
                    existing.sid != sid
                        && runtime.scripts.current_name().is_some_and(|name| !name.starts_with('<'))
                        && runtime.scripts.script_name(existing.sid) == runtime.scripts.current_name()
                });
                let canonical = match runtime.functions.define(
                    signature,
                    body,
                    sid,
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
                    if let Some((_, current)) = data.entries.iter_mut().find(|(name, _)| name == &key) {
                        *current = value;
                    } else {
                        data.entries.push((key, value));
                    }
                }
                pc = block_end + 1;
                continue;
            }
            "elseif" | "else" | "endif" | "endwhile" | "endfor" | "catch" | "finally"
            | "endtry" | "endfunction" => {
                return error_flow(runtime, "E580", format!(":{name} without matching opener"));
            }
            _ => {}
        }

        let message_start = editor.messages().len();
        let flow = dispatch(runtime, editor, scope, lua, command);
        if let Err(capture_flow) = capture_command_messages(runtime, editor, scope, command, message_start) {
            return capture_flow;
        }
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
        pc += 1;
    }
    Flow::Normal
}

fn dispatch<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    // invalid_range runs in do_one_cmd before the command function, so every
    // EX_RANGE command is bounded whether or not it goes on to resolve its
    // addresses (ex_docmd.c:2209).
    if let Err(message) = check_address_domain(editor, command) {
        return error_flow(runtime, "E16", message);
    }
    let name = command.command.name();
    match name {
        "lua" => command_lua(runtime, editor, scope, lua, command),
        "luado" => command_luado(runtime, editor, scope, lua, command),
        "luafile" => command_luafile(runtime, editor, scope, lua, command),
        "let" => command_let(runtime, editor, scope, lua, &command.args, false),
        "const" => command_let(runtime, editor, scope, lua, &command.args, true),
        "unlet" => command_unlet(runtime, editor, scope, &command.args, command.bang),
        "delfunction" => command_delfunction(runtime, command),
        "set" => command_set(runtime, editor, scope, &command.args, SetLayer::Effective),
        "setlocal" => command_set(runtime, editor, scope, &command.args, SetLayer::Local),
        "setglobal" => command_set(runtime, editor, scope, &command.args, SetLayer::Global),
        "syntax" if matches!(command.args.trim(), "on" | "off") => Flow::Normal,
        "filetype" => command_filetype(runtime, editor, scope, lua, command),
        "insert" => Flow::Normal,
        "aunmenu" | "tlunmenu" if command.args.trim() == "*" => Flow::Normal,
        "echo" | "echomsg" | "echon" | "echoerr" => {
            command_echo(runtime, editor, scope, lua, name, &command.args)
        }
        "eval" => match eval_text(runtime, editor, scope, lua, command.args.trim()) { Ok(_) => Flow::Normal, Err(flow) => flow },
        "redir" => command_redir(runtime, editor, scope, command),
        "break" => Flow::Break,
        "continue" => Flow::Continue,
        "throw" => match eval_text(runtime, editor, scope, lua, command.args.trim()) {
            Ok(value) => Flow::Exception(VimException {
                kind: VimExceptionKind::Throw,
                value,
                throwpoint: runtime.throwpoint(),
            }),
            Err(flow) => flow,
        },
        "call" => command_call(runtime, editor, scope, lua, command),
        "return" => {
            if command.args.trim().is_empty() {
                Flow::Return(Typval::Number(0))
            } else {
                match eval_text(runtime, editor, scope, lua, command.args.trim()) {
                    Ok(value) => Flow::Return(value),
                    Err(flow) => flow,
                }
            }
        }
        "execute" => command_execute(runtime, editor, scope, lua, &command.args),
        "cd" => command_cd(runtime, editor, &command.args, false),
        "lcd" => command_cd(runtime, editor, &command.args, true),
        "swapname" => {
            push_text_message(editor, "No swap file".to_owned(), false, false);
            Flow::Normal
        }
        "source" => {
            let path = argument_path(editor, &command.args);
            match source_path(runtime, editor, scope, lua, &path, false) {
                Ok(Flow::Finish) => Flow::Normal,
                Ok(flow) => flow,
                Err(error) => exec_error_flow(runtime, error),
            }
        }
        "finish" if runtime.scripts.current_sid().is_some() => Flow::Finish,
        "finish" => error_flow(runtime, "E168", ":finish used outside of a sourced file"),
        "normal" => command_normal(runtime, editor, &command.args),
        "global" => command_global(runtime, editor, scope, lua, command, false),
        "vglobal" => command_global(runtime, editor, scope, lua, command, true),
        "substitute" => command_substitute(runtime, editor, scope, command),
        "edit" => command_edit(runtime, editor, command),
        "read" => command_read(runtime, editor, scope, lua, command),
        "enew" => command_enew(runtime, editor, command),
        "write" | "wq" | "xit" => {
            let flow = command_write(runtime, editor, scope, lua, command);
            if matches!(flow, Flow::Normal) && matches!(name, "wq" | "xit") {
                command_close(runtime, editor, command, true)
            } else {
                flow
            }
        }
        "split" | "new" => command_split(runtime, editor, command, false),
        "vsplit" | "vnew" => command_split(runtime, editor, command, true),
        "tabnew" | "tabedit" => command_tabnew(runtime, editor, command),
        "tabonly" => command_tabonly(runtime, editor, command),
        "undo" => command_undo(runtime, editor, command),
        "redo" => command_redo(runtime, editor),
        "retab" => command_retab(runtime, editor, scope, command),
        "hide" => command_hide(runtime, editor, command),
        "sleep" => command_sleep(runtime, editor, command),
        "scriptencoding" => command_scriptencoding(runtime, command),
        "argdelete" => command_argdelete(runtime, editor, command),
        "z" => command_z(runtime, editor, command),
        "lockvar" => command_lockvar(runtime, scope, command, true),
        "unlockvar" => command_lockvar(runtime, scope, command, false),
        "fold" => command_fold(runtime, editor, command),
        "foldopen" | "foldclose" => command_foldopen(runtime, editor, command),
        "resize" => command_resize(runtime, editor, command),
        "wincmd" => command_wincmd(runtime, editor, command),
        "echohl" => command_echohl(runtime, editor, command),
        "redraw" | "redrawstatus" | "redrawtabline" => command_redraw(runtime, editor),
        "close" => command_close(runtime, editor, command, false),
        "only" => command_only(runtime, editor),
        "quit" => command_close(runtime, editor, command, true),
        "qall" => command_qall(runtime, editor, command),
        "cquit" => command_cquit(command),
        "bnext" => command_buffer_step(runtime, editor, command, 1),
        "bprevious" | "bprev" => command_buffer_step(runtime, editor, command, -1),
        "buffer" => command_buffer(runtime, editor, command),
        "bwipeout" | "bwipe" => command_buffer_remove(runtime, editor, command, true),
        "bdelete" | "bdel" | "bunload" | "bun" => command_buffer_remove(runtime, editor, command, false),
        "args" => command_args(runtime, editor, command),
        "next" => command_next(runtime, editor, command),
        "previous" | "Next" => command_previous(runtime, editor, command),
        "argdo" => command_argdo(runtime, editor, scope, lua, command),
        "put" => command_put(runtime, editor, scope, lua, command),
        "print" => command_print(runtime, editor, command),
        "delete" => command_delete(runtime, editor, command),
        "yank" => command_yank(runtime, editor, command),
        "mark" | "k" => command_mark(runtime, editor, command),
        "marks" => command_marks(runtime, editor),
        "registers" | "display" => command_registers(runtime, editor, &command.args),
        "colorscheme" => command_colorscheme(runtime, editor, scope, lua, command),
        "language" => command_language(runtime, editor, scope, command),
        "highlight" => command_highlight(runtime, editor, command),
        "augroup" => command_augroup(runtime, editor, command),
        "autocmd" => command_autocmd(runtime, editor, command),
        "command" => command_user_command(runtime, editor, command),
        "comclear" => { runtime.user_commands.commands.clear(); Flow::Normal },
        "delcommand" => command_delcommand(runtime, command),
        "map" | "nmap" | "vmap" | "xmap" | "smap" | "omap" | "imap" | "cmap"
        | "lmap" | "tmap" | "noremap" | "nnoremap" | "vnoremap" | "xnoremap"
        | "snoremap" | "onoremap" | "inoremap" | "cnoremap" | "lnoremap"
        | "tnoremap" | "unmap" | "nunmap" | "vunmap" | "xunmap" | "sunmap"
        | "ounmap" | "iunmap" | "cunmap" | "lunmap" | "tunmap" | "mapclear"
        | "nmapclear" | "vmapclear" | "xmapclear" | "smapclear" | "omapclear"
        | "imapclear" | "cmapclear" | "lmapclear" | "tmapclear" => {
            command_map(runtime, editor, command)
        }
        _ => match &command.command {
            ResolvedCommand::User(user_name) => command_invoke_user(runtime, editor, scope, lua, user_name, command),
            ResolvedCommand::Builtin(spec) => Flow::NotImplemented(spec.name.to_owned()),
        },
    }
}

fn eval_text<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    text: &str,
) -> Result<Typval, Flow> {
    let expression = ExprParser::new(text.as_bytes())
        .parse()
        .map_err(|error| eval_error_flow(runtime, error))?;
    let regex = VimRegex;
    let ambiguous_wide = matches!(editor.options().get_global("ambiwidth"), Ok(OptionValue::String(value)) if value == "double");
    let mut host = EvalHost {
        runtime,
        editor,
        lua,
        builtins: Builtins::new(&regex).with_ambiguous_width(ambiguous_wide),
        submatches: None,
    };
    Evaluator::new(&mut host, &regex)
        .eval(&expression, scope)
        .map_err(|error| eval_error_flow(host.runtime, error))
}

fn eval_condition<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    text: &str,
) -> Result<bool, Flow> {
    let value = eval_text(runtime, editor, scope, lua, text)?;
    match value {
        Typval::Number(number) => Ok(number != 0),
        Typval::Bool(value) => Ok(value),
        Typval::String(value) => Ok(parse_number_prefix(&value.to_string_lossy()) != 0),
        Typval::Float(value) => Ok(value != 0.0),
        Typval::Channel(id) | Typval::Job(id) => Ok(id != 0),
        _ => Err(error_flow(runtime, "E745", "Using a List as a Number")),
    }
}

pub(crate) struct EvalHost<'a, F: FileIO> {
    pub(crate) runtime: &'a mut ExRuntime<F>,
    pub(crate) editor: &'a mut Editor,
    pub(crate) lua: Option<&'a Rc<RefCell<dyn LuaExec>>>,
    builtins: Builtins<'a>,
    pub(crate) submatches: Option<Vec<String>>,
}

impl<F: FileIO> BuiltinHost for EvalHost<'_, F> {
    fn call(
        &mut self,
        name: &OxStr,
        args: Vec<Typval>,
        scope: &mut Scope,
    ) -> ox_eval::Result<Typval> {
        let name_text = name.to_string_lossy();
        if let Some(family) = crate::builtins::route(&name_text) {
            return crate::builtins::call(self, family, &name_text, args, scope);
        }
        let sid = self
            .runtime
            .functions
            .active_sid()
            .or_else(|| self.runtime.scripts.current_sid())
            .unwrap_or(0);
        if self.runtime.functions.contains(&name_text, sid) || name_text.contains('#') {
            let (first, last) = current_line_pair(self.editor);
            return call_user_function(
                self.runtime,
                self.editor,
                scope,
                self.lua,
                &name.to_string_lossy(),
                args,
                first,
                last,
            )
                .map_err(|flow| flow_to_eval_error(flow, &name_text));
        }
        self.builtins.call(name, args, scope)
    }

    fn closure_registry(&self) -> Option<ox_eval::eval::ClosureRegistry> {
        Some(self.builtins.closure_registry().clone())
    }
}

fn command_resize<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let height = command.args.trim().parse::<usize>().unwrap_or(1).max(1);
    let Some(window) = editor.current_window() else { return error_flow(runtime, "E443", "Cannot rotate when another window is split"); };
    match editor.set_window_height(window, height) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E36", error.to_string()),
    }
}

fn command_wincmd<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let Some(key) = command.args.trim().chars().next() else { return error_flow(runtime, "E474", "Invalid argument"); };
    let Some(tab) = editor.current_tabpage() else { return Flow::Normal; };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let Some(current) = editor.current_window() else { return Flow::Normal; };
    let next = match key {
        'w' => windows.iter().position(|window| *window == current).and_then(|index| windows.get((index + 1) % windows.len())).copied(),
        'W' => windows.iter().position(|window| *window == current).and_then(|index| windows.get((index + windows.len() - 1) % windows.len())).copied(),
        'h' | 'j' | 'k' | 'l' => directional_window(editor, current, &windows, key),
        _ => return error_flow(runtime, "E474", format!("Invalid argument: {key}")),
    };
    match next.map(|window| editor.set_current_window(window)) {
        None | Some(Ok(())) => Flow::Normal,
        Some(Err(error)) => error_flow(runtime, "E957", error.to_string()),
    }
}

fn directional_window(editor: &Editor, current: WinHandle, windows: &[WinHandle], key: char) -> Option<WinHandle> {
    let origin = editor.window_geometry(current).ok()?;
    windows.iter().copied().filter(|window| *window != current).filter_map(|window| {
        let geometry = editor.window_geometry(window).ok()?;
        let distance = match key {
            'h' if geometry.col < origin.col => origin.col - geometry.col,
            'l' if geometry.col > origin.col => geometry.col - origin.col,
            'k' if geometry.row < origin.row => origin.row - geometry.row,
            'j' if geometry.row > origin.row => geometry.row - origin.row,
            _ => return None,
        };
        Some((distance, window))
    }).min_by_key(|(distance, _)| *distance).map(|(_, window)| window)
}

fn command_echohl<F: FileIO>(_runtime: &ExRuntime<F>, _editor: &mut Editor, _command: &ExCommand) -> Flow {
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
fn command_redraw<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor) -> Flow {
    let Some(window) = editor.current_window() else { return Flow::Normal };
    let Ok(state) = editor.window(window) else { return Flow::Normal };
    let cursor = state.cursor;
    let last = editor
        .buffer(state.buffer)
        .ok()
        .and_then(|buffer| buffer.text().ok())
        .map_or(1, Buffer::line_count);
    let clamped = cursor.lnum.clamp(1, last.max(1));
    if clamped == cursor.lnum {
        return Flow::Normal;
    }
    match editor.set_window_cursor(window, Position { lnum: clamped, col: cursor.col }) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E948", error.to_string()),
    }
}

/// Resolves a command's file argument, expanding a bare `%` to the current
/// buffer's name (`expand_filename`, `ex_docmd.c`).
fn argument_path(editor: &Editor, argument: &str) -> PathBuf {
    let argument = argument.trim();
    if argument != "%" {
        return PathBuf::from(argument);
    }
    editor
        .current_buffer()
        .and_then(|buffer| editor.buffer(buffer).ok())
        .map_or_else(|| PathBuf::from(argument), |buffer| PathBuf::from(buffer.name().to_string_lossy().into_owned()))
}

pub(crate) fn resolve_buffer_argument(editor: &Editor, argument: Option<&Typval>) -> Option<BufHandle> {
    match argument {
        None => editor.current_buffer(),
        Some(Typval::Number(0)) => editor.current_buffer(),
        Some(Typval::Number(number)) => BufHandle::try_from(*number)
            .ok()
            .filter(|buffer| editor.buffer(*buffer).is_ok()),
        Some(Typval::String(name)) if name.as_bytes().is_empty() || name.as_bytes() == b"%" => editor.current_buffer(),
        Some(Typval::String(name)) => editor
            .buffers()
            .into_iter()
            .find(|buffer| editor.buffer(*buffer).is_ok_and(|state| state.name() == name)),
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
        let Some(buffer) = self.0.current_buffer() else { return Ok(0); };
        Ok(self
            .0
            .buffer(buffer)
            .ok()
            .and_then(|state| state.text().ok())
            .map_or(0, Buffer::line_count))
    }

    fn get_line(&self, lnum: usize) -> ox_eval::Result<Option<OxStr>> {
        let Some(buffer) = self.0.current_buffer() else { return Ok(None); };
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
            .replace_buffer_lines(buffer, lnum, lnum, &[text.as_bytes().to_vec()], cursor, cursor, 0)
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
        let cursor = self.cursor_or(Position { lnum: after + 1, col: 0 });
        self.0
            .append_buffer_lines(buffer, after, &[text.as_bytes().to_vec()], cursor, 0)
            .map(|_| ())
            .map_err(|error| EvalError::new("E16", 0, error.to_string()))
    }

    /// `var2fpos` for string lnum arguments: `"."` is the current window's
    /// cursor line, `"'x"` the mark position (buffer-local first, then the
    /// uppercase/numbered global marks, like `getmark`).
    fn address_line(&self, address: &str) -> ox_eval::Result<Option<i64>> {
        let mut chars = address.chars();
        match chars.next() {
            Some('.') if chars.next().is_none() => {
                Ok(Some(self.cursor_or(Position { lnum: 1, col: 0 }).lnum as i64))
            }
            Some('\'') => {
                let Some(name) = chars.next() else { return Ok(None) };
                let Some(buffer) = self.0.current_buffer() else { return Ok(None) };
                let local = self
                    .0
                    .local_mark(buffer, name)
                    .map_err(|error| EvalError::new("E749", 0, error.to_string()))?
                    .map(|position| position.lnum);
                if let Some(line) = local {
                    return Ok(Some(line as i64));
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
                Ok(global.map(|line| line as i64))
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
        Ok(ox_regex::exec(&program, &RegexText::new(text.to_string_lossy().into_owned())).is_some())
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
        let Some(position) = text.position(start) else { return Ok(None) };
        let found = regex_exec_at(
            &program,
            &text,
            position,
        );
        Ok(found.map(|matched| (matched.start.byte, matched.end.byte)))
    }

    fn find_captures(
        &self,
        text: &OxStr,
        pattern: &OxStr,
        start: usize,
    ) -> ox_eval::Result<Option<ox_eval::RegexMatch>> {
        let source = text.to_string_lossy().into_owned();
        let program = compile_regex(&pattern.to_string_lossy(), Magic::Magic)
            .map_err(|error| match error {
                RegexCompileError::Syntax { message: "lookaround suffix follows nothing", .. } => {
                    EvalError::new("E866", 0, "(NFA regexp) Misplaced @")
                }
                other => EvalError::new("E54", 0, other.to_string()),
            })?;
        let text = RegexText::new(source);
        let Some(position) = text.position(start) else { return Ok(None) };
        Ok(regex_exec_at(&program, &text, position).map(|matched| ox_eval::RegexMatch {
            start: matched.start.byte,
            end: matched.end.byte,
            captures: matched.captures.into_iter().map(|capture| capture.map(|capture| (capture.start.byte, capture.end.byte))).collect(),
        }))
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
            let Some(position) = regex_text.position(cursor) else { break };
            let Some(matched) = regex_exec_at(
                &program,
                &regex_text,
                position,
            ) else {
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

fn call_user_function<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    args: Vec<Typval>,
    first_line: usize,
    last_line: usize,
) -> Result<Typval, Flow> {
    call_user_function_with_self(
        runtime, editor, scope, lua, name, args, first_line, last_line, None,
    )
}

pub(crate) fn call_user_function_with_self<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    mut args: Vec<Typval>,
    first_line: usize,
    last_line: usize,
    receiver: Option<DictRef>,
) -> Result<Typval, Flow> {
    let mut sid = runtime
        .functions
        .active_sid()
        .or_else(|| runtime.scripts.current_sid())
        .unwrap_or(0);
    if !runtime.functions.contains(name, sid) && name.contains('#') {
        let path = runtime.scripts.resolve_autoload(name).ok_or_else(|| {
            error_flow(runtime, "E117", format!("Unknown function: {name}"))
        })?;
        if !runtime.scripts.is_sourced_once(&path) {
            let flow = source_path(runtime, editor, scope, lua, &path, true)
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
            match eval_text(runtime, editor, scope, lua, expression) {
                Ok(value) => args.push(value),
                Err(flow) => return Err(flow),
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
    let parsed = parse_program(&runtime.user_commands, &logical)
        .map_err(|error| exec_error_flow(runtime, error))?;
    let function = runtime
        .functions
        .begin_call(name, sid, args, first_line, last_line, scope)
        .map_err(|error| userfunc_error_flow(runtime, error))?;
    if let Some(receiver) = receiver {
        scope.local.push((OxStr::from("self"), Typval::Dict(receiver)));
    }
    let switched_script = function.sid != 0 && runtime.scripts.current_sid() != Some(function.sid);
    let caller_script = scope.script.clone();
    if switched_script {
        runtime.scripts.load_script_scope(function.sid, scope);
    }
    let flow = run_program(runtime, editor, scope, lua, &parsed, 0, parsed.len());
    if switched_script {
        runtime.scripts.store_script_scope(function.sid, scope);
        scope.script = caller_script;
    }
    let flow = match flow {
        Flow::NotImplemented(name) => {
            error_flow(runtime, "E117", format!("not implemented: {name}"))
        }
        flow => flow,
    };
    runtime.functions.end_call(scope);
    match flow {
        Flow::Normal => Ok(Typval::Number(0)),
        Flow::Return(value) => Ok(value),
        flow => Err(flow),
    }
}

fn source_path<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
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
    let name = runtime.scripts.io().canonicalize(path).display().to_string();
    let caller_script = scope.script.clone();
    let sid = runtime.scripts.push_source(name);
    let lines = expand_script_lines(&runtime.scripts, lines, sid);
    runtime.scripts.load_script_scope(sid, scope);
    if load_once {
        runtime.scripts.mark_sourced_once(path);
    }
    let program = parse_program(&runtime.user_commands, &lines);
    let flow = match program {
        Ok(program) => run_program(runtime, editor, scope, lua, &program, 0, program.len()),
        Err(error) => exec_error_flow(runtime, error),
    };
    runtime.scripts.store_script_scope(sid, scope);
    runtime.scripts.pop_source();
    scope.script = caller_script;
    Ok(flow)
}

fn command_let<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: &str,
    constant: bool,
) -> Flow {
    let Some((target, operator, expression)) = split_assignment(args) else {
        return error_flow(runtime, "E121", format!("Undefined variable: {}", args.trim()));
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
        match eval_text(runtime, editor, scope, lua, strip_expression_comment(expression)) {
            Ok(value) => value,
            Err(flow) => return flow,
        }
    };
    let key = canonical_target(target);
    if runtime.const_vars.contains(&key) {
        return error_flow(runtime, "E46", format!("Cannot change read-only variable \"{target}\""));
    }
    let assigned = if operator == "=" {
        value
    } else {
        let previous = match read_target(runtime, editor, scope, target) {
            Ok(value) => value,
            Err(flow) => return flow,
        };
        let combined = if target.trim_start().starts_with('&') {
            apply_option_assignment_operator(runtime, previous, value, operator)
        } else {
            apply_assignment_operator(runtime, previous, value, operator)
        };
        match combined {
            Ok(value) => value,
            Err(flow) => return flow,
        }
    };
    if let Err(flow) = assign_target(runtime, editor, scope, target, assigned, constant) {
        return flow;
    }
    if constant {
        runtime.const_vars.insert(key);
    }
    Flow::Normal
}

fn command_unlet<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    args: &str,
    bang: bool,
) -> Flow {
    for target in args.split_ascii_whitespace() {
        let key = canonical_target(target);
        if runtime.const_vars.contains(&key) {
            return error_flow(runtime, "E46", format!("Cannot change read-only variable \"{target}\""));
        }
        let removed = remove_target(editor, scope, target);
        if !removed && !bang {
            return error_flow(runtime, "E108", format!("No such variable: \"{target}\""));
        }
    }
    Flow::Normal
}

fn command_delfunction<F: FileIO>(runtime: &mut ExRuntime<F>, command: &ExCommand) -> Flow {
    let name = command.args.trim();
    if name.is_empty() { return error_flow(runtime, "E471", "Argument required"); }
    if name.split_whitespace().count() != 1 { return error_flow(runtime, "E488", "Trailing characters"); }
    let sid = runtime.functions.active_sid().or_else(|| runtime.scripts.current_sid()).unwrap_or(0);
    if runtime.functions.is_active(name, sid) {
        return error_flow(runtime, "E131", format!("Cannot delete function {name}: It is in use"));
    }
    if runtime.functions.remove(name, sid) || command.bang { Flow::Normal }
    else { error_flow(runtime, "E130", format!("Unknown function: {name}")) }
}

#[derive(Clone, Copy)]
enum SetLayer {
    Effective,
    Local,
    Global,
}

fn command_set<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    args: &str,
    layer: SetLayer,
) -> Flow {
    if args.trim().is_empty() || args.trim() == "all" {
        let all = args.trim() == "all";
        for metadata in OPTION_METADATA {
            if !all && option_is_default(editor, metadata.name) {
                continue;
            }
            if let Some(text) = display_option(editor, metadata.name, layer) {
                push_info_text_message(editor, text);
            }
        }
        return Flow::Normal;
    }
    let mut touched_runtimepath = false;
    for raw in split_set_args(args) {
        if let Err((code, message)) = set_one(editor, scope, &raw, layer) {
            return error_flow(runtime, code, message);
        }
        touched_runtimepath |= set_arg_targets(&raw);
    }
    if touched_runtimepath {
        sync_runtime_roots(runtime, editor);
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
    if let Ok(OptionValue::String(rtp)) = editor.options().get_global("runtimepath") {
        if !rtp.is_empty() {
            runtime.scripts.set_runtime_roots_from_rtp(rtp);
        }
    }
}

fn command_echo<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    args: &str,
) -> Flow {
    if let Ok(value) = eval_text(runtime, editor, scope, lua, args) {
        push_text_message(
            editor,
            typval_to_display(&value, false),
            name == "echoerr",
            name == "echomsg",
        );
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
            editor,
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
    push_text_message(editor, text, name == "echoerr", name == "echomsg");
    Flow::Normal
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
            ox_regex::exec(compiled, &RegexText::new(name.to_owned())).is_none()
        }) {
            continue;
        }
        let required = function.args.len().saturating_sub(function.default_args.len());
        let mut arguments = Vec::with_capacity(function.args.len() + usize::from(function.varargs));
        for (index, argument) in function.args.iter().enumerate() {
            if index < required {
                arguments.push(argument.clone());
            } else {
                arguments.push(format!("{argument} = {}", function.default_args[index - required]));
            }
        }
        if function.varargs {
            arguments.push("...".to_owned());
        }
        let mut signature = format!("function {name}({})", arguments.join(", "));
        if function.flags.abort { signature.push_str(" abort"); }
        if function.flags.range { signature.push_str(" range"); }
        if function.flags.dict { signature.push_str(" dict"); }
        if function.flags.closure { signature.push_str(" closure"); }
        push_text_message(editor, signature, false, false);
    }
    Flow::Normal
}

fn command_redir<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    command: &ExCommand,
) -> Flow {
    let argument = command.args.trim();
    if argument.eq_ignore_ascii_case("END") {
        let Some(redirection) = runtime.redirection.take() else { return Flow::Normal };
        return finish_redirection(runtime, editor, scope, redirection);
    }
    if runtime.redirection.is_some() {
        return error_flow(runtime, "E930", "Cannot use :redir while redirection is active");
    }

    let target = if let Some(register) = argument.strip_prefix('@') {
        let mut chars = register.chars();
        let Some(written_name) = chars.next() else { return error_flow(runtime, "E474", "Invalid argument") };
        let suffix = chars.as_str();
        if !matches!(suffix, "" | ">" | ">>") || written_name == '_' {
            return error_flow(runtime, "E474", format!("Invalid argument: {argument}"));
        }
        let append = written_name.is_ascii_uppercase() || suffix == ">>";
        let name = written_name.to_ascii_lowercase();
        if editor.registers().get(name).is_err() {
            return error_flow(runtime, "E474", format!("Invalid argument: {argument}"));
        }
        if !append {
            let empty = match RegisterContent::characterwise(&[]) {
                Ok(content) => content,
                Err(error) => return error_flow(runtime, "E354", error.to_string()),
            };
            if let Err(error) = editor.registers_mut().set(name, empty) {
                return error_flow(runtime, "E354", error.to_string());
            }
            scope.set_register(&[name as u8], Typval::String(OxStr::from("")));
        }
        RedirTarget::Register { name }
    } else if let Some(variable) = argument.strip_prefix("=>>").map(str::trim) {
        if variable.is_empty() || variable.starts_with(['@', '$', '&']) {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        match read_target(runtime, editor, scope, variable) {
            Ok(Typval::String(_)) => {}
            Ok(_) => return error_flow(runtime, "E734", "Wrong variable type for .="),
            Err(flow) => return flow,
        }
        RedirTarget::Variable { name: variable.to_owned(), append: true }
    } else if let Some(variable) = argument.strip_prefix("=>").map(str::trim) {
        if variable.is_empty() || variable.starts_with(['@', '$', '&']) {
            return error_flow(runtime, "E474", "Invalid argument");
        }
        if let Err(flow) = assign_target(runtime, editor, scope, variable, Typval::String(OxStr::from("")), false) {
            return flow;
        }
        RedirTarget::Variable { name: variable.to_owned(), append: false }
    } else if let Some(file) = argument.strip_prefix(">>").map(str::trim) {
        if file.is_empty() { return error_flow(runtime, "E474", "Invalid argument") }
        let path = PathBuf::from(expand_env_esc(file));
        if let Err(error) = runtime.scripts.io().write_bytes(&path, &[], true) {
            return error_flow(runtime, "E484", format!("{}: {error}", path.display()));
        }
        RedirTarget::File { path }
    } else if let Some(file) = argument.strip_prefix('>').map(str::trim) {
        if file.is_empty() { return error_flow(runtime, "E474", "Invalid argument") }
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
        seen_messages: editor.messages().len(),
    });
    Flow::Normal
}

fn finish_redirection<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    redirection: Redirection,
) -> Flow {
    match redirection.target {
        RedirTarget::Register { .. } | RedirTarget::File { .. } => Flow::Normal,
        RedirTarget::Variable { name, append } => {
            let output = if append {
                match read_target(runtime, editor, scope, &name) {
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
            match assign_target(runtime, editor, scope, &name, Typval::String(OxStr::from(output.as_str())), false) {
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
    let silent = command.modifiers.iter().any(|modifier| modifier.kind == ModifierKind::Silent);
    let mut write = None;
    if let Some(redirection) = runtime.redirection.as_mut() {
        let start = redirection.seen_messages.max(command_start).min(editor.messages().len());
        let mut chunk = String::new();
        for (index, message) in editor.messages()[start..].iter().enumerate() {
            let Object::String(text) = &message.content else { continue };
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
                .map_err(|error| error_flow(runtime, "E484", format!("{}: {error}", path.display())))?,
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

fn command_call<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let text = command.args.trim();
    let Some(open) = text.find('(') else {
        return error_flow(runtime, "E107", "Missing parentheses: :call");
    };
    let Some(close) = text.rfind(')') else {
        return error_flow(runtime, "E107", "Missing parentheses: :call");
    };
    // ex_call: only text that `ends_excmd` rejects is trailing — a `"`
    // comment (with or without leading whitespace) ends the command.
    let trailing = text[close + 1..].trim_start();
    if !trailing.is_empty() && !trailing.starts_with('"') {
        return error_flow(runtime, "E488", "Trailing characters");
    }
    let name = text[..open].trim();
    let sid = runtime.scripts.current_sid().unwrap_or(0);
    // Builtin callees evaluate as call expressions through the expression
    // evaluator, which routes the editor seams (buffer builtins, regex,
    // user functions) the same way `:let` does; upstream `:call` reaches
    // builtins through the same eval machinery (`eval/userfunc.c`
    // `call_func`). Unknown names keep the registry's E117 below.
    if !runtime.functions.contains(name, sid)
        && !name.contains('#')
        && ox_eval::builtin_spec(name).is_some()
    {
        let (first, last) = resolve_range(editor, command).unwrap_or_else(|_| current_line_pair(editor));
        let addressed = if command.range.is_none() { first..=first } else { first..=last };
        for lnum in addressed {
            if command.range.is_some() {
                if let Some(window) = editor.current_window() {
                    if let Err(error) = editor.set_window_cursor(window, Position { lnum, col: 0 }) {
                        return error_flow(runtime, "E16", error.to_string());
                    }
                }
            }
            if let Err(flow) = eval_text(runtime, editor, scope, lua, &text[..=close]) {
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
        match eval_text(runtime, editor, scope, lua, arg) {
            Ok(value) => values.push(value),
            Err(flow) => return flow,
        }
    }
    let (first, last) = resolve_range(editor, command).unwrap_or_else(|_| current_line_pair(editor));
    let accepts_range = runtime
        .functions
        .get(name, sid)
        .is_some_and(|function| function.flags.range);
    if command.range.is_none() || accepts_range {
        return match call_user_function(runtime, editor, scope, lua, name, values, first, last) {
            Ok(_) => Flow::Normal,
            Err(flow) => flow,
        };
    }

    for lnum in first..=last {
        if let Some(window) = editor.current_window() {
            if let Err(error) = editor.set_window_cursor(window, Position { lnum, col: 0 }) {
                return error_flow(runtime, "E16", error.to_string());
            }
        }
        if let Err(flow) = call_user_function(
            runtime,
            editor,
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

fn command_execute<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
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
            editor,
            scope,
            lua,
            &args[expression.span.start..expression.span.end],
        ) {
            Ok(value) => pieces.push(typval_to_text(&value)),
            Err(flow) => return flow,
        }
    }
    let line = pieces.join(" ");
    let logical = vec![LogicalLine { text: line, first_line: runtime.scripts.current_line() }];
    let program = match parse_program(&runtime.user_commands, &logical) {
        Ok(program) => program,
        Err(error) => return exec_error_flow(runtime, error),
    };
    run_program(runtime, editor, scope, lua, &program, 0, program.len())
}

fn command_cd<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &Editor, args: &str, local: bool) -> Flow {
    let path = args.trim();
    if path.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    match change_directory(runtime, editor, path, local) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, error.code, error.message),
    }
}

fn directory_target<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &Editor, path: &str) -> ox_eval::Result<PathBuf> {
    if path == "-" {
        return runtime.previous_dir.clone().ok_or_else(|| EvalError::new("E186", 0, "No previous directory"));
    }
    let direct = PathBuf::from(path);
    if direct.is_absolute() || direct.is_dir() { return Ok(direct); }
    if let Ok(OptionValue::String(cdpath)) = editor.options().get_global("cdpath") {
        for entry in cdpath.split(',') {
            let base = if entry.is_empty() { Path::new(".") } else { Path::new(entry) };
            let candidate = base.join(path);
            if candidate.is_dir() { return Ok(candidate); }
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
pub(crate) fn change_directory<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &Editor, path: &str, local: bool) -> ox_eval::Result<PathBuf> {
    let previous = std::env::current_dir().ok();
    let target = directory_target(runtime, editor, path)?;
    std::env::set_current_dir(&target).map_err(|error| EvalError::new("E344", 0, format!("Can't find directory {path}: {error}")))?;
    runtime.previous_dir = previous.clone();
    if local {
        let window = editor.current_window().ok_or_else(|| EvalError::new("E16", 0, "No current window"))?;
        runtime.local_dir = previous.clone().map(|directory| (window, directory));
    } else {
        runtime.local_dir = None;
    }
    Ok(previous.unwrap_or_default())
}

fn command_normal<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, args: &str) -> Flow {
    let mut machine = ModeMachine::default();
    match machine.feed_keys(editor, args.trim_start()) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E523", error.to_string()),
    }
}

fn command_global<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
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
    let nested = if rest.trim().is_empty() { "print" } else { rest.trim() };
    let (start, end) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let buffer = match editor.current_buffer() {
        Some(buffer) => buffer,
        None => return error_flow(runtime, "E749", "Empty buffer"),
    };
    let program_regex = match compile_regex(&pattern, Magic::Magic) {
        Ok(program) => program,
        Err(error) => return error_flow(runtime, "E54", error.to_string()),
    };
    let lines = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let mut marked = Vec::new();
    let namespace = match editor.buffer_mut(buffer).and_then(|state| {
        state
            .extmarks
            .create_namespace("")
            .map_err(|error| crate::EditorError::Buffer(error.into()))
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
            placement.attributes = ExtmarkAttributes { invalidate: true, ..ExtmarkAttributes::default() };
            let id = match editor.buffer_mut(buffer).and_then(|state| {
                state
                    .extmarks
                    .set(namespace, None, placement)
                    .map_err(|error| crate::EditorError::Buffer(error.into()))
            }) {
                Ok(id) => id,
                Err(error) => {
                    cleanup_global_marks(editor, buffer, namespace, &marked);
                    return error_flow(runtime, "E16", error.to_string());
                }
            };
            marked.push(id);
        }
    }
    for id in marked.iter().copied() {
        let target = match editor.buffer(buffer).and_then(|state| {
            state
                .extmarks
                .get(namespace, id)
                .map(|mark| mark.filter(|mark| !mark.invalid).map(|mark| mark.placement.position.row + 1))
                .map_err(|error| crate::EditorError::Buffer(error.into()))
        }) {
            Ok(target) => target,
            Err(error) => {
                cleanup_global_marks(editor, buffer, namespace, &marked);
                return error_flow(runtime, "E16", error.to_string());
            }
        };
        if let Ok(state) = editor.buffer_mut(buffer) {
            let _ = state.extmarks.delete(namespace, id);
        }
        let Some(lnum) = target else { continue };
        if let Some(window) = editor.current_window() {
            if let Err(error) = editor.set_window_cursor(window, Position { lnum, col: 0 }) {
                cleanup_global_marks(editor, buffer, namespace, &marked);
                return error_flow(runtime, "E16", error.to_string());
            }
        }
        let logical = vec![LogicalLine { text: nested.to_owned(), first_line: runtime.scripts.current_line() }];
        let program = match parse_program(&runtime.user_commands, &logical) {
            Ok(program) => program,
            Err(error) => {
                cleanup_global_marks(editor, buffer, namespace, &marked);
                return exec_error_flow(runtime, error);
            }
        };
        let flow = run_program(runtime, editor, scope, lua, &program, 0, program.len());
        if !matches!(flow, Flow::Normal) {
            cleanup_global_marks(editor, buffer, namespace, &marked);
            return flow;
        }
    }
    cleanup_global_marks(editor, buffer, namespace, &marked);
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

fn command_substitute<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
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
    let Some((replacement, flags)) = take_delimited(&replacement_input, delimiter) else {
        return error_flow(runtime, "E488", "Trailing characters");
    };
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
    let (start, end) = match resolve_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    let buffer = match editor.current_buffer() {
        Some(buffer) => buffer,
        None => return error_flow(runtime, "E749", "Empty buffer"),
    };
    let original = match buffer_lines(editor, buffer) {
        Ok(lines) => lines,
        Err(message) => return error_flow(runtime, "E749", message),
    };
    let program_regex = match compile_regex(&compiled_pattern, Magic::Magic) {
        Ok(program) => program,
        Err(error) => return error_flow(runtime, "E54", error.to_string()),
    };
    let mut replacement_lines = Vec::new();
    let mut changed = false;
    for lnum in start..=end.min(original.len()) {
        let source = String::from_utf8_lossy(&original[lnum - 1]).into_owned();
        match substitute_line(runtime, editor, scope, &program_regex, &source, &replacement, expression, global) {
            Ok((line, did_change)) => {
                changed |= did_change;
                replacement_lines.push(line.into_bytes());
            }
            Err(flow) => return flow,
        }
    }
    if !changed && !suppress_nomatch {
        return error_flow(runtime, "E486", format!("Pattern not found: {pattern}"));
    }
    if changed {
        let cursor = editor.current_window().and_then(|window| editor.window(window).ok()).map_or(
            Position { lnum: start, col: 0 },
            |window| window.cursor,
        );
        if let Err(error) = editor.replace_buffer_lines(buffer, start, end, &replacement_lines, cursor, cursor, 0) {
            return error_flow(runtime, "E16", error.to_string());
        }
    }
    Flow::Normal
}

fn substitute_line<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    program: &ox_regex::Prog,
    source: &str,
    replacement: &str,
    expression: Option<&str>,
    global: bool,
) -> Result<(String, bool), Flow> {
    let text = RegexText::new(source.to_owned());
    let mut output = String::new();
    let mut previous = 0;
    let mut cursor = 0;
    let mut changed = false;
    while cursor <= source.len() {
        let Some(position) = text.position(cursor) else { break };
        let Some(matched) = regex_exec_at(
            program,
            &text,
            position,
        ) else {
            break;
        };
        output.push_str(&source[previous..matched.start.byte]);
        let mut groups = vec![source[matched.start.byte..matched.end.byte].to_owned()];
        for capture in &matched.captures {
            groups.push(capture.as_ref().map_or_else(String::new, |capture| {
                source[capture.start.byte..capture.end.byte].to_owned()
            }));
        }
        let rendered = if let Some(expression) = expression {
            eval_substitute_expression(runtime, editor, scope, expression, groups)?
        } else {
            expand_replacement(replacement, &groups)
        };
        output.push_str(&rendered);
        changed = true;
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
    if !changed {
        return Ok((source.to_owned(), false));
    }
    output.push_str(&source[previous..]);
    Ok((output, true))
}

fn eval_substitute_expression<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
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
        editor,
        lua: None,
        builtins: Builtins::new(&regex),
        submatches: Some(groups),
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
const DEFAULT_TABPAGE_GEOMETRY: crate::Geometry =
    crate::Geometry { row: 0, col: 0, width: 80, height: 24 };

/// Loads `path` into a fresh listed buffer named after it, saved-clean.
///
/// A missing file is not an error: upstream's `:edit`/`:split`/`:tabedit` open
/// an empty buffer for a name that does not exist yet. Shared by every command
/// that opens a file into a new buffer so the read, the name, and the
/// saved-state marking have one owner.
fn buffer_from_file<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    path: &std::path::Path,
) -> Result<BufHandle, Flow> {
    let text = match runtime.scripts.io().read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error_flow(runtime, "E484", format!("Can't open file {}: {error}", path.display())));
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
    Ok(handle)
}

fn command_edit<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let path = PathBuf::from(command.args.trim());
    if path.as_os_str().is_empty() {
        return error_flow(runtime, "E32", "No file name");
    }
    if let Some(current) = editor.current_buffer() {
        if editor.buffer(current).is_ok_and(|buffer| buffer.modified) && !command.bang {
            return error_flow(runtime, "E37", "No write since last change (add ! to override)");
        }
    }
    let handle = match buffer_from_file(runtime, editor, &path) {
        Ok(handle) => handle,
        Err(flow) => return flow,
    };
    if editor.current_window().is_none() {
        match editor.create_tabpage(handle, DEFAULT_TABPAGE_GEOMETRY) {
            Ok(_) => {}
            Err(error) => return error_flow(runtime, "E948", error.to_string()),
        }
    } else if let Err(error) = editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
        return error_flow(runtime, "E948", error.to_string());
    }
    Flow::Normal
}
fn command_enew<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    if let Some(current) = editor.current_buffer() {
        if editor.buffer(current).is_ok_and(|buffer| buffer.modified) && !command.bang {
            return error_flow(runtime, "E37", "No write since last change (add ! to override)");
        }
    }
    let handle = match editor.create_buffer(true) {
        Ok(handle) => handle,
        Err(error) => return error_flow(runtime, "E948", error.to_string()),
    };
    if editor.current_window().is_none() {
        if let Err(error) = editor.create_tabpage(handle, Geometry { row: 0, col: 0, width: 80, height: 24 }) {
            return error_flow(runtime, "E948", error.to_string());
        }
    } else if let Err(error) = editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
        return error_flow(runtime, "E948", error.to_string());
    }
    Flow::Normal
}

/// `:bwipeout`/`:bdelete` (`ex_cmds.c` ex_bwipe/ex_bdelete): resolve the
/// buffer from the count or argument (defaulting to the current buffer),
/// move displaying windows onto another buffer, then wipe or unload it.
/// The modified-buffer guard matches `do_buffer`'s E89.
fn command_buffer_remove<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    command: &ExCommand,
    wipe: bool,
) -> Flow {
    let requested = command
        .count
        .and_then(|value| i64::try_from(value).ok())
        .or_else(|| command.args.trim().parse::<i64>().ok());
    let target = match requested.and_then(|value| BufHandle::try_from(value).ok()) {
        Some(handle) => handle,
        None => match editor.current_buffer() {
            Some(handle) => handle,
            None => return error_flow(runtime, "E85", "There is no listed buffer"),
        },
    };
    if editor.buffer(target).is_err() {
        return error_flow(runtime, "E86", format!("Buffer {} does not exist", i64::from(target)));
    }
    if !command.bang && editor.buffer(target).is_ok_and(|state| state.modified) {
        return error_flow(runtime, "E89", "No write since last change (add ! to override)");
    }
    let mut attached = Vec::new();
    for window in editor.windows() {
        if editor.window(window).is_ok_and(|state| state.buffer == target) {
            attached.push(window);
        }
    }
    if !attached.is_empty() {
        let replacement = match editor.buffers().into_iter().find(|&buffer| buffer != target) {
            Some(other) => other,
            None => match editor.create_buffer(true) {
                Ok(handle) => handle,
                Err(error) => return error_flow(runtime, "E948", error.to_string()),
            },
        };
        for window in attached {
            if let Err(error) = editor.set_window_buffer(window, replacement, BufferRelease::KeepLoaded) {
                return error_flow(runtime, "E948", error.to_string());
            }
        }
    }
    if !wipe {
        // `:bdelete` keeps the buffer loaded but removes it from the list.
        if let Ok(state) = editor.buffer_mut(target) {
            state.listed = false;
        }
        return match editor.unload_buffer(target) {
            Ok(()) => Flow::Normal,
            Err(error) => error_flow(runtime, "E90", error.to_string()),
        };
    }
    match editor.wipe_buffer(target) {
        Ok(_) => {
            if runtime.local_dir.as_ref().is_some_and(|(window, _)| Some(*window) == editor.current_window()) {
                if let Some((_, directory)) = runtime.local_dir.take() { let _ = std::env::set_current_dir(directory); }
            }
            Flow::Normal
        }
        Err(error) => error_flow(runtime, "E90", error.to_string()),
    }
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
fn command_read<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let Some(buffer) = editor.current_buffer() else {
        return error_flow(runtime, "E749", "Empty buffer");
    };
    let after = match resolve_range_raw(editor, command) {
        Ok((_, end)) => end.min(buffer_last_line(editor)),
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
        let flow = fire_read_autocmd(runtime, editor, scope, lua, Event::FilterReadPre, None);
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
        output
    } else {
        let name = command.args.trim();
        let path = if name.is_empty() {
            let existing = match editor.buffer(buffer) {
                Ok(state) => state.name().to_string_lossy().into_owned(),
                Err(error) => return error_flow(runtime, "E32", error.to_string()),
            };
            if existing.is_empty() {
                return error_flow(runtime, "E32", "No file name");
            }
            PathBuf::from(existing)
        } else {
            argument_path(editor, name)
        };
        let name = path.to_string_lossy().into_owned();
        // FileReadCmd intercepts: when a definition matches, it does the read
        // itself and this command performs none of its own work.
        let plan = editor.autocmds_mut().plan(
            Event::FileReadCmd,
            AutocmdContext { buffer: None, file_name: Some(&name), ..AutocmdContext::default() },
        );
        if !plan.ready.is_empty() {
            return run_autocmd_plan(runtime, editor, scope, lua, plan);
        }
        let flow = fire_read_autocmd(runtime, editor, scope, lua, Event::FileReadPre, Some(&name));
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
        let bytes = match runtime.scripts.io().read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                return error_flow(runtime, "E484", format!("Can't open file {}", path.display()));
            }
        };
        matched = Some(name);
        split_read_lines(&bytes)
    };

    if !lines.is_empty() {
        let window = editor.current_window();
        let cursor = window
            .and_then(|window| editor.window(window).ok())
            .map_or(Position { lnum: after.max(1), col: 0 }, |state| state.cursor);
        if let Err(error) = editor.append_buffer_lines(buffer, after, &lines, cursor, 0) {
            return error_flow(runtime, "E484", error.to_string());
        }
        let target = if command.usefilter { after + lines.len() } else { after + 1 };
        let column = lines
            .get(if command.usefilter { lines.len() - 1 } else { 0 })
            .map_or(0, |line| line.iter().take_while(|byte| matches!(byte, b' ' | b'\t')).count());
        if let Some(window) = window {
            if let Err(error) = editor.set_window_cursor(window, Position { lnum: target, col: column }) {
                return error_flow(runtime, "E484", error.to_string());
            }
        }
    }

    // Both post events fire even for an empty read: upstream's readfile runs
    // them on the way out regardless of how many lines arrived.
    let post = if command.usefilter { Event::FilterReadPost } else { Event::FileReadPost };
    let flow = fire_read_autocmd(runtime, editor, scope, lua, post, matched.as_deref());
    if !matches!(flow, Flow::Normal) || !command.usefilter {
        return flow;
    }
    fire_shell_filter_post(runtime, editor, scope, lua)
}

/// Runs one `readfile` autocommand event.
///
/// `matched` is the file name upstream matches the pattern against for the
/// `FileRead*` events, which pass `sfname` with a null buffer
/// (`fileio.c:336,640,1925`). The `Filter*` events pass a null name and
/// `curbuf` instead, so they match the current buffer's name, as
/// `:help FilterReadPre` documents.
fn fire_read_autocmd<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    event: Event,
    matched: Option<&str>,
) -> Flow {
    let (buffer, name) = match matched {
        Some(name) => (None, name.to_owned()),
        None => (editor.current_buffer(), current_buffer_name(editor)),
    };
    let plan = editor.autocmds_mut().plan(
        event,
        AutocmdContext { buffer, file_name: Some(&name), ..AutocmdContext::default() },
    );
    run_autocmd_plan(runtime, editor, scope, lua, plan)
}

/// `ShellFilterPost`, which `do_bang` applies after every ranged `do_filter`
/// run whether or not the filter produced output (`ex_cmds.c:1236`).
fn fire_shell_filter_post<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
) -> Flow {
    let name = current_buffer_name(editor);
    let buffer = editor.current_buffer();
    let plan = editor.autocmds_mut().plan(
        Event::ShellFilterPost,
        AutocmdContext { buffer, file_name: Some(&name), ..AutocmdContext::default() },
    );
    run_autocmd_plan(runtime, editor, scope, lua, plan)
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
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
    let output = match std::process::Command::new(shell).arg(flag).arg(command).output() {
        Ok(output) => output,
        Err(error) => return Err(error_flow(runtime, "E485", format!("Can't read file {command}: {error}"))),
    };
    let status = output.status.code().unwrap_or(-1);
    replace_scope_pair(&mut scope.vim, "shell_error", Typval::Number(i64::from(status)));
    Ok(split_read_lines(&output.stdout))
}

fn command_write<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, command: &ExCommand) -> Flow {
    if command.usefilter {
        return command_write_filter(runtime, editor, scope, lua, command);
    }
    let buffer = match editor.current_buffer() {
        Some(buffer) => buffer,
        None => return error_flow(runtime, "E32", "No file name"),
    };
    if editor.buffer(buffer).is_ok_and(|state| state.readonly) && !command.bang {
        return error_flow(runtime, "E45", "'readonly' option is set (add ! to override)");
    }
    let name = command.args.trim();
    let path = if name.is_empty() {
        let existing = match editor.buffer(buffer) {
            Ok(state) => state.name().to_string_lossy(),
            Err(error) => return error_flow(runtime, "E32", error.to_string()),
        };
        if existing.is_empty() {
            return error_flow(runtime, "E32", "No file name");
        }
        PathBuf::from(existing.as_ref())
    } else {
        PathBuf::from(name)
    };
    let bytes = match editor.buffer(buffer).and_then(|state| state.text().map_err(Into::into)) {
        Ok(text) => text.to_bytes(),
        Err(error) => return error_flow(runtime, "E749", error.to_string()),
    };
    let contents = String::from_utf8_lossy(&bytes);
    if let Err(error) = runtime.scripts.io().write_string(&path, &contents) {
        return error_flow(runtime, "E212", format!("Can't open file for writing: {error}"));
    }
    if let Ok(state) = editor.buffer_mut(buffer) {
        state.set_name(OxStr::from(path.to_string_lossy().as_ref()));
        state.mark_saved();
    }
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
fn command_write_filter<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let shell_command = command.args.trim();
    if shell_command.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
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
    let mut input = Vec::new();
    for line in lines.iter().take(end).skip(start.saturating_sub(1)) {
        input.extend_from_slice(line);
        input.push(b'\n');
    }
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
    let mut child = match std::process::Command::new(shell)
        .arg(flag)
        .arg(shell_command)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return error_flow(runtime, "E485", format!("Can't read file {shell_command}: {error}"));
        }
    };
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write as _;
        if let Err(error) = stdin.write_all(&input) {
            return error_flow(runtime, "E212", format!("Can't open file for writing: {error}"));
        }
    }
    drop(child.stdin.take());
    let status = match child.wait() {
        Ok(status) => status.code().unwrap_or(-1),
        Err(error) => {
            return error_flow(runtime, "E485", format!("Can't read file {shell_command}: {error}"));
        }
    };
    replace_scope_pair(&mut scope.vim, "shell_error", Typval::Number(i64::from(status)));
    fire_shell_filter_post(runtime, editor, scope, lua)
}

fn command_split<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand, vertical: bool) -> Flow {
    let buffer = match editor.current_buffer() {
        Some(buffer) => buffer,
        None => return error_flow(runtime, "E749", "Empty buffer"),
    };
    let tab = match editor.current_tabpage() {
        Some(tab) => tab,
        None => return error_flow(runtime, "E749", "No current tabpage"),
    };
    let window = match editor.current_window() {
        Some(window) => window,
        None => return error_flow(runtime, "E749", "No current window"),
    };
    // `:new` and `:vnew` open an empty buffer; `:split`/`:vsplit` without an
    // argument keep showing the current one (`ex_splitview`, do_exedit).
    let new_buffer = if command.args.trim().is_empty() {
        if matches!(command.command.name(), "new" | "vnew") {
            match editor.create_buffer(true) {
                Ok(handle) => handle,
                Err(error) => return error_flow(runtime, "E948", error.to_string()),
            }
        } else {
            buffer
        }
    } else {
        match buffer_from_file(runtime, editor, &PathBuf::from(command.args.trim())) {
            Ok(handle) => handle,
            Err(flow) => return flow,
        }
    };
    let created = if vertical {
        editor.split_vertical(tab, window, new_buffer)
    } else {
        editor.split_horizontal(tab, window, new_buffer)
    };
    let created = match created {
        Ok(created) => created,
        Err(error) => {
            if new_buffer != buffer {
                let _ = editor.wipe_buffer(new_buffer);
            }
            return error_flow(runtime, "E36", error.to_string());
        }
    };
    if let Err(error) = editor.set_current_window(created) {
        let _ = editor.close_window(tab, created, true);
        if new_buffer != buffer {
            let _ = editor.wipe_buffer(new_buffer);
        }
        return error_flow(runtime, "E36", error.to_string());
    }
    Flow::Normal
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
fn command_tabnew<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
            Ok(handle) => handle,
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
fn command_tabonly<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    if editor.tabpages().len() <= 1 {
        push_text_message(editor, "Already only one tab page".to_owned(), false, false);
        return Flow::Normal;
    }
    let argument = command.args.trim();
    let keep = match tabpage_arg(editor, command) {
        Ok(target) => target,
        Err(()) => return error_flow(runtime, "E475", format!("Invalid argument: {argument}")),
    };
    if let Err(error) = editor.set_current_tabpage(keep) {
        return error_flow(runtime, "E475", error.to_string());
    }
    for tab in editor.tabpages() {
        if tab != keep {
            if editor.close_tabpage(tab).is_err() {
                return error_flow(runtime, "E444", "Cannot close last window");
            }
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
            Some(_) => resolve_range_raw(editor, command).map(|(_, end)| end).map_err(|_| ())?,
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
            let step = if rest.is_empty() { 1 } else { rest.parse::<isize>().map_err(|_| ())? };
            usize::try_from(step * relative + isize::try_from(current).map_err(|_| ())?).map_err(|_| ())?
        }
    };

    if number < 1 || number > last {
        return Err(());
    }
    tabs.get(number - 1).copied().ok_or(())
}

fn command_close<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand, quit: bool) -> Flow {
    let buffer = match editor.current_buffer() {
        Some(buffer) => buffer,
        None => return if quit { Flow::Quit(0) } else { error_flow(runtime, "E444", "Cannot close last window") },
    };
    if editor
        .buffer(buffer)
        .is_ok_and(|state| state.modified && state.attachments == 1)
        && !command.bang
    {
        return error_flow(runtime, "E37", "No write since last change (add ! to override)");
    }
    let tab = match editor.current_tabpage() {
        Some(tab) => tab,
        None => return Flow::Quit(0),
    };
    let window = match editor.current_window() {
        Some(window) => window,
        None => return Flow::Quit(0),
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
            return if quit { Flow::Quit(0) } else { error_flow(runtime, "E444", "Cannot close last window") };
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
    tabs.get(index + 1).or_else(|| index.checked_sub(1).and_then(|prev| tabs.get(prev))).copied()
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
fn command_undo<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
/// option at all, so that form reports NotImplemented rather than silently
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
        let Some(line) = lines.get(lnum - 1) else { continue };
        let retabbed = retab_line(line, old_tabstop, target_tabstop, expandtab, command.bang, indent_only);
        if let Some(rebuilt) = retabbed.line {
            let cursor = editor
                .current_window()
                .and_then(|window| editor.window(window).ok())
                .map_or(Position { lnum, col: 0 }, |state| state.cursor);
            if let Err(error) = editor.replace_buffer_lines(buffer, lnum, lnum, &[rebuilt], cursor, cursor, 0) {
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
        if let Err((code, message)) =
            set_and_mirror(editor, scope, "tabstop", OptionValue::Number(written), SetLayer::Local)
        {
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
            match flush_retab_run(
                &mut output,
                &run,
                run_start_vcol,
                vcol,
                new_tabstop,
                expandtab,
                forceit,
                tail,
            ) {
                Ok(run_changed) => changed |= run_changed,
                Err(TextTooLong) => {
                    too_long = true;
                    break;
                }
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
        match flush_retab_run(&mut output, &run, run_start_vcol, vcol, new_tabstop, expandtab, forceit, 0) {
            Ok(run_changed) => changed |= run_changed,
            Err(TextTooLong) => {
                too_long = true;
                output.extend_from_slice(run.as_bytes());
            }
        }
    }
    RetabbedLine { line: changed.then_some(output), too_long }
}

/// Emits one whitespace run, rebuilt when upstream would rebuild it.
///
/// `tail` is the byte count that follows the run in the line, which upstream
/// carries in `old_len - col` when it sizes the replacement.
///
/// Returns whether the emitted bytes differ from the run as it stood.
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
    if !had_tab && !(forceit && spaces > 1) {
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
    matches!(editor.options().get_buffer(buffer, name), Ok(OptionValue::Boolean(true)))
}

/// `:lockvar` and `:unlockvar` (`eval/vars.c` `ex_lockvar`:1554).
///
/// The depth defaults to 2, `!` means "everything" (-1), and a leading digit
/// run overrides it. The names that follow are handled one per whitespace-
/// separated word, as `ex_unletlock` walks them, and the lock itself is
/// `Scope::lockvar`/`unlockvar`.
fn command_lockvar<F: FileIO>(
    runtime: &mut ExRuntime<F>,
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
/// writes `'foldmarker'` into the text and lets the marker scan find it
/// (`foldCreateMarkers`, `fold.c:1554`).
fn command_fold<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
            return error_flow(runtime, "E350", "Cannot create fold with current 'foldmethod'");
        }
    }
    let folds = match editor.buffer_mut(buffer) {
        Ok(state) => &mut state.folds,
        Err(error) => return error_flow(runtime, "E749", error.to_string()),
    };
    folds.set_method(FoldMethod::Manual);
    match folds.create_manual(FoldPosition::new(first - 1, 0), FoldPosition::new(last, 0)) {
        Ok(_) => Flow::Normal,
        // An identical fold already exists, which upstream tolerates: foldCreate
        // simply nests another entry and the visible result is unchanged.
        Err(crate::fold::FoldError::DuplicateRange) => Flow::Normal,
        Err(error) => error_flow(runtime, "E350", error.to_string()),
    }
}

/// `foldCreateMarkers` (`fold.c:1554-1575`): append the `'foldmarker'` pair to
/// the first and last addressed lines so the marker scan finds a fold there.
///
/// Named gap: upstream wraps each marker in `'commentstring'` unless the line
/// already ends inside a comment (`foldAddMarker`, `fold.c:1579-1609`). The
/// wrap is implemented; the "already a comment" refinement needs `skip_comment`
/// and a comment parser this port does not have, so a marker added to a line
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
        let Some(line) = lines.get(lnum - 1) else { continue };
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
        if let Err(error) = editor.replace_buffer_lines(buffer, lnum, lnum, &[rebuilt], cursor, cursor, 0) {
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
fn command_foldopen<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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

/// `:hide` (`ex_docmd.c` `ex_hide`:5369): close a window without freeing its
/// buffer.
///
/// Without an address this is the current window; with one it is that window
/// number in the current tabpage, falling back to the last window when the
/// number is past the end (`win_find_nr`). A bare `:hide` is this command,
/// while `:hide {cmd}` is the modifier the parser already separates.
fn command_hide<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let Some(tab) = editor.current_tabpage() else {
        return error_flow(runtime, "E749", "No current tabpage");
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let target = match (&command.range, command.count) {
        (None, None) => editor.current_window(),
        (_, Some(count)) => window_by_number(&windows, count as usize),
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
    windows.get(number - 1).copied().or_else(|| windows.last().copied())
}

/// `:sleep` (`ex_docmd.c` `ex_sleep`:6459): pause for the count, in seconds by
/// default or milliseconds with an `m` suffix.
///
/// The count defaults to 1 and anything other than `m` or an empty tail is
/// `E475` reporting the *remaining* argument, not the whole one. A zero count
/// is `E939` from the shared count parse, since `sleep` carries no `ZEROR`.
fn command_sleep<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
        return error_flow(runtime, "E167", ":scriptencoding used outside of a sourced file");
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
fn command_argdelete<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
    let index = if count == 0 { 0 } else { requested.min(count - 1) };
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
fn command_z<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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

    let signed = |value: isize| -> isize { value };
    let big = bigness as isize;
    let base = lnum as isize;
    let (start, end, cursor, ruled) = match kind {
        b'-' => {
            let start = base - big * repeats as isize + 1;
            (start, start + big - 1, start + big - 1, false)
        }
        b'=' => (base - (big + 1) / 2 + 1, base + (big + 1) / 2 - 1, base, true),
        b'^' => (base - big * 2, base - big, base - big, false),
        b'.' => {
            let start = base - (big + 1) / 2 + 1;
            (start, base + (big + 1) / 2 - 1, base + (big + 1) / 2 - 1, false)
        }
        _ => {
            let mut start = base;
            if argument.starts_with('+') {
                start += big * signed(repeats as isize - 1) + 1;
            } else if !addressed {
                start += 1;
            }
            (start, start + big - 1, start + big - 1, false)
        }
    };
    let first = start.max(1) as usize;
    let last = (end.min(lines.len() as isize)).max(0) as usize;
    let cursor = cursor.clamp(1, lines.len() as isize) as usize;

    let number = matches!(option_value(editor, "number", SetLayer::Effective), Some(OptionValue::Boolean(true)));
    let width = lines.len().to_string().len();
    let rule = "-".repeat(screen_number_option(editor, "columns").saturating_sub(1));
    for index in first..=last {
        let Some(line) = lines.get(index - 1) else { continue };
        if ruled && index == lnum {
            push_info_text_message(editor, rule.clone());
        }
        let text = String::from_utf8_lossy(line).into_owned();
        push_info_text_message(editor, if number { format!("{index:>width$} {text}") } else { text });
        if ruled && index == lnum {
            push_info_text_message(editor, rule.clone());
        }
    }
    if let Some(window) = editor.current_window() {
        if let Err(error) = editor.set_window_cursor(window, Position { lnum: cursor, col: 0 }) {
            return error_flow(runtime, "E16", error.to_string());
        }
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
        if window != current {
            if let Err(error) = editor.close_window(tab, window, true) {
                return error_flow(runtime, "E445", error.to_string());
            }
        }
    }
    Flow::Normal
}

/// `:qall` (`ex_docmd.c` ex_quitall): quit all windows and the host
/// process when no buffer has unwritten changes; the bang form always
/// quits.  `check_changed_any` blocks on any modified buffer, hidden or
/// displayed, matching upstream's process-wide guard.
fn command_qall<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    if !command.bang
        && editor
            .buffers()
            .into_iter()
            .any(|buffer| editor.buffer(buffer).is_ok_and(|state| state.modified))
    {
        return error_flow(runtime, "E37", "No write since last change (add ! to override)");
    }
    Flow::Quit(0)
}

/// `:cquit [code]` (`ex_docmd.c` ex_cquit): terminate the host with
/// `code`, defaulting to EXIT_FAILURE when no count is given.
fn command_cquit(command: &ExCommand) -> Flow {
    let code = command
        .count
        .and_then(|value| i64::try_from(value).ok())
        .or_else(|| command.args.trim().parse::<i64>().ok())
        .unwrap_or(1);
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
    if let Some(current) = editor.current_buffer() {
        if editor.buffer(current).is_ok_and(|state| state.modified) && !command.bang {
            return error_flow(runtime, "E37", "No write since last change (add ! to override)");
        }
    }
    let current = editor.current_buffer();
    let current_index = current.and_then(|current| buffers.iter().position(|buffer| *buffer == current)).unwrap_or(0);
    let next = (current_index as isize + step).rem_euclid(buffers.len() as isize) as usize;
    match editor.set_current_buffer(buffers[next], BufferRelease::KeepLoaded) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E86", error.to_string()),
    }
}

fn command_buffer<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let requested = command
        .count
        .and_then(|value| i64::try_from(value).ok())
        .or_else(|| command.args.trim().parse::<i64>().ok());
    let handle = match requested.and_then(|value| BufHandle::try_from(value).ok()) {
        Some(handle) => handle,
        None => return error_flow(runtime, "E93", format!("More than one match for {}", command.args.trim())),
    };
    if let Some(current) = editor.current_buffer() {
        if editor.buffer(current).is_ok_and(|state| state.modified) && !command.bang {
            return error_flow(runtime, "E37", "No write since last change (add ! to override)");
        }
    }
    match editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E86", error.to_string()),
    }
}

/// `:args` (`ex_args`, arglist.c 502): with file arguments the list is
/// redefined and the first entry edited, exactly like `:next`; without
/// arguments the list is printed with the current entry in brackets.
fn command_args<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
fn command_next<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let list = command.args.trim();
    if list.is_empty() {
        let step = command_step(command);
        let target = editor.arglist().index() as i64 + step;
        return do_argfile(runtime, editor, command.bang, target);
    }
    // The changed-buffer guard runs before the list is replaced (ex_next
    // checks first so a failure leaves the old list intact).
    if let Some(current) = editor.current_buffer() {
        if editor.buffer(current).is_ok_and(|state| state.modified) && !command.bang {
            return error_flow(runtime, "E37", "No write since last change (add ! to override)");
        }
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
    editor
        .arglist_mut()
        .set(names.into_iter().map(|name| OxStr::from(name.as_str())).collect());
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
    {
        if let Ok(value) = i64::try_from(line) {
            return value;
        }
    }
    1
}
fn command_previous<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let step = command_step(command);
    let arglist = editor.arglist();
    let index = arglist.index() as i64;
    let count = arglist.len() as i64;
    let target = if index - step >= count { count - 1 } else { index - step };
    do_argfile(runtime, editor, command.bang, target)
}

/// Edits entry `target` of the argument list (`do_argfile`, arglist.c
/// 600): out-of-range targets fail with E163/E164/E165, and the index
/// only advances when the edit succeeded.
fn do_argfile<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, force: bool, target: i64) -> Flow {
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
fn edit_argument_file<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, force: bool, name: &str) -> Flow {
    if !force {
        if let Some(current) = editor.current_buffer() {
            if editor.buffer(current).is_ok_and(|state| state.modified) {
                return error_flow(runtime, "E37", "No write since last change (add ! to override)");
            }
        }
    }
    for handle in editor.buffers() {
        if editor.buffer(handle).is_ok_and(|state| state.name().as_bytes() == name.as_bytes()) {
            return match editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
                Ok(()) => Flow::Normal,
                Err(error) => error_flow(runtime, "E86", error.to_string()),
            };
        }
    }
    let text = match runtime.scripts.io().read_to_string(Path::new(name)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return error_flow(runtime, "E484", format!("Can't open file {name}: {error}")),
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
        return match editor.create_tabpage(handle, crate::Geometry { row: 0, col: 0, width: 80, height: 24 }) {
            Ok(_) => Flow::Normal,
            Err(error) => error_flow(runtime, "E948", error.to_string()),
        };
    }
    match editor.set_current_buffer(handle, BufferRelease::KeepLoaded) {
        Ok(()) => Flow::Normal,
        Err(error) => error_flow(runtime, "E86", error.to_string()),
    }
}

/// `:argdo` (`ex_listdo` CMD_argdo, ex_cmds2.c 461): for every entry in
/// the range switch to its buffer and execute the command tail; a failing
/// switch or command aborts the loop. The entry already displayed is not
/// re-edited (upstream avoids reloading it).
fn command_argdo<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let nested = command.args.trim();
    if nested.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let count = editor.arglist().len();
    if count == 0 {
        return Flow::Normal;
    }
    let (start, end) = match resolve_arg_range(editor, command) {
        Ok(range) => range,
        Err(message) => return error_flow(runtime, "E16", message),
    };
    if start > count {
        return Flow::Normal;
    }
    let logical = vec![LogicalLine { text: nested.to_owned(), first_line: runtime.scripts.current_line() }];
    let program = match parse_program(&runtime.user_commands, &logical) {
        Ok(program) => program,
        Err(error) => return exec_error_flow(runtime, error),
    };
    for entry in start..=end.min(count) {
        let index = entry - 1;
        if editor.arglist().index() != index || !editing_argument(editor, index) {
            let flow = do_argfile(runtime, editor, command.bang, index as i64);
            if !matches!(flow, Flow::Normal) {
                return flow;
            }
            if editor.arglist().index() != index {
                break;
            }
        }
        let flow = run_program(runtime, editor, scope, lua, &program, 0, program.len());
        if !matches!(flow, Flow::Normal) {
            return flow;
        }
    }
    Flow::Normal
}

/// Whether the current buffer already displays argument `index`
/// (`editing_arg_idx`, arglist.c 463).
fn editing_argument(editor: &Editor, index: usize) -> bool {
    let Some(name) = editor.arglist().name(index) else { return false };
    editor
        .current_buffer()
        .is_some_and(|buffer| editor.buffer(buffer).is_ok_and(|state| state.name().as_bytes() == name.as_bytes()))
}

/// Resolves a `:argdo` range against the argument list itself (entries,
/// not buffer lines); without a range the whole list is addressed.
fn resolve_arg_range(editor: &Editor, command: &ExCommand) -> Result<(usize, usize), String> {
    let count = editor.arglist().len();
    let current = editor.arglist().index() + 1;
    let Some(range) = &command.range else { return Ok((1, count)) };
    if matches!(range.kind, RangeKind::WholeBuffer) { return Ok((1, count)) };
    let start = range.start.as_ref().map_or(Ok(current), |address| resolve_address(editor, address, current, count))?;
    let end = range.end.as_ref().map_or(Ok(start), |address| resolve_address(editor, address, current, count))?;
    if start > end { return Err("Invalid range".to_owned()); }
    Ok((start.max(1), end))
}

fn command_put<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E749", "Empty buffer") };
    let position = editor.current_window().and_then(|window| editor.window(window).ok()).map_or(Position { lnum: 1, col: 0 }, |window| window.cursor);
    let register = command.register.unwrap_or('"');
    if register == '=' && !command.args.is_empty() {
        let value = match eval_text(runtime, editor, scope, lua, &command.args) {
            Ok(value) => value,
            Err(flow) => return flow,
        };
        let lines = typval_to_text(&value)
            .split('\n')
            .map(|line| line.as_bytes().to_vec())
            .collect::<Vec<_>>();
        return match editor.buffer_mut(buffer).and_then(|state| {
            state.append_lines(position.lnum, &lines, position, 0).map_err(Into::into)
        }) {
            Ok(_) => Flow::Normal,
            Err(error) => error_flow(runtime, "E354", error.to_string()),
        };
    }
    match editor.put_register(buffer, position, register, 0) {
        Ok(true) => Flow::Normal,
        Ok(false) => error_flow(runtime, "E353", format!("Nothing in register {register}")),
        Err(error) => error_flow(runtime, "E353", error.to_string()),
    }
}

fn command_delete<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E749", "Empty buffer") };
    let (start, end) = match resolve_range(editor, command) { Ok(range) => range, Err(message) => return error_flow(runtime, "E16", message) };
    let lines = match buffer_lines(editor, buffer) { Ok(lines) => lines, Err(message) => return error_flow(runtime, "E749", message) };
    let selected = lines[start.saturating_sub(1)..end.min(lines.len())].to_vec();
    let content = match RegisterContent::linewise(selected) { Ok(content) => content, Err(error) => return error_flow(runtime, "E354", error.to_string()) };
    if let Some(register) = command.register { if let Err(error) = editor.registers_mut().delete_to(register, content.clone()) { return error_flow(runtime, "E354", error.to_string()); } } else { editor.registers_mut().delete(content); }
    let replacement = if start == 1 && end >= lines.len() { vec![Vec::new()] } else { Vec::new() };
    let cursor = Position { lnum: start.min(lines.len().saturating_sub(end - start + 1)).max(1), col: 0 };
    match editor.replace_buffer_lines(buffer, start, end, &replacement, cursor, cursor, 0) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, "E16", error.to_string()),
    }
}

fn command_yank<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E749", "Empty buffer") };
    let (start, end) = match resolve_range(editor, command) { Ok(range) => range, Err(message) => return error_flow(runtime, "E16", message) };
    let lines = match buffer_lines(editor, buffer) { Ok(lines) => lines, Err(message) => return error_flow(runtime, "E749", message) };
    let content = match RegisterContent::linewise(lines[start.saturating_sub(1)..end.min(lines.len())].to_vec()) { Ok(content) => content, Err(error) => return error_flow(runtime, "E354", error.to_string()) };
    let result = if let Some(register) = command.register { editor.registers_mut().yank_to(register, content) } else { editor.registers_mut().yank(content); Ok(()) };
    match result { Ok(()) => Flow::Normal, Err(error) => error_flow(runtime, "E354", error.to_string()) }
}

/// `:print` / `:p` — `ex_docmd.c` `ex_print`: every addressed line goes to
/// the message sink as an Echo message. Numbering follows `print_line` →
/// `print_line_no_prefix` (`ex_cmds.c`): the 'number' option prefixes each
/// line with its right-aligned line number padded to the width of the last
/// line number (`number_width`). An empty buffer raises E749 first, and the
/// cursor lands on the last printed line.
fn command_print<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E749", "Empty buffer") };
    let lines = match buffer_lines(editor, buffer) { Ok(lines) => lines, Err(message) => return error_flow(runtime, "E749", message) };
    if lines.len() == 1 && lines[0].is_empty() { return error_flow(runtime, "E749", "Empty buffer"); }
    let (start, end) = match resolve_range(editor, command) { Ok(range) => range, Err(message) => return error_flow(runtime, "E16", message) };
    let number = matches!(option_value(editor, "number", SetLayer::Effective), Some(OptionValue::Boolean(true)));
    let width = lines.len().to_string().len();
    let last = end.min(lines.len());
    for lnum in start..=last {
        let text = String::from_utf8_lossy(&lines[lnum - 1]).into_owned();
        let message = if number { format!("{lnum:>width$} {text}") } else { text };
        push_info_text_message(editor, message);
    }
    if let Some(window) = editor.current_window() {
        if let Err(error) = editor.set_window_cursor(window, Position { lnum: last, col: 0 }) {
            return error_flow(runtime, "E16", error.to_string());
        }
    }
    Flow::Normal
}

fn command_mark<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let Some(name) = command.args.trim().chars().next() else { return error_flow(runtime, "E191", "Argument must be a letter or forward/backward quote") };
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E20", "Mark not set") };
    let position = editor.current_window().and_then(|window| editor.window(window).ok()).map_or(Position { lnum: 1, col: 0 }, |window| window.cursor);
    match editor.set_local_mark(buffer, name, position) { Ok(_) => Flow::Normal, Err(error) => error_flow(runtime, "E191", error.to_string()) }
}

fn command_marks<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor) -> Flow {
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E20", "Mark not set") };
    let marks = match editor.buffer(buffer) {
        Ok(state) => state.marks.iter().collect::<Vec<_>>(),
        Err(error) => return error_flow(runtime, "E20", error.to_string()),
    };
    push_text_message(editor, "mark line  col file/text".to_owned(), false, false);
    for (name, position) in marks {
        push_text_message(editor, format!(" {name} {:>5} {:>4}", position.lnum, position.col), false, false);
    }
    Flow::Normal
}

fn command_registers<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, args: &str) -> Flow {
    let requested = args.trim();
    let names = if requested.is_empty() { "0123456789abcdefghijklmnopqrstuvwxyz\"-:.%#=*+_/@" } else { requested };
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
fn source_runtime_file<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    path: &Path,
) -> Flow {
    if path.extension().is_some_and(|extension| extension == "lua") {
        let Some(host) = lua else { return Flow::NotImplemented("luafile".to_owned()) };
        if let Err(error) = sync_scope_into_editor(editor, scope) {
            return exec_error_flow(runtime, error);
        }
        let result = host.borrow_mut().execute_file(editor, path);
        let sync = sync_editor_into_scope(editor, scope);
        return match (result, sync) {
            (Err(error), _) => lua_error_flow(runtime, error, "E5112", "E5113"),
            (Ok(()), Err(error)) => exec_error_flow(runtime, error),
            (Ok(()), Ok(())) => Flow::Normal,
        };
    }
    match source_path(runtime, editor, scope, lua, path, false) {
        Ok(Flow::Finish) => Flow::Normal,
        Ok(flow) => flow,
        Err(error) => exec_error_flow(runtime, error),
    }
}

/// `source_runtime(names, DIP_ALL)` (`runtime.c` `do_in_path`:430-515):
/// walk 'runtimepath' in order and, in each entry, source every one of the
/// whitespace-separated `names` that exists there. Wildcards in `names` are
/// not expanded; the `:filetype` file lists are all literal names.
fn source_runtime_all<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    names: &str,
) -> Flow {
    let roots: Vec<PathBuf> =
        runtime.scripts.runtime_roots().iter().map(|root| root.path().to_path_buf()).collect();
    for root in roots {
        for name in names.split_ascii_whitespace() {
            let path = root.join(name);
            if !runtime.scripts.io().exists(&path) {
                continue;
            }
            let flow = source_runtime_file(runtime, editor, scope, lua, &path);
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
fn command_filetype<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    command: &ExCommand,
) -> Flow {
    let mut arg = command.args.trim();
    if arg.is_empty() {
        let state = runtime.filetype;
        let detect = if state.detect == Some(true) { "ON" } else { "OFF" };
        let dependent = |value: Option<bool>| match (value, state.detect) {
            (Some(true), Some(true)) => "ON",
            (Some(true), _) => "(on)",
            _ => "OFF",
        };
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
                let flow = source_runtime_all(runtime, editor, scope, lua, FILETYPE_FILE);
                if !matches!(flow, Flow::Normal) {
                    return flow;
                }
                runtime.filetype.detect = Some(true);
                if plugin {
                    let flow = source_runtime_all(runtime, editor, scope, lua, FTPLUGIN_FILE);
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                    runtime.filetype.plugin = Some(true);
                }
                if indent {
                    let flow = source_runtime_all(runtime, editor, scope, lua, INDENT_FILE);
                    if !matches!(flow, Flow::Normal) {
                        return flow;
                    }
                    runtime.filetype.indent = Some(true);
                }
            }
            if arg == "detect" {
                return filetype_detect_autocmds(runtime, editor, scope, lua);
            }
            Flow::Normal
        }
        "off" => {
            if !plugin && !indent {
                let flow = source_runtime_all(runtime, editor, scope, lua, FTOFF_FILE);
                if !matches!(flow, Flow::Normal) {
                    return flow;
                }
                runtime.filetype.detect = Some(false);
                return Flow::Normal;
            }
            if plugin {
                let flow = source_runtime_all(runtime, editor, scope, lua, FTPLUGOF_FILE);
                if !matches!(flow, Flow::Normal) {
                    return flow;
                }
                runtime.filetype.plugin = Some(false);
            }
            if indent {
                let flow = source_runtime_all(runtime, editor, scope, lua, INDOFF_FILE);
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
fn filetype_detect_autocmds<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
) -> Flow {
    let Some(group) = editor.autocmds().group("filetypedetect") else { return Flow::Normal };
    let buffer = editor.current_buffer();
    let name = buffer
        .and_then(|buffer| editor.buffer(buffer).ok())
        .map(|state| state.name().to_string_lossy().into_owned())
        .unwrap_or_default();
    let plan = editor.autocmds_mut().plan_in_group(
        Event::BufReadPost,
        group,
        AutocmdContext { buffer, file_name: Some(&name), ..AutocmdContext::default() },
    );
    run_autocmd_plan(runtime, editor, scope, lua, plan)
}

fn command_colorscheme<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
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
        return error_flow(runtime, "E185", format!("Cannot find color scheme '{name}'"));
    };
    let flow = source_runtime_file(runtime, editor, scope, lua, &path);
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
    let plan = editor.autocmds_mut().plan(
        Event::ColorScheme,
        AutocmdContext { file_name: Some(name), ..AutocmdContext::default() },
    );
    run_autocmd_plan(runtime, editor, scope, lua, plan)
}

/// Executes one [`FiringPlan`] in order, acknowledging `++once` definitions
/// as each action starts and stopping at the first non-normal flow
/// (`autocmd.c` `apply_autocmds_group` runs the matched list in sequence).
fn run_autocmd_plan<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    plan: FiringPlan,
) -> Flow {
    for action in plan.ready {
        if action.once {
            editor.autocmds_mut().consume_once(action.id);
        }
        let action_flow = match action.kind {
            AutocmdKind::ExString(source) => {
                let logical = vec![LogicalLine {
                    text: source,
                    first_line: runtime.scripts.current_line(),
                }];
                match parse_program(&runtime.user_commands, &logical) {
                    Ok(program) => run_program(runtime, editor, scope, lua, &program, 0, program.len()),
                    Err(error) => exec_error_flow(runtime, error),
                }
            }
            AutocmdKind::LuaCallback(reference) => {
                let Some(lua) = lua else {
                    return error_flow(runtime, "E5108", "Lua callbacks are not installed");
                };
                if let Err(error) = sync_scope_into_editor(editor, scope) {
                    return exec_error_flow(runtime, error);
                }
                match usize::try_from(reference) {
                    Ok(reference) => match lua.borrow_mut().invoke_callback(editor, reference, Vec::new()) {
                        Ok(()) => match sync_editor_into_scope(editor, scope) {
                            Ok(()) => Flow::Normal,
                            Err(error) => exec_error_flow(runtime, error),
                        },
                        Err(error) => lua_error_flow(runtime, error, "E5107", "E5108"),
                    },
                    Err(_) => error_flow(runtime, "E5108", "Lua callback reference is out of range"),
                }
            }
        };
        if !matches!(action_flow, Flow::Normal) {
            return action_flow;
        }
    }
    Flow::Normal
}

/// `getout` (`main.c`:753-882), the exit sequence: `VimLeavePre` runs first,
/// then the ShaDa write this port does not have, then `VimLeave`, and only
/// then does the process go away. Both events fire once per process, which is
/// what `apply_autocmds` guarantees upstream by never returning to the Ex loop
/// afterwards; here the flag on the runtime says the same thing, since a
/// `:quit` inside a `VimLeave` handler must not restart the sequence.
///
/// An autocmd that fails does not cancel the exit: upstream reports it through
/// `emsg` and carries on to `os_exit`, so the text is recorded as a message and
/// the requested status survives.
fn fire_exit_autocmds<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
) {
    if runtime.exiting {
        return;
    }
    runtime.exiting = true;
    for event in [Event::VimLeavePre, Event::VimLeave] {
        let plan = editor.autocmds_mut().plan(event, AutocmdContext::default());
        if plan.ready.is_empty() {
            continue;
        }
        let flow = run_autocmd_plan(runtime, editor, scope, lua, plan);
        if let Flow::Exception(exception) = flow {
            push_text_message(editor, exception.message(), true, true);
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
        return error_flow(runtime, "E197", format!("Cannot set language to \"{name}\""));
    }
    // Keep number parsing on decimal points, as upstream re-pins LC_NUMERIC
    // after every successful setlocale.
    ox_sys::set_locale(LocaleCategory::Numeric, "C");

    set_language_env(scope, "LC_ALL", "");
    if !matches!(what, LocaleCategory::Time | LocaleCategory::Collate) {
        if what == LocaleCategory::All {
            set_language_env(scope, "LANG", name);
            set_language_env(scope, "LANGUAGE", "");
        }
        if what != LocaleCategory::CType {
            set_language_env(scope, "LC_MESSAGES", name);
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

/// One `os_setenv` pair from `ex_language`, applied through the audited
/// seam and mirrored into the executor's `$` scope so `$LC_ALL`-style reads
/// observe the new value.
fn set_language_env(scope: &mut Scope, name: &str, value: &str) {
    ox_sys::set_env(name, value);
    scope.set_env(name.as_bytes(), Typval::String(OxStr::from(value)));
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
        replace_scope_pair(&mut scope.vim, name, Typval::String(OxStr::from(value.as_str())));
    }
}

fn command_highlight<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let args = command.args.trim();
    if args.is_empty() {
        let messages = editor
            .highlights()
            .iter()
            .map(|(name, attributes)| format!(
                "{name} xxx {}",
                attributes
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
            .collect::<Vec<_>>();
        for message in messages {
            push_text_message(editor, message, false, false);
        }
        return Flow::Normal;
    }
    let mut words = args.split_ascii_whitespace();
    let Some(first) = words.next() else { return Flow::Normal };
    if first.eq_ignore_ascii_case("clear") {
        if let Some(name) = words.next() { editor.highlights_mut().remove(name); } else { editor.highlights_mut().clear(); }
        return Flow::Normal;
    }

    let default = first.eq_ignore_ascii_case("default") || first.eq_ignore_ascii_case("def");
    let Some(group_or_link) = (if default { words.next() } else { Some(first) }) else {
        return error_flow(runtime, "E471", "Argument required");
    };
    let link = group_or_link.eq_ignore_ascii_case("link");
    let Some(group) = (if link { words.next() } else { Some(group_or_link) }) else {
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
            let Some((key, value)) = word.split_once('=') else { return error_flow(runtime, "E416", format!("Missing equal sign: {word}")) };
            attributes.insert(key.to_ascii_lowercase(), value.to_owned());
        }
    }
    editor.highlights_mut().insert(group.to_owned(), attributes);
    Flow::Normal
}

fn command_augroup<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let name = command.args.trim();
    if name.eq_ignore_ascii_case("END") {
        runtime.current_augroup = AugroupId::default();
        return Flow::Normal;
    }
    match editor.autocmds_mut().create_group(name, command.bang) {
        Ok(group) => { runtime.current_augroup = group; Flow::Normal }
        Err(error) => error_flow(runtime, "E936", error.to_string()),
    }
}

fn command_autocmd<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let args = command.args.trim();
    if args.is_empty() { return Flow::Normal; }
    if command.bang {
        if let Some(group) = editor.autocmds().group(args) {
            return match editor.autocmds_mut().delete(DeleteAutocmds {
                group: Some(group),
                event: None,
                pattern: None,
            }) {
                Ok(_) => Flow::Normal,
                Err(error) => error_flow(runtime, "E216", error.to_string()),
            };
        }
    }
    let mut words = args.splitn(3, char::is_whitespace).filter(|word| !word.is_empty());
    let Some(event_name) = words.next() else { return Flow::Normal };
    let event = match Event::from_name(event_name) { Some(event) => event, None => return error_flow(runtime, "E216", format!("No such group or event: {event_name}")) };
    let pattern = words.next().unwrap_or("*");
    if command.bang {
        return match editor.autocmds_mut().delete(DeleteAutocmds { group: Some(runtime.current_augroup), event: Some(event), pattern: Some(pattern) }) {
            Ok(_) => Flow::Normal,
            Err(error) => error_flow(runtime, "E216", error.to_string()),
        };
    }
    let body = words.next().unwrap_or("");
    if body.is_empty() { return Flow::Normal; }
    match editor.autocmds_mut().register(event, pattern, AutocmdKind::ExString(body.to_owned()), AutocmdOptions { group: runtime.current_augroup, ..AutocmdOptions::default() }) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, "E216", error.to_string()),
    }
}

fn command_user_command<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let args = command.args.trim();
    if args.is_empty() {
        for name in runtime.user_commands.commands.keys() { push_text_message(editor, name.clone(), false, false); }
        return Flow::Normal;
    }
    let mut words = args.split_ascii_whitespace().peekable();
    let mut nargs = '0';
    let mut accepts_bang = false;
    let mut accepts_range = false;
    let mut accepts_register = false;
    while words.peek().is_some_and(|word| word.starts_with('-')) {
        let flag = words.next().unwrap_or_default();
        if let Some(value) = flag.strip_prefix("-nargs=") { nargs = value.chars().next().unwrap_or('0'); }
        else if flag == "-bang" { accepts_bang = true; }
        else if flag == "-range" || flag.starts_with("-range=") { accepts_range = true; }
        else if flag == "-register" { accepts_register = true; }
        else if !matches!(flag, "-bar" | "-buffer" | "-complete" | "-count") && !flag.starts_with("-complete=") && !flag.starts_with("-count=") && !flag.starts_with("-addr=") { return error_flow(runtime, "E181", format!("Invalid attribute: {flag}")); }
    }
    let Some(name) = words.next() else { return error_flow(runtime, "E183", "User defined commands must be capitalized") };
    if !valid_user_command_name(name) { return error_flow(runtime, "E183", "User defined commands must be capitalized") }
    let source = runtime
        .scripts
        .current_name()
        .zip(runtime.scripts.current_sid())
        .map(|(name, sid)| (name.to_owned(), sid));
    if let Some(existing) = runtime.user_commands.commands.get(name) {
        let same_script_new_source = match (&existing.source, &source) {
            (Some((existing_name, existing_sid)), Some((current_name, current_sid))) => {
                existing_name == current_name && existing_sid != current_sid
            }
            _ => false,
        };
        if !command.bang && !same_script_new_source {
            return error_flow(runtime, "E174", "Command already exists: add ! to replace it");
        }
    }
    let body = words.collect::<Vec<_>>().join(" ");
    runtime.user_commands.commands.insert(name.to_owned(), UserCommand { name: name.to_owned(), body, nargs, accepts_bang, accepts_range, accepts_register, source });
    Flow::Normal
}

fn command_delcommand<F: FileIO>(runtime: &mut ExRuntime<F>, command: &ExCommand) -> Flow {
    let name = command.args.trim();
    if runtime.user_commands.commands.remove(name).is_none() && !command.bang { return error_flow(runtime, "E184", format!("No such user-defined command: {name}")); }
    Flow::Normal
}

fn command_invoke_user<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, name: &str, command: &ExCommand) -> Flow {
    let Some(definition) = runtime.user_commands.commands.get(name).cloned() else { return error_flow(runtime, "E492", format!("Not an editor command: {name}")) };
    let args = command.args.trim();
    let count = count_ex_arguments(args);
    let valid = match definition.nargs { '0' => count == 0, '1' => count == 1, '?' => count <= 1, '+' => count >= 1, '*' => true, _ => false };
    if !valid { return error_flow(runtime, "E471", "Argument required") }
    if command.bang && !definition.accepts_bang { return error_flow(runtime, "E477", "No ! allowed") }
    if command.range.is_some() && !definition.accepts_range { return error_flow(runtime, "E481", "No range allowed") }
    if command.register.is_some() && !definition.accepts_register { return error_flow(runtime, "E488", "Trailing characters") }
    let (line1, line2) = resolve_range(editor, command).unwrap_or_else(|_| current_line_pair(editor));
    let expanded = definition.body
        .replace("<f-args>", &split_command_arguments(args))
        .replace("<args>", args)
        .replace("<q-args>", &format!("'{}'", args.replace('\'', "''")))
        .replace("<bang>", if command.bang { "!" } else { "" })
        .replace("<line1>", &line1.to_string())
        .replace("<line2>", &line2.to_string())
        .replace("<count>", &command.count.unwrap_or(0).to_string())
        .replace("<reg>", &command.register.map_or(String::new(), |value| value.to_string()));
    let logical = vec![LogicalLine { text: expanded, first_line: runtime.scripts.current_line() }];
    let program = match parse_program(&runtime.user_commands, &logical) { Ok(program) => program, Err(error) => return exec_error_flow(runtime, error) };
    run_program(runtime, editor, scope, lua, &program, 0, program.len())
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

fn command_map<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let name = command.command.name();
    let modes = map_modes(name, command.bang);
    let scope = if command.args.contains("<buffer>") { MapScope::Buffer(editor.current_buffer().unwrap_or(BufHandle::CURRENT)) } else { MapScope::Global };
    if name.ends_with("clear") { editor.mappings_mut().mapclear(modes, scope); return Flow::Normal; }
    let args = command.args.replace("<buffer>", "");
    let mut split = args.trim().splitn(2, char::is_whitespace).filter(|part| !part.is_empty());
    let Some(lhs) = split.next() else { return Flow::Normal };
    if name.ends_with("unmap") { editor.mappings_mut().unmap(&Keys::from(lhs), modes, scope); return Flow::Normal; }
    let Some(rhs) = split.next() else { return error_flow(runtime, "E474", "Invalid argument") };
    let action = match MappingAction::parse_rhs(rhs.trim()) { Ok(action) => action, Err(error) => return error_flow(runtime, "E474", error.to_string()) };
    let options = MappingOptions { modes, scope, remap: !name.contains("nore"), nowait: command.args.contains("<nowait>"), silent: command.args.contains("<silent>"), description: None };
    let result = if name.contains("nore") { editor.mappings_mut().noremap(Keys::from(lhs), action, options) } else { editor.mappings_mut().map(Keys::from(lhs), action, options) };
    match result { Ok(()) => Flow::Normal, Err(error) => error_flow(runtime, "E474", error.to_string()) }
}

pub(crate) fn sync_editor_into_scope(editor: &Editor, scope: &mut Scope) -> Result<(), ExecError> {
    scope.global = dict_to_scope(editor.gvars());
    if let Some(buffer) = editor.current_buffer() { scope.buffer = dict_to_scope(editor.buffer(buffer).map_err(|error| ExecError::Editor(error.to_string()))?.variables()); }
    if let Some(window) = editor.current_window() { scope.window = dict_to_scope(editor.window_variables(window).map_err(|error| ExecError::Editor(error.to_string()))?); }
    if let Some(tab) = editor.current_tabpage() { scope.tab = dict_to_scope(editor.tabpage_variables(tab).map_err(|error| ExecError::Editor(error.to_string()))?); }
    scope.vim = dict_to_scope(editor.vvars());
    scope.registers.clear();
    for name in "0123456789abcdefghijklmnopqrstuvwxyz\"-:.%#=*+_/@".chars() {
        if let Ok(Some(content)) = editor.registers().get(name) { scope.set_register(&[name as u8], Typval::String(OxStr(content.to_bytes()))); }
    }
    scope.options_global.clear();
    scope.options_local.clear();
    for metadata in OPTION_METADATA {
        if metadata.scopes.contains(&OptionScope::Global) {
            if let Ok(value) = editor.options().get_global(metadata.name) { scope.set_option(EvalOptionScope::Global, metadata.name.as_bytes(), option_to_typval(value)); }
        }
        if let Some(buffer) = editor.current_buffer() {
            if metadata.scopes.contains(&OptionScope::Buffer) { if let Ok(value) = editor.options().get_buffer(buffer, metadata.name) { scope.set_option(EvalOptionScope::Local, metadata.name.as_bytes(), option_to_typval(value)); } }
        }
        if let Some(window) = editor.current_window() {
            if metadata.scopes.contains(&OptionScope::Window) { if let Ok(value) = editor.options().get_window(window, metadata.name) { scope.set_option(EvalOptionScope::Local, metadata.name.as_bytes(), option_to_typval(value)); } }
        }
    }
    Ok(())
}

pub(crate) fn sync_scope_into_editor(editor: &mut Editor, scope: &Scope) -> Result<(), ExecError> {
    *editor.gvars_mut() = scope_to_dict(&scope.global);
    if let Some(buffer) = editor.current_buffer() { *editor.buffer_mut(buffer).map_err(|error| ExecError::Editor(error.to_string()))?.variables_mut() = scope_to_dict(&scope.buffer); }
    if let Some(window) = editor.current_window() { *editor.window_variables_mut(window).map_err(|error| ExecError::Editor(error.to_string()))? = scope_to_dict(&scope.window); }
    if let Some(tab) = editor.current_tabpage() { *editor.tabpage_variables_mut(tab).map_err(|error| ExecError::Editor(error.to_string()))? = scope_to_dict(&scope.tab); }
    *editor.vvars_mut() = scope_to_dict(&scope.vim);
    Ok(())
}

fn assign_target<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, target: &str, value: Typval, constant: bool) -> Result<(), Flow> {
    let target = target.trim();
    if let Some(inner) = target.strip_prefix('[').and_then(|target| target.strip_suffix(']')) {
        let targets = split_comma_args(inner);
        let Typval::List(values) = value else { return Err(error_flow(runtime, "E714", "List required")); };
        let values = values.borrow().items.clone();
        if targets.len() < values.len() { return Err(error_flow(runtime, "E687", "Less targets than List items")); }
        if targets.len() > values.len() { return Err(error_flow(runtime, "E688", "More targets than List items")); }
        for (target, value) in targets.into_iter().zip(values) {
            assign_target(runtime, editor, scope, target, value, constant)?;
        }
        return Ok(());
    }
    if let Some(register) = target.strip_prefix('@').and_then(|name| name.chars().next()) {
        let content = RegisterContent::characterwise(typval_to_text(&value).as_bytes()).map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
        editor.registers_mut().set(register, content).map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
        scope.set_register(&[register as u8], value);
        return Ok(());
    }
    if let Some(environment) = target.strip_prefix('$') {
        // `ex_let_env` (`eval/vars.c`:1349-1351) hands the value straight to
        // `vim_setenv_ext`, which is `os_setenv`: the assignment *is* a change
        // to the process environment, so every child inherits it. Recording it
        // only in the script scope leaves children with the value oxvim was
        // started with, which is how `setup.vim`'s
        // `let $HOME = expand(getcwd() . '/XfakeHOME')` sandbox failed to
        // reach `system('rm -rf ...')` and a suite cleanup deleted the real
        // home directory instead of the sandbox.
        let text = typval_to_text(&value);
        ox_sys::set_env(environment, &text);
        scope.set_env(environment.as_bytes(), Typval::String(OxStr(text.into_bytes())));
        return Ok(());
    }
    if let Some(option) = target.strip_prefix('&') { return assign_option(runtime, editor, scope, option, value); }
    let (kind, name) = parse_scope_name(target);
    if kind == Some(ScopeKind::Vim) && vim_variable_is_writable(name.as_bytes()) {
        replace_scope_pair(&mut scope.vim, &name, value);
    } else if let Some(kind) = kind {
        scope.set_scoped(kind, name.as_bytes(), 0, value).map_err(|error| eval_error_flow(runtime, error))?;
    } else {
        scope.set(name.as_bytes(), value).map_err(|error| eval_error_flow(runtime, error))?;
    }
    Ok(())
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

fn read_target<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &Editor, scope: &Scope, target: &str) -> Result<Typval, Flow> {
    let target = target.trim();
    if let Some(register) = target.strip_prefix('@').and_then(|name| name.chars().next()) { return Ok(scope.get_register(&[register as u8])); }
    if let Some(environment) = target.strip_prefix('$') {
        // The scope copy is seeded from the process environment at startup and
        // kept by `let $VAR`, but the process environment is what `vim_getenv`
        // reads, so a variable set through it alone -- `setenv()`, a locale
        // change -- is visible here too. An unset one is the empty string, as
        // upstream's expression evaluation gives.
        if scope.contains_env(environment.as_bytes()) {
            return Ok(scope.get_env(environment.as_bytes()));
        }
        return Ok(Typval::String(
            std::env::var_os(environment)
                .map_or_else(|| OxStr::from(""), |value| OxStr::from(value.to_string_lossy().as_ref())),
        ));
    }
    if let Some(option) = target.strip_prefix('&') { return Ok(read_option(editor, option)); }
    let (kind, name) = parse_scope_name(target);
    let value = if let Some(kind) = kind { scope.get_scoped(kind, name.as_bytes(), 0) } else { scope.get(name.as_bytes(), 0) };
    value.cloned().map_err(|error| eval_error_flow(runtime, error))
}

fn remove_target(editor: &mut Editor, scope: &mut Scope, target: &str) -> bool {
    let target = target.trim();
    if let Some(register) = target.strip_prefix('@').and_then(|name| name.chars().next()) {
        let Ok(content) = RegisterContent::characterwise(&[]) else {
            return false;
        };
        return editor.registers_mut().set(register, content).is_ok();
    }
    if let Some(environment) = target.strip_prefix('$') {
        // `do_unlet_var` (`eval/vars.c`:1653-1654) is `vim_unsetenv_ext`, the
        // process-wide unset, for the same reason the assignment is a
        // process-wide set. It reports no failure upstream, and a variable
        // that was never recorded in the scope copy is still unset in the
        // environment the children see.
        ox_sys::unset_env(environment);
        remove_scope_pair(&mut scope.env, environment);
        return true;
    }
    if target.starts_with('&') { return false; }
    let (kind, name) = parse_scope_name(target);
    match kind {
        Some(ScopeKind::Global) => remove_scope_pair(&mut scope.global, &name),
        Some(ScopeKind::Buffer) => remove_scope_pair(&mut scope.buffer, &name),
        Some(ScopeKind::Window) => remove_scope_pair(&mut scope.window, &name),
        Some(ScopeKind::Tab) => remove_scope_pair(&mut scope.tab, &name),
        Some(ScopeKind::Script) => remove_scope_pair(&mut scope.script, &name),
        Some(ScopeKind::Local) => remove_scope_pair(&mut scope.local, &name),
        Some(ScopeKind::Argument | ScopeKind::Vim) => false,
        None => remove_scope_pair(&mut scope.local, &name) || remove_scope_pair(&mut scope.global, &name),
    }
}

fn assign_option<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, option: &str, value: Typval) -> Result<(), Flow> {
    let (prefix, name) = if let Some(name) = option.strip_prefix("g:") { (SetLayer::Global, name) } else if let Some(name) = option.strip_prefix("l:") { (SetLayer::Local, name) } else { (SetLayer::Effective, option) };
    let metadata = crate::option_metadata(name).ok_or_else(|| error_flow(runtime, "E355", format!("Unknown option: {name}")))?;
    let converted = typval_to_option(&value, metadata.value_type).map_err(|message| error_flow(runtime, "E474", message))?;
    set_option_value(editor, metadata.name, converted, prefix).map_err(|(code, message)| error_flow(runtime, code, message))?;
    let eval_scope = if matches!(prefix, SetLayer::Global) { EvalOptionScope::Global } else { EvalOptionScope::Local };
    scope.set_option(eval_scope, metadata.name.as_bytes(), value);
    if metadata.name == "runtimepath" {
        sync_runtime_roots(runtime, editor);
    }
    Ok(())
}

fn read_option(editor: &Editor, option: &str) -> Typval {
    let (layer, name) = if let Some(name) = option.strip_prefix("g:") { (SetLayer::Global, name) } else if let Some(name) = option.strip_prefix("l:") { (SetLayer::Local, name) } else { (SetLayer::Effective, option) };
    option_value(editor, name, layer).map_or(Typval::Number(0), option_to_typval)
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
        let previous = bytes[..index].iter().rposition(|byte| !byte.is_ascii_whitespace());
        if index > 0 && bytes[index - 1].is_ascii_whitespace() && previous.is_some_and(|previous| {
            bytes[previous].is_ascii_alphanumeric() || matches!(bytes[previous], b'\'' | b'"' | b']' | b')' | b'}')
        }) {
            return expression[..index].trim_end();
        }
        quote = Some(byte);
    }
    expression
}

fn split_assignment(args: &str) -> Option<(&str, &str, &str)> {
    let bytes = args.as_bytes();
    let mut quote = None;
    let mut depth = 0usize;
    for index in 0..bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote { if byte == active && (index == 0 || bytes[index - 1] != b'\\') { quote = None; } continue; }
        if matches!(byte, b'\'' | b'"') { quote = Some(byte); continue; }
        if matches!(byte, b'(' | b'[' | b'{') { depth += 1; continue; }
        if matches!(byte, b')' | b']' | b'}') { depth = depth.saturating_sub(1); continue; }
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
            return Some((args[..start].trim(), args[start..=index].trim(), args[index + 1..].trim()));
        }
    }
    None
}

fn split_for(args: &str) -> Option<(&str, &str)> { args.split_once(" in ").map(|(target, expression)| (target.trim(), expression.trim())) }

fn parse_scope_name(target: &str) -> (Option<ScopeKind>, String) {
    let bytes = target.as_bytes();
    if bytes.len() > 2 && bytes[1] == b':' { if let Some(kind) = ScopeKind::from_byte(bytes[0]) { return (Some(kind), target[2..].to_owned()); } }
    (None, target.to_owned())
}

fn canonical_target(target: &str) -> String { target.trim().to_owned() }

fn apply_assignment_operator<F: FileIO>(runtime: &mut ExRuntime<F>, left: Typval, right: Typval, operator: &str) -> Result<Typval, Flow> {
    if operator == "+=" {
        if let (Typval::List(left_items), Typval::List(right_items)) = (&left, &right) {
            let appended = right_items.borrow().items.clone();
            left_items.borrow_mut().items.extend(appended);
            return Ok(left);
        }
    }
    match operator {
        "+=" => Ok(Typval::Number(typval_number(&left).unwrap_or(0).saturating_add(typval_number(&right).unwrap_or(0)))),
        "-=" => Ok(Typval::Number(typval_number(&left).unwrap_or(0).saturating_sub(typval_number(&right).unwrap_or(0)))),
        ".=" | "..=" => Ok(Typval::String(OxStr(
            format!("{}{}", typval_to_text(&left), typval_to_text(&right)).into_bytes(),
        ))),
        _ => Err(error_flow(runtime, "E734", format!("Wrong variable type for {operator}"))),
    }
}

/// Compound assignment on an option reference, per eval/vars.c
/// `ex_let_option`: `.`/`..` operators concatenate and are rejected on
/// number and boolean options, while `+`/`-` do arithmetic and are
/// rejected on string options — both rejections raise E734.
fn apply_option_assignment_operator<F: FileIO>(runtime: &mut ExRuntime<F>, current: Typval, operand: Typval, operator: &str) -> Result<Typval, Flow> {
    let concatenate = operator.starts_with('.');
    match (current, concatenate) {
        (Typval::String(current), true) => Ok(Typval::String(OxStr(
            format!("{}{}", current.to_string_lossy(), typval_to_text(&operand)).into_bytes(),
        ))),
        (Typval::Number(current), false) => match operator {
            "+=" => Ok(Typval::Number(current.saturating_add(typval_number(&operand).unwrap_or(0)))),
            "-=" => Ok(Typval::Number(current.saturating_sub(typval_number(&operand).unwrap_or(0)))),
            _ => Err(error_flow(runtime, "E734", format!("Wrong variable type for {operator}"))),
        },
        _ => Err(error_flow(runtime, "E734", format!("Wrong variable type for {operator}"))),
    }
}

fn iterable_values(value: Typval) -> Result<Vec<Typval>, &'static str> {
    match value {
        Typval::List(list) => list.try_borrow().map(|data| data.items.clone()).map_err(|_| "List is locked"),
        Typval::Blob(bytes) => Ok(bytes.into_iter().map(|byte| Typval::Number(byte as i64)).collect()),
        _ => Err("List required"),
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
    let start = range.start.as_ref().map_or(Ok(current), |address| resolve_address(editor, address, current, last))?;
    let end = range.end.as_ref().map_or(Ok(start), |address| resolve_address(editor, address, current, last))?;
    if start > end { return Err("Invalid range".to_owned()); }
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
            (arglist.index().saturating_add(1), if count == 0 { 1 } else { count })
        }
        AddrType::Buffers => {
            let current = editor.current_buffer().map_or(1, |buffer| {
                usize::try_from(i64::from(buffer)).unwrap_or(1)
            });
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
    // Only an explicit address can leave a domain; the defaults never do.
    if command.range.is_none() {
        return Ok(());
    }
    let addr_type = effective_addr_type(&command.command);
    // Upstream accepts any range in these domains, so there is nothing to
    // bound even though the values resolve.
    if matches!(addr_type, AddrType::Other | AddrType::TabsRelative | AddrType::Unsigned | AddrType::None) {
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
    editor.current_buffer().and_then(|buffer| editor.buffer(buffer).ok()).and_then(|state| state.text().ok()).map_or(1, Buffer::line_count)
}

fn resolve_address(editor: &Editor, address: &Address, current: usize, last: usize) -> Result<usize, String> {
    let mut value = match &address.base {
        AddressBase::Current => current,
        AddressBase::Last => last,
        AddressBase::Line(line) => *line as usize,
        AddressBase::Mark(name) => { let buffer = editor.current_buffer().ok_or_else(|| "Mark not set".to_owned())?; editor.local_mark(buffer, *name).map_err(|error| error.to_string())?.ok_or_else(|| "Mark not set".to_owned())?.lnum },
        AddressBase::ForwardSearch(_) | AddressBase::BackwardSearch(_) => return Err("Search addresses are not implemented in ranges".to_owned()),
    };
    for offset in &address.offsets { value = if *offset >= 0 { value.saturating_add(*offset as usize) } else { value.saturating_sub(offset.unsigned_abs() as usize) }; }
    Ok(value)
}

fn current_line_pair(editor: &Editor) -> (usize, usize) { let current = editor.current_window().and_then(|window| editor.window(window).ok()).map_or(1, |window| window.cursor.lnum); (current, current) }

struct IfBlock { branches: Vec<IfBranch>, end: usize }
struct IfBranch { condition: Option<String>, start: usize, end: usize }

fn find_if(program: &[Instruction], open: usize, limit: usize) -> Option<IfBlock> {
    let mut depth = 0usize;
    let mut markers = Vec::new();
    let mut index = open + 1;
    while index < limit {
        match program[index].name() {
            "if" => depth += 1,
            "endif" if depth == 0 => {
                let mut branches = Vec::new();
                let mut condition = Some(program[open].command.as_ref()?.args.trim().to_owned());
                let mut start = open + 1;
                for marker in markers {
                    branches.push(IfBranch { condition, start, end: marker });
                    condition = match program[marker].name() { "elseif" => Some(program[marker].command.as_ref()?.args.trim().to_owned()), _ => None };
                    start = marker + 1;
                }
                branches.push(IfBranch { condition, start, end: index });
                return Some(IfBlock { branches, end: index });
            }
            "endif" => depth = depth.saturating_sub(1),
            "elseif" | "else" if depth == 0 => markers.push(index),
            _ => {}
        }
        index += 1;
    }
    None
}

struct TryBlock { try_end: usize, catches: Vec<CatchBlock>, finally: Option<(usize, usize)>, end: usize }
struct CatchBlock { pattern: Option<String>, start: usize, end: usize }

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
                return Some(TryBlock { try_end, catches, finally, end: index });
            }
            "endtry" => depth = depth.saturating_sub(1),
            "catch" | "finally" if depth == 0 => markers.push(index),
            _ => {}
        }
        index += 1;
    }
    None
}

fn find_matching(program: &[Instruction], open: usize, limit: usize, opener: &str, closer: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, instruction) in program.iter().enumerate().take(limit).skip(open + 1) {
        let name = instruction.name();
        if name == opener { depth += 1; } else if name == closer { if depth == 0 { return Some(index); } depth -= 1; }
    }
    None
}

fn parse_catch_pattern(args: &str) -> Option<String> {
    let args = args.trim();
    if args.is_empty() { return None; }
    let delimiter = args.chars().next()?;
    take_delimited(args, delimiter).map(|(pattern, _)| pattern)
}

fn regex_matches_catch_pattern(pattern: &str, text: &str) -> Result<bool, String> {
    let program = compile_regex(pattern, Magic::Magic).map_err(|error| error.to_string())?;
    Ok(ox_regex::exec(&program, &RegexText::new(text.to_owned())).is_some())
}

fn render_command(command: &ExCommand) -> String {
    let mut text = String::new();
    if let Some(range) = &command.range { text.push_str(&render_range(range)); }
    text.push_str(command.command.name());
    if command.bang { text.push('!'); }
    if !command.args.is_empty() { text.push(' '); text.push_str(&command.args); }
    text
}

fn render_range(range: &Range) -> String {
    if matches!(range.kind, RangeKind::WholeBuffer) { return "%".to_owned(); }
    let mut text = range.start.as_ref().map(render_address).unwrap_or_default();
    if let Some(end) = &range.end { text.push(','); text.push_str(&render_address(end)); }
    text
}

fn render_address(address: &Address) -> String {
    let mut text = match &address.base { AddressBase::Current => ".".to_owned(), AddressBase::Last => "$".to_owned(), AddressBase::Line(line) => line.to_string(), AddressBase::Mark(mark) => format!("'{mark}"), AddressBase::ForwardSearch(pattern) => format!("/{pattern}/"), AddressBase::BackwardSearch(pattern) => format!("?{pattern}?") };
    for offset in &address.offsets { if *offset >= 0 { text.push('+'); text.push_str(&offset.to_string()); } else { text.push_str(&offset.to_string()); } }
    text
}

fn split_comma_args(source: &str) -> Vec<&str> { split_top_level(source, b',', true) }

fn split_top_level(source: &str, delimiter: u8, exact: bool) -> Vec<&str> {
    let bytes = source.as_bytes(); let mut result = Vec::new(); let mut start = 0usize; let mut quote = None; let mut depth = 0usize; let mut index = 0usize;
    while index < bytes.len() { let byte = bytes[index]; if let Some(active) = quote { if byte == active && (index == 0 || bytes[index - 1] != b'\\') { quote = None; } index += 1; continue; } if matches!(byte, b'\'' | b'"') { quote = Some(byte); index += 1; continue; } if matches!(byte, b'(' | b'[' | b'{') { depth += 1; } else if matches!(byte, b')' | b']' | b'}') { depth = depth.saturating_sub(1); } else if depth == 0 && (byte == delimiter || (!exact && delimiter == b' ' && byte.is_ascii_whitespace())) { if start < index { result.push(source[start..index].trim()); } while index + 1 < bytes.len() && bytes[index + 1].is_ascii_whitespace() { index += 1; } start = index + 1; } index += 1; }
    if start < source.len() { result.push(source[start..].trim()); } result
}

fn take_delimited(source: &str, delimiter: char) -> Option<(String, &str)> {
    let mut escaped = false; let start = delimiter.len_utf8();
    for (relative, character) in source[start..].char_indices() { if escaped { escaped = false; continue; } if character == '\\' { escaped = true; continue; } if character == delimiter { let end = start + relative; return Some((source[start..end].to_owned(), &source[end + delimiter.len_utf8()..])); } }
    None
}

fn expand_replacement(replacement: &str, groups: &[String]) -> String {
    let mut output = String::new(); let mut chars = replacement.chars();
    while let Some(character) = chars.next() { if character == '&' { output.push_str(groups.first().map_or("", String::as_str)); continue; } if character != '\\' { output.push(character); continue; } match chars.next() { Some('0') | Some('&') => output.push_str(groups.first().map_or("", String::as_str)), Some(digit @ '1'..='9') => { let index = digit.to_digit(10).unwrap_or(0) as usize; output.push_str(groups.get(index).map_or("", String::as_str)); } Some('r') => output.push('\r'), Some('n') => output.push('\n'), Some('t') => output.push('\t'), Some(other) => output.push(other), None => output.push('\\') } }
    output
}

fn substitute_plain(source: &str, pattern: &str, replacement: &str, global: bool) -> Result<String, String> {
    let program = compile_regex(pattern, Magic::Magic).map_err(|error| error.to_string())?;
    let text = RegexText::new(source.to_owned()); let mut output = String::new(); let mut previous = 0usize; let mut cursor = 0usize;
    while cursor <= source.len() { let Some(position) = text.position(cursor) else { break }; let Some(matched) = regex_exec_at(&program, &text, position) else { break; }; output.push_str(&source[previous..matched.start.byte]); let mut groups = vec![source[matched.start.byte..matched.end.byte].to_owned()]; for capture in &matched.captures { groups.push(capture.as_ref().map_or_else(String::new, |capture| source[capture.start.byte..capture.end.byte].to_owned())); } output.push_str(&expand_replacement(replacement, &groups)); previous = matched.end.byte; if !global { break; } cursor = if matched.start.byte == matched.end.byte { next_boundary(source, matched.end.byte) } else { matched.end.byte }; if cursor > source.len() { break; } }
    output.push_str(&source[previous..]); Ok(output)
}

fn next_boundary(text: &str, at: usize) -> usize { if at >= text.len() { return text.len().saturating_add(1); } at + text[at..].chars().next().map_or(1, char::len_utf8) }

pub(crate) fn typval_to_text(value: &Typval) -> String { match value { Typval::String(value) => value.to_string_lossy().into_owned(), _ => typval_to_display(value, false) } }

fn typval_to_display(value: &Typval, quoted_strings: bool) -> String {
    match value {
        Typval::Number(value) => value.to_string(),
        Typval::Float(value) => value.to_string(),
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
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
        Typval::List(list) => list.try_borrow().map_or("[]".to_owned(), |data| {
            format!(
                "[{}]",
                data.items
                    .iter()
                    .map(|item| typval_to_display(item, true))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        Typval::Dict(dict) => dict.try_borrow().map_or("{}".to_owned(), |data| {
            format!(
                "{{{}}}",
                data.entries
                    .iter()
                    .map(|(key, value)| format!(
                        "'{}': {}",
                        key.to_string_lossy().replace('\'', "''"),
                        typval_to_display(value, true)
                    ))
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
    }
}

pub(crate) fn typval_number(value: &Typval) -> Option<i64> { match value { Typval::Number(value) => Some(*value), Typval::Bool(value) => Some(i64::from(*value)), Typval::String(value) => value.to_string_lossy().parse().ok(), Typval::Channel(value) | Typval::Job(value) => i64::try_from(*value).ok(), _ => None } }
fn parse_number_prefix(text: &str) -> i64 {
    let bytes = text.trim_start().as_bytes();
    let mut end = 0;
    if bytes.first().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
        end = 1;
    }
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == 0 || (end == 1 && matches!(bytes.first(), Some(b'+') | Some(b'-'))) {
        return 0;
    }
    std::str::from_utf8(&bytes[..end])
        .ok()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

pub(crate) fn option_to_typval(value: &OptionValue) -> Typval { match value { OptionValue::Boolean(value) => Typval::Number(i64::from(*value)), OptionValue::Number(value) => Typval::Number(*value), OptionValue::String(value) => Typval::String(OxStr::from(value.as_str())) } }

fn dictionary_function_target(scope: &Scope, name: &str) -> Result<Option<(DictRef, OxStr)>, (&'static str, String)> {
    let Some((dictionary, member)) = name.rsplit_once('.') else { return Ok(None); };
    let mut path = dictionary.split('.');
    let root = path.next().expect("a dotted function name has a dictionary root");
    let root_value = if root.as_bytes().get(1) == Some(&b':') {
        let kind = ScopeKind::from_byte(root.as_bytes()[0]).ok_or_else(|| ("E128", format!("Invalid function name: {name}")))?;
        if kind == ScopeKind::Global {
            return Err(("E862", "Cannot use g: here".to_owned()));
        }
        scope.get_scoped(kind, &root.as_bytes()[2..], 0)
    } else {
        scope.get(root.as_bytes(), 0)
    }
    .map_err(|error| (error.code, error.message))?
    .clone();

    let mut current = match root_value {
        Typval::Dict(dictionary) => dictionary,
        _ => return Err(("E715", "Dictionary required".to_owned())),
    };
    for key in path {
        let value = current
            .try_borrow()
            .map_err(|_| ("E724", "Unable to correctly dump variable with self-referencing container".to_owned()))?
            .entries
            .iter()
            .find(|(name, _)| name.as_bytes() == key.as_bytes())
            .map(|(_, value)| value.clone())
            .ok_or_else(|| ("E716", format!("Key not present in Dictionary: {key}")))?;
        current = match value {
            Typval::Dict(dictionary) => dictionary,
            _ => return Err(("E715", "Dictionary required".to_owned())),
        };
    }
    Ok(Some((current, OxStr::from(member))))
}

pub(crate) fn typval_to_option(value: &Typval, value_type: OptionType) -> Result<OptionValue, String> { match value_type { OptionType::Boolean => typval_number(value).map(|value| OptionValue::Boolean(value != 0)).ok_or_else(|| "Number required".to_owned()), OptionType::Number => typval_number(value).map(OptionValue::Number).ok_or_else(|| "Number required".to_owned()), OptionType::String => Ok(OptionValue::String(typval_to_text(value))) } }

fn option_value<'a>(editor: &'a Editor, name: &str, layer: SetLayer) -> Option<&'a OptionValue> {
    let metadata = crate::option_metadata(name)?;
    match layer { SetLayer::Global => editor.options().get_global(metadata.name).ok(), SetLayer::Local | SetLayer::Effective => { if metadata.scopes.contains(&OptionScope::Window) { editor.current_window().and_then(|window| editor.options().get_window(window, metadata.name).ok()) } else if metadata.scopes.contains(&OptionScope::Buffer) { editor.current_buffer().and_then(|buffer| editor.options().get_buffer(buffer, metadata.name).ok()) } else { editor.options().get_global(metadata.name).ok() } } }
}

fn set_option_value(editor: &mut Editor, name: &str, value: OptionValue, layer: SetLayer) -> Result<(), (&'static str, String)> {
    let metadata = crate::option_metadata(name).ok_or_else(|| ("E355", format!("Unknown option: {name}")))?;
    let result = match layer { SetLayer::Global => editor.options_mut().set_global(metadata.name, value), SetLayer::Local | SetLayer::Effective => { if metadata.scopes.contains(&OptionScope::Window) { let window = editor.current_window().ok_or_else(|| ("E355", "No current window".to_owned()))?; editor.options_mut().set_window(window, metadata.name, value) } else if metadata.scopes.contains(&OptionScope::Buffer) { let buffer = editor.current_buffer().ok_or_else(|| ("E355", "No current buffer".to_owned()))?; editor.options_mut().set_buffer(buffer, metadata.name, value) } else { editor.options_mut().set_global(metadata.name, value) } } };
    result.map_err(|error| ("E474", error.to_string()))
}

fn set_one(editor: &mut Editor, scope: &mut Scope, raw: &str, layer: SetLayer) -> Result<(), (&'static str, String)> {
    let raw = raw.trim();
    if let Some(name) = raw.strip_suffix('?') { if let Some(text) = display_option(editor, name, layer) { push_info_text_message(editor, text); return Ok(()); } return Err(("E518", format!("Unknown option: {name}"))); }
    if let Some(name) = raw.strip_suffix("&vim").or_else(|| raw.strip_suffix('&')) { let metadata = crate::option_metadata(name).ok_or_else(|| ("E518", format!("Unknown option: {name}")))?; let value = metadata.default.value.map(OptionValue::from).ok_or_else(|| ("E474", format!("No literal default for {name}")))?; return set_and_mirror(editor, scope, metadata.name, value, layer); }
    for operator in ["+=", "-=", "^=", "="] { if let Some((name, value)) = raw.split_once(operator) { let metadata = crate::option_metadata(name).ok_or_else(|| ("E518", format!("Unknown option: {name}")))?; let mut next = match metadata.value_type { OptionType::Boolean => OptionValue::Boolean(matches!(value, "1" | "true" | "on")), OptionType::Number => OptionValue::Number(value.parse().map_err(|_| ("E521", format!("Number required after =: {value}")))?), OptionType::String => OptionValue::String(if metadata.expand { expand_env_esc(value) } else { value.to_owned() }) }; if operator != "=" { let current = option_value(editor, metadata.name, layer).cloned().unwrap_or_else(|| metadata.default.value.map(OptionValue::from).unwrap_or(OptionValue::String(String::new()))); next = modify_option(current, next, operator, metadata.list)?; } return set_and_mirror(editor, scope, metadata.name, next, layer); } }
    let (name, value) = if let Some(name) = raw.strip_prefix("no") { (name, false) } else if let Some(name) = raw.strip_prefix("inv") { let current = option_value(editor, name, layer).and_then(|value| match value { OptionValue::Boolean(value) => Some(*value), _ => None }).unwrap_or(false); (name, !current) } else if let Some(name) = raw.strip_suffix('!') { let current = option_value(editor, name, layer).and_then(|value| match value { OptionValue::Boolean(value) => Some(*value), _ => None }).unwrap_or(false); (name, !current) } else { (raw, true) };
    let metadata = crate::option_metadata(name).ok_or_else(|| ("E518", format!("Unknown option: {name}")))?;
    if metadata.value_type != OptionType::Boolean { if let Some(text) = display_option(editor, name, layer) { push_info_text_message(editor, text); return Ok(()); } }
    set_and_mirror(editor, scope, metadata.name, OptionValue::Boolean(value), layer)
}

/// Writes one option to the editor and mirrors it into the eval scope, the
/// same dual write `:let &opt` performs through `assign_option`. Without the
/// mirror, `&opt` reads inside the same command batch would keep observing
/// the pre-`:set` snapshot until the next editor→scope sync.
fn set_and_mirror(editor: &mut Editor, scope: &mut Scope, name: &'static str, value: OptionValue, layer: SetLayer) -> Result<(), (&'static str, String)> {
    set_option_value(editor, name, value.clone(), layer)?;
    let eval_scope = if matches!(layer, SetLayer::Global) { EvalOptionScope::Global } else { EvalOptionScope::Local };
    scope.set_option(eval_scope, name.as_bytes(), option_to_typval(&value));
    Ok(())
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
    if bytes.first() == Some(&b'~') && (bytes.len() == 1 || bytes[1] == b'/') {
        if let Some(home) = std::env::var_os("HOME") {
            output.extend_from_slice(home.to_string_lossy().as_bytes());
            index = 1;
        }
    }
    while index < bytes.len() {
        if bytes[index] != b'$' || index + 1 >= bytes.len() {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        let (name, next) = if bytes[index + 1] == b'{' {
            match bytes[index + 2..].iter().position(|&byte| byte == b'}') {
                Some(close) => (&value[index + 2..index + 2 + close], index + 2 + close + 1),
                None => {
                    output.push(b'$');
                    index += 1;
                    continue;
                }
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
        match std::env::var_os(name) {
            Some(text) => output.extend_from_slice(text.to_string_lossy().as_bytes()),
            // Unset stays literal, like upstream `vim_getenv` returning NULL.
            None => {
                output.push(b'$');
                output.extend_from_slice(if bytes[index + 1] == b'{' {
                    format!("{{{name}}}")
                } else {
                    name.to_owned()
                }.as_bytes());
            }
        }
        index = next;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn modify_option(current: OptionValue, next: OptionValue, operator: &str, list: Option<OptionListKind>) -> Result<OptionValue, (&'static str, String)> {
    match (current, next) {
        (OptionValue::Number(left), OptionValue::Number(right)) => Ok(OptionValue::Number(match operator {
            "+=" => left.saturating_add(right),
            "-=" => left.saturating_sub(right),
            "^=" => right.saturating_mul(10).saturating_add(left),
            _ => right,
        })),
        (OptionValue::String(mut left), OptionValue::String(right)) => {
            if let Some(kind @ (OptionListKind::Comma | OptionListKind::OneComma | OptionListKind::CommaColon | OptionListKind::OneCommaColon | OptionListKind::FlagsComma)) = list {
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
    let colon = matches!(kind, OptionListKind::CommaColon | OptionListKind::OneCommaColon);
    let reject_empty = matches!(kind, OptionListKind::OneComma | OptionListKind::OneCommaColon);
    let mut items = CommaItems::new(left)
        .filter(|item| !reject_empty || !item.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for operand in CommaItems::new(right) {
        let matches = |item: &str| {
            if colon {
                if let Some(offset) = find_unescaped(operand, ':') {
                    return item.get(..=offset) == operand.get(..=offset);
                }
            }
            item == operand
        };
        match operator {
            "-=" => items.retain(|item| !matches(item)),
            "+=" => {
                if colon { items.retain(|item| !matches(item)); }
                if !items.iter().any(|item| item == operand) { items.push(operand.to_owned()); }
            }
            "^=" => {
                if colon { items.retain(|item| !matches(item)); }
                if !items.iter().any(|item| item == operand) { items.insert(0, operand.to_owned()); }
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
    let Some(metadata) = crate::option_metadata(name) else { return true };
    let Some(default) = metadata.default.value.map(OptionValue::from) else { return false };
    editor.options().get_global(metadata.name).is_ok_and(|value| value == &default)
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
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase) && name.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

pub(crate) fn buffer_lines(editor: &Editor, buffer: BufHandle) -> Result<Vec<Vec<u8>>, String> {
    let state = editor.buffer(buffer).map_err(|error| error.to_string())?;
    let text = state.text().map_err(|error| error.to_string())?;
    (1..=text.line_count()).map(|line| text.line(line).map_err(|error| error.to_string())).collect()
}

fn dict_to_scope(dict: &Dict) -> ScopeMap {
    dict.0.iter().map(|(key, value)| (key.clone(), object_to_typval(value))).collect()
}
fn scope_to_dict(scope: &ScopeMap) -> Dict {
    Dict(scope.iter().map(|(key, value)| (key.clone(), typval_to_object(value))).collect())
}
pub(crate) fn object_to_typval(value: &Object) -> Typval {
    match value {
        Object::Nil => Typval::Special(Special::Null),
        Object::Boolean(value) => Typval::Bool(*value),
        Object::Integer(value) => Typval::Number(*value),
        Object::Float(value) => Typval::Float(*value),
        Object::String(value) => Typval::String(value.clone()),
        Object::Array(values) => Typval::list(values.iter().map(object_to_typval).collect()),
        Object::Dict(values) => Typval::dict(values.0.iter().map(|(key, value)| (key.clone(), object_to_typval(value))).collect()),
        Object::LuaRef(value) => Typval::Number(i64::from(*value)),
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
        Typval::Blob(value) => Object::Array(value.iter().map(|byte| Object::Integer(i64::from(*byte))).collect()),
        Typval::List(value) => value.try_borrow().map_or(Object::Nil, |data| Object::Array(data.items.iter().map(typval_to_object).collect())),
        Typval::Dict(value) => value.try_borrow().map_or(Object::Nil, |data| Object::Dict(Dict(data.entries.iter().map(|(key, value)| (key.clone(), typval_to_object(value))).collect()))),
        Typval::Funcref(value) | Typval::Partial(value) => Object::String(value.name.clone()),
        Typval::Bool(value) => Object::Boolean(*value),
        Typval::Special(Special::Null) => Object::Nil,
        Typval::Channel(value) | Typval::Job(value) => Object::Integer(i64::try_from(*value).unwrap_or(i64::MAX)),
    }
}

fn remove_scope_pair(map: &mut ScopeMap, name: &str) -> bool {
    let before = map.len();
    map.retain(|(key, _)| key.as_bytes() != name.as_bytes());
    before != map.len()
}

pub(crate) fn replace_scope_pair(map: &mut ScopeMap, name: &str, value: Typval) -> Option<Typval> {
    let previous = map
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .map(|(_, value)| value.clone());
    remove_scope_pair(map, name);
    map.push((OxStr::from(name), value));
    previous
}

fn restore_scope_pair(map: &mut ScopeMap, name: &str, previous: Option<Typval>) {
    remove_scope_pair(map, name);
    if let Some(value) = previous {
        map.push((OxStr::from(name), value));
    }
}

pub(crate) fn push_text_message(editor: &mut Editor, text: String, error: bool, history: bool) {
    editor.push_message(Message {
        kind: if error { MessageKind::Error } else { MessageKind::Echo },
        content: Object::String(OxStr(text.into_bytes())),
        history,
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
    });
}

fn error_flow<F: FileIO>(runtime: &ExRuntime<F>, code: &'static str, message: impl Into<String>) -> Flow {
    Flow::Exception(runtime.exception(code, message))
}
fn userfunc_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: UserFuncError) -> Flow {
    error_flow(runtime, error.code, error.message)
}
fn eval_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: EvalError) -> Flow {
    match error.kind {
        EvalErrorKind::NotImplemented(name) => Flow::NotImplemented(name.to_string_lossy().into_owned()),
        EvalErrorKind::Vim => error_flow(runtime, error.code, error.message),
    }
}
pub(crate) fn exec_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: ExecError) -> Flow {
    match error {
        ExecError::Vim(exception) => Flow::Exception(exception),
        ExecError::NotImplemented(name) => Flow::NotImplemented(name),
        ExecError::Eval(error) => eval_error_flow(runtime, error),
        ExecError::Parse(error) => error_flow(runtime, error.code.as_str(), error.message),
        ExecError::Io { path, message } => error_flow(runtime, "E484", format!("{}: {message}", path.display())),
        ExecError::Editor(message) => error_flow(runtime, "E605", message),
    }
}
pub(crate) fn flow_to_eval_error(flow: Flow, name: &str) -> EvalError {
    match flow {
        Flow::Exception(exception) => {
            EvalError::new("E605", 0, exception.message())
        }
        Flow::NotImplemented(name) => EvalError::not_implemented(OxStr(name.into_bytes())),
        _ => EvalError::new("E117", 0, format!("Unknown function: {name}")),
    }
}

fn command_lua<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, command: &ExCommand) -> Flow {
    let Some(lua) = lua else { return Flow::NotImplemented("lua".to_owned()) };
    let mut code = command.args.trim_start().to_owned();
    let mut heredoc = false;
    if let Some((header, body)) = code.split_once('\n') {
        if header.starts_with("<<") {
            heredoc = true;
            code = body.to_owned();
        }
    }
    if code.is_empty() && !heredoc {
        if command.range.is_none() {
            return error_flow(runtime, "E471", "Argument required");
        }
        let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E749", "Empty buffer") };
        let lines = match buffer_lines(editor, buffer) { Ok(lines) => lines, Err(message) => return error_flow(runtime, "E749", message) };
        let (first, last) = match resolve_range(editor, command) { Ok(range) => range, Err(message) => return error_flow(runtime, "E16", message) };
        code = lines[first.saturating_sub(1)..last.min(lines.len())]
            .iter()
            .map(|line| String::from_utf8_lossy(line))
            .collect::<Vec<_>>()
            .join("\n");
    } else if let Some(expression) = code.strip_prefix('=') {
        code = format!("vim._print(true, {expression})");
    }
    if let Err(error) = sync_scope_into_editor(editor, scope) {
        return exec_error_flow(runtime, error);
    }
    let result = lua.borrow_mut().execute_chunk(editor, &code, Vec::new());
    let sync = sync_editor_into_scope(editor, scope);
    match (result, sync) {
        (Err(error), _) => lua_error_flow(runtime, error, "E5107", "E5108"),
        (Ok(_), Err(error)) => exec_error_flow(runtime, error),
        (Ok(_), Ok(())) => Flow::Normal,
    }
}

fn command_luafile<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, command: &ExCommand) -> Flow {
    let Some(lua) = lua else { return Flow::NotImplemented("luafile".to_owned()) };
    let path = command.args.trim();
    if path.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    if let Err(error) = sync_scope_into_editor(editor, scope) {
        return exec_error_flow(runtime, error);
    }
    let result = lua.borrow_mut().execute_file(editor, Path::new(path));
    let sync = sync_editor_into_scope(editor, scope);
    match (result, sync) {
        (Err(error), _) => lua_error_flow(runtime, error, "E5112", "E5113"),
        (Ok(()), Err(error)) => exec_error_flow(runtime, error),
        (Ok(()), Ok(())) => Flow::Normal,
    }
}

fn command_luado<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, command: &ExCommand) -> Flow {
    let Some(lua) = lua else { return Flow::NotImplemented("luado".to_owned()) };
    let body = command.args.trim_start();
    if body.is_empty() {
        return error_flow(runtime, "E471", "Argument required");
    }
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E749", "Empty buffer") };
    let (first, last) = if command.range.is_none() {
        match buffer_lines(editor, buffer) {
            Ok(lines) => (1, lines.len()),
            Err(message) => return error_flow(runtime, "E749", message),
        }
    } else {
        match resolve_range(editor, command) {
            Ok(range) => range,
            Err(message) => return error_flow(runtime, "E16", message),
        }
    };
    let chunk = format!("return (function(line, linenr) {body} end)(...)");
    if let Err(error) = sync_scope_into_editor(editor, scope) {
        return exec_error_flow(runtime, error);
    }
    for lnum in first..=last {
        let lines = match buffer_lines(editor, buffer) {
            Ok(lines) => lines,
            Err(_) => break,
        };
        let Some(line) = lines.get(lnum.saturating_sub(1)).cloned() else { break };
        let result = lua.borrow_mut().execute_chunk(
            editor,
            &chunk,
            vec![Object::String(OxStr(line)), Object::Integer(lnum as i64)],
        );
        if editor.current_buffer() != Some(buffer) {
            break;
        }
        let replacement = match result {
            Ok(Object::String(value)) => Some(value.as_bytes().to_vec()),
            Ok(Object::Integer(value)) => Some(value.to_string().into_bytes()),
            Ok(Object::Float(value)) => Some(value.to_string().into_bytes()),
            Ok(_) => None,
            Err(error) => return lua_error_flow(runtime, error, "E5109", "E5111"),
        };
        if let Some(replacement) = replacement {
            let cursor = editor.current_window().and_then(|window| editor.window(window).ok()).map_or(Position { lnum, col: 0 }, |window| window.cursor);
            if let Err(error) = editor.replace_buffer_lines(buffer, lnum, lnum, &[replacement], cursor, cursor, 0) {
                return error_flow(runtime, "E16", error.to_string());
            }
        }
    }
    match sync_editor_into_scope(editor, scope) {
        Ok(()) => Flow::Normal,
        Err(error) => exec_error_flow(runtime, error),
    }
}

fn lua_error_flow<F: FileIO>(runtime: &mut ExRuntime<F>, error: LuaExecError, load_code: &'static str, runtime_code: &'static str) -> Flow {
    match error {
        LuaExecError::Load(message) => error_flow(runtime, load_code, message),
        LuaExecError::Runtime(message) | LuaExecError::Conversion(message) => {
            error_flow(runtime, runtime_code, message)
        }
    }
}
