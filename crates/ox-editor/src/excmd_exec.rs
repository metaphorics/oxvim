//! Ex command execution against the single-writer [`Editor`] model.
//!
//! Parsing remains in `ox-excmd`; this module owns command/control state,
//! script and function frames, exception transfer, user commands, and the
//! narrow host adapters needed by `ox-eval` and `ox-regex`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::cell::RefCell;
use std::rc::Rc;

use ox_eval::scope::{OptionScope as EvalOptionScope, ScopeMap};
use ox_eval::{
    builtin_spec, call_buffer_builtin, exists as exists_in_scope, is_buffer_builtin, BuiltinHost,
    BufferHost, Builtins, EvalError, EvalErrorKind, Evaluator, Parser as ExprParser, RegexEngine,
    Scope, ScopeKind,
};
use ox_excmd::{
    resolve_command, Address, AddressBase, ExCommand, ParseError, Parser as ExParser, Range,
    RangeKind, ResolveError, ResolvedCommand, UserCommandMatch, UserCommandProvider,
};
use ox_regex::{
    compile as compile_regex, exec_at as regex_exec_at, Magic, Position as RegexPosition,
    Text as RegexText,
};
use ox_sys::LocaleCategory;
use ox_text::{Buffer, Position};
use ox_types::{BufHandle, Dict, DictRef, Funcref, Object, OxStr, Special, Typval};

use crate::autocmd::{AutocmdContext, AutocmdKind, AutocmdOptions, AugroupId, DeleteAutocmds, Event};
use crate::extmark::{ExtmarkAttributes, ExtmarkId, ExtmarkPlacement, ExtmarkPosition, NamespaceId};
use crate::mapping::{MapMode, MapModes, MapScope, MappingAction, MappingOptions};
use crate::options::{find_unescaped, CommaItems, OptionListKind, OptionScope, OptionType, OptionValue, OPTION_METADATA};
use crate::register::RegisterContent;
use crate::script::{FileIO, LogicalLine, RealFileIO, ScriptCtx, Sid};
use crate::typeahead::Keys;
use crate::userfunc::{UserFuncError, UserFunctions};
use crate::{
    BufferRelease, ChannelIds, Editor, Geometry, JobCallbacks, JobEvent, JobManager, JobStartOptions, Message,
    MessageKind, ModeMachine,
};

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
}

#[derive(Clone, Debug, Default)]
struct UserCommandRegistry {
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

struct ExRuntime<F: FileIO> {
    scripts: ScriptCtx<F>,
    functions: UserFunctions,
    user_commands: UserCommandRegistry,
    const_vars: BTreeSet<String>,
    channel_ids: ChannelIds,
    jobs: Option<JobManager>,
    current_augroup: AugroupId,
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
        }
    }

    fn throwpoint(&self) -> String {
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
        source_path(&mut self.runtime, editor, &mut self.scope, self.lua.as_ref(), path, false)
            .and_then(flow_to_result)
    }
}

#[derive(Clone)]
struct Instruction {
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
enum Flow {
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

fn parse_program(
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
            Err(error) if error.code == ox_excmd::ErrorCode::E492 => {
                program.push(Instruction {
                    command: None,
                    parse_error: Some(error),
                    source: command_text.to_owned(),
                    line: line.first_line,
                });
                continue;
            }
            Err(error) => parse_put_expression(&parser, command_text).ok_or(error)?,
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

fn run_program<F: FileIO>(
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
                        Some(condition) => match eval_condition(runtime, editor, scope, condition) {
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
                    match eval_condition(runtime, editor, scope, command.args.trim()) {
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
                let canonical = match runtime.functions.define(
                    signature,
                    body,
                    sid,
                    command.bang,
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

        let flow = dispatch(runtime, editor, scope, lua, command);
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
    let name = command.command.name();
    match name {
        "lua" => command_lua(runtime, editor, scope, lua, command),
        "luado" => command_luado(runtime, editor, scope, lua, command),
        "luafile" => command_luafile(runtime, editor, scope, lua, command),
        "let" => command_let(runtime, editor, scope, &command.args, false),
        "const" => command_let(runtime, editor, scope, &command.args, true),
        "unlet" => command_unlet(runtime, editor, scope, &command.args, command.bang),
        "set" => command_set(runtime, editor, scope, &command.args, SetLayer::Effective),
        "setlocal" => command_set(runtime, editor, scope, &command.args, SetLayer::Local),
        "setglobal" => command_set(runtime, editor, scope, &command.args, SetLayer::Global),
        "aunmenu" | "tlunmenu" if command.args.trim() == "*" => Flow::Normal,
        "echo" | "echomsg" | "echon" | "echoerr" => {
            command_echo(runtime, editor, scope, name, &command.args)
        }
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
        "source" => {
            let path = PathBuf::from(command.args.trim());
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
        "enew" => command_enew(runtime, editor, command),
        "write" | "wq" | "xit" => {
            let flow = command_write(runtime, editor, command);
            if matches!(flow, Flow::Normal) && matches!(name, "wq" | "xit") {
                command_close(runtime, editor, command, true)
            } else {
                flow
            }
        }
        "split" => command_split(runtime, editor, command, false),
        "vsplit" => command_split(runtime, editor, command, true),
        "close" => command_close(runtime, editor, command, false),
        "quit" => command_close(runtime, editor, command, true),
        "qall" => command_qall(runtime, editor, command),
        "cquit" => command_cquit(command),
        "bnext" => command_buffer_step(runtime, editor, command, 1),
        "bprevious" | "bprev" => command_buffer_step(runtime, editor, command, -1),
        "buffer" => command_buffer(runtime, editor, command),
        "bwipeout" | "bwipe" => command_buffer_remove(runtime, editor, command, true),
        "bdelete" | "bdel" | "bunload" | "bun" => command_buffer_remove(runtime, editor, command, false),
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
    let mut host = EvalHost {
        runtime,
        editor,
        lua,
        builtins: Builtins::new(&regex),
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
    text: &str,
) -> Result<bool, Flow> {
    let value = eval_text(runtime, editor, scope, None, text)?;
    match value {
        Typval::Number(number) => Ok(number != 0),
        Typval::Bool(value) => Ok(value),
        Typval::String(value) => Ok(parse_number_prefix(&value.to_string_lossy()) != 0),
        Typval::Float(value) => Ok(value != 0.0),
        Typval::Channel(id) | Typval::Job(id) => Ok(id != 0),
        _ => Err(error_flow(runtime, "E745", "Using a List as a Number")),
    }
}

struct EvalHost<'a, F: FileIO> {
    runtime: &'a mut ExRuntime<F>,
    editor: &'a mut Editor,
    lua: Option<&'a Rc<RefCell<dyn LuaExec>>>,
    builtins: Builtins<'a>,
    submatches: Option<Vec<String>>,
}

impl<F: FileIO> BuiltinHost for EvalHost<'_, F> {
    fn call(
        &mut self,
        name: &OxStr,
        args: Vec<Typval>,
        scope: &mut Scope,
    ) -> ox_eval::Result<Typval> {
        let name_text = name.to_string_lossy();
        if matches!(&*name_text, "jobstart" | "jobstop" | "jobwait" | "jobpid" | "chansend" | "jobsend") {
            return call_job_builtin(
                self.runtime, self.editor, scope, self.lua, &name_text, args,
            );
        }
        if name_text == "eval" {
            let [Typval::String(source)] = args.as_slice() else {
                return Err(EvalError::new("E119", 0, "One string argument required"));
            };
            let expression = ExprParser::new(source.as_bytes()).parse()?;
            let regex = VimRegex;
            return Evaluator::new(self, &regex).eval(&expression, scope);
        }
        if name_text == "submatch" {
            let index = args.first().and_then(typval_number).unwrap_or(0).max(0) as usize;
            let value = self
                .submatches
                .as_ref()
                .and_then(|groups| groups.get(index))
                .cloned()
                .unwrap_or_default();
            return Ok(Typval::String(OxStr(value.into_bytes())));
        }
        if name_text == "exists" {
            return exists_with_editor(self.runtime, self.editor, scope, args);
        }
        if name_text == "expand" {
            return call_expand_builtin(self.runtime, self.editor, args);
        }
        if name_text == "system" {
            return call_system_builtin(args, scope);
        }
        if crate::fs_builtins::is_filesystem_builtin(&name_text) {
            return crate::fs_builtins::call(self.runtime.scripts.io(), &name_text, args);
        }
        // Buffer-seam builtins (`getline`/`setline`) reach the current buffer
        // through ox_eval::BufferHost; the typval-only dispatcher below has
        // no buffer access.
        if is_buffer_builtin(&name_text) {
            let mut seam = CurrentBuffer(self.editor);
            return call_buffer_builtin(&mut seam, &name_text, args);
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

fn call_job_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    match name {
        "jobstart" => {
            let options = normalize_job_options(&args)?;
            let id = runtime.channel_ids.allocate();
            let mut manager = match runtime.jobs.take() {
                Some(manager) => manager,
                None => match JobManager::new() {
                    Ok(manager) => manager,
                    Err(_) => return Ok(Typval::Number(-1)),
                },
            };
            let started = manager.start(id, options);
            runtime.jobs = Some(manager);
            Ok(Typval::Number(if started.is_ok() { id as i64 } else { -1 }))
        }
        "jobstop" => {
            let id = job_id(args.first())?;
            let Some(manager) = runtime.jobs.as_mut() else { return Ok(Typval::Number(0)); };
            manager.stop(id)
                .map(|stopped| Typval::Number(i64::from(stopped)))
                .map_err(|message| EvalError::new("E900", 0, message))
        }
        "jobpid" => {
            let id = job_id(args.first())?;
            Ok(Typval::Number(runtime.jobs.as_ref().and_then(|jobs| jobs.pid(id)).map_or(0, i64::from)))
        }
        "chansend" | "jobsend" => {
            let id = job_id(args.first())?;
            let data = channel_bytes(args.get(1))?;
            let Some(manager) = runtime.jobs.as_mut() else { return Ok(Typval::Number(0)); };
            manager.send(id, data)
                .map(|sent| Typval::Number(i64::from(sent)))
                .map_err(|message| EvalError::new("E900", 0, message))
        }
        "jobwait" => {
            let ids = job_ids(args.first())?;
            let timeout = match args.get(1) {
                Some(value) => value_number(value).ok_or_else(|| EvalError::new("E474", 0, "Invalid argument"))?,
                None => -1,
            };
            let Some(mut manager) = runtime.jobs.take() else {
                return Ok(Typval::list(ids.iter().map(|_| Typval::Number(-3)).collect()));
            };
            let waited = manager.wait(&ids, timeout);
            runtime.jobs = Some(manager);
            let (statuses, events) = waited.map_err(|message| EvalError::new("E900", 0, message))?;
            invoke_job_events(runtime, editor, scope, lua, events)?;
            Ok(Typval::list(statuses.into_iter().map(Typval::Number).collect()))
        }
        _ => unreachable!(),
    }
}

fn call_system_builtin(args: Vec<Typval>, scope: &mut Scope) -> ox_eval::Result<Typval> {
    let Some(command) = args.first().and_then(|value| match value {
        Typval::String(value) => Some(value.to_string_lossy()),
        _ => None,
    }) else {
        return Err(EvalError::new("E730", 0, "Using a List as a String"));
    };
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
    let output = std::process::Command::new(shell)
        .arg(flag)
        .arg(command.as_ref())
        .output()
        .map_err(|error| EvalError::new("E677", 0, format!("Error writing temp file: {error}")))?;
    let status = output.status.code().unwrap_or(-1);
    replace_scope_pair(&mut scope.vim, "shell_error", Typval::Number(i64::from(status)));
    Ok(Typval::String(OxStr(output.stdout)))
}

fn call_expand_builtin<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &Editor,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    let [Typval::String(value), ..] = args.as_slice() else {
        return Err(EvalError::new("E730", 0, "Using a List as a String"));
    };
    let text = value.to_string_lossy();
    let expanded = match text.as_ref() {
        "%" => editor
            .current_buffer()
            .and_then(|buffer| editor.buffer(buffer).ok())
            .map_or_else(String::new, |buffer| buffer.name().to_string_lossy().into_owned()),
        "<SID>" => runtime
            .functions
            .active_sid()
            .or_else(|| runtime.scripts.current_sid())
            .map_or_else(String::new, |sid| format!("<SNR>{sid}_")),
        _ => text.into_owned(),
    };
    Ok(Typval::String(OxStr(expanded.into_bytes())))
}

fn normalize_job_options(args: &[Typval]) -> ox_eval::Result<JobStartOptions> {
    let (program, command_args) = job_command(args.first())?;
    let options = match args.get(1) {
        None => Typval::dict(Vec::new()),
        Some(Typval::Dict(options)) => Typval::Dict(options.clone()),
        Some(_) => return Err(EvalError::new("E1206", 0, "Dictionary required")),
    };
    let Typval::Dict(options_ref) = options else { unreachable!() };
    let get = |key: &str| {
        options_ref.borrow().entries.iter().find(|(name, _)| name.as_bytes() == key.as_bytes()).map(|(_, value)| value.clone())
    };
    let callbacks = JobCallbacks {
        options: options_ref.clone(),
        stdout: callback_option(get("on_stdout"))?,
        stderr: callback_option(get("on_stderr"))?,
        exit: callback_option(get("on_exit"))?,
    };
    let environment = match get("env") {
        None => None,
        Some(Typval::Dict(values)) => {
            let mut environment = std::env::vars_os().collect::<Vec<_>>();
            for (name, value) in &values.borrow().entries {
                let value = value_text(value)?;
                let name = OsString::from(name.to_string_lossy().into_owned());
                if let Some((_, current)) = environment.iter_mut().find(|(current, _)| current == &name) {
                    *current = OsString::from(value);
                } else {
                    environment.push((name, OsString::from(value)));
                }
            }
            Some(environment)
        }
        Some(_) => return Err(EvalError::new("E1206", 0, "env must be a Dictionary")),
    };
    let cwd = get("cwd").map(|value| value_text(&value).map(PathBuf::from)).transpose()?;
    let stdin_pipe = match get("stdin") {
        Some(value) => value_text(&value)? != "null",
        None => true,
    };
    Ok(JobStartOptions {
        program,
        args: command_args,
        environment,
        cwd,
        detached: get("detach").is_some_and(|value| value_bool(&value)),
        pty: get("pty").is_some_and(|value| value_bool(&value)) || get("term").is_some_and(|value| value_bool(&value)),
        rpc: get("rpc").is_some_and(|value| value_bool(&value)),
        stdin_pipe,
        stdout_buffered: get("stdout_buffered").is_some_and(|value| value_bool(&value)),
        stderr_buffered: get("stderr_buffered").is_some_and(|value| value_bool(&value)),
        callbacks,
    })
}

fn job_command(value: Option<&Typval>) -> ox_eval::Result<(PathBuf, Vec<OsString>)> {
    match value {
        Some(Typval::String(command)) if !command.as_bytes().is_empty() => {
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("sh"));
            Ok((PathBuf::from(shell), vec![OsString::from("-c"), OsString::from(command.to_string_lossy().into_owned())]))
        }
        Some(Typval::List(items)) => {
            let items = items.borrow();
            let mut values = items.items.iter().map(value_text).collect::<ox_eval::Result<Vec<_>>>()?;
            if values.first().is_none_or(String::is_empty) {
                return Err(EvalError::new("E474", 0, "Invalid argument"));
            }
            let program = PathBuf::from(values.remove(0));
            Ok((program, values.into_iter().map(OsString::from).collect()))
        }
        _ => Err(EvalError::new("E474", 0, "Invalid argument")),
    }
}

fn callback_option(value: Option<Typval>) -> ox_eval::Result<Option<Typval>> {
    match value {
        None | Some(Typval::Special(Special::Null)) => Ok(None),
        Some(value @ (Typval::String(_) | Typval::Funcref(_) | Typval::Partial(_))) => Ok(Some(value)),
        Some(_) => Err(EvalError::new("E921", 0, "Invalid callback argument")),
    }
}

fn invoke_job_events<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    events: Vec<JobEvent>,
) -> ox_eval::Result<()> {
    for event in events {
        let name = match event.callback {
            Typval::Funcref(funcref) | Typval::Partial(funcref) if funcref.registry.is_some() => {
                let reference = funcref.registry.expect("guarded Lua callback reference");
                let Some(lua) = lua else {
                    return Err(EvalError::new("E5108", 0, "Lua callback host is not installed"));
                };
                let args = event.args.iter().map(typval_to_object).collect();
                lua.borrow_mut()
                    .invoke_callback(editor, reference, args)
                    .map_err(|error| EvalError::new("E5108", 0, format!("{error:?}")))?;
                continue;
            }
            Typval::String(name) => name,
            Typval::Funcref(funcref) | Typval::Partial(funcref) => funcref.name,
            _ => continue,
        };
        call_user_function_with_self(
            runtime, editor, scope, lua, &name.to_string_lossy(), event.args, 1, 1,
            Some(event.receiver),
        )
        .map_err(|flow| flow_to_eval_error(flow, &name.to_string_lossy()))?;
    }
    Ok(())
}

fn job_id(value: Option<&Typval>) -> ox_eval::Result<u64> {
    let value = value.and_then(value_number).ok_or_else(|| EvalError::new("E475", 0, "Invalid argument: expected job id"))?;
    u64::try_from(value).map_err(|_| EvalError::new("E475", 0, "Invalid argument: expected job id"))
}

fn job_ids(value: Option<&Typval>) -> ox_eval::Result<Vec<u64>> {
    let Some(Typval::List(values)) = value else { return Err(EvalError::new("E714", 0, "List required")); };
    values.borrow().items.iter().map(|value| job_id(Some(value))).collect()
}

fn channel_bytes(value: Option<&Typval>) -> ox_eval::Result<Vec<u8>> {
    match value {
        Some(Typval::String(value)) => Ok(value.as_bytes().to_vec()),
        Some(Typval::Blob(value)) => Ok(value.clone()),
        Some(Typval::List(values)) => {
            let values = values.borrow();
            let mut bytes = Vec::new();
            for value in &values.items {
                bytes.extend_from_slice(value_text(value)?.as_bytes());
                bytes.push(b'\n');
            }
            Ok(bytes)
        }
        Some(value) => Ok(value_text(value)?.into_bytes()),
        None => Err(EvalError::new("E119", 0, "Not enough arguments")),
    }
}

fn value_text(value: &Typval) -> ox_eval::Result<String> {
    match value {
        Typval::String(value) => Ok(value.to_string_lossy().into_owned()),
        Typval::Number(value) => Ok(value.to_string()),
        Typval::Bool(value) => Ok(i64::from(*value).to_string()),
        _ => Err(EvalError::new("E730", 0, "Using a non-String as a String")),
    }
}

fn value_number(value: &Typval) -> Option<i64> {
    match value {
        Typval::Number(value) => Some(*value),
        Typval::Bool(value) => Some(i64::from(*value)),
        Typval::Job(value) | Typval::Channel(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

fn value_bool(value: &Typval) -> bool {
    value_number(value).is_some_and(|value| value != 0)
}

/// [`BufferHost`] adapter over the editor's current buffer, mapping the
/// evaluator's line-addressed builtins onto the single-writer line
/// mutations `Editor::replace_buffer_lines`/`append_buffer_lines`. Undo
/// timestamps match the other ex mutations here (0); the recorded cursor is
/// the window cursor, like `:substitute`.
struct CurrentBuffer<'a>(&'a mut Editor);

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
struct VimRegex;

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
        let found = regex_exec_at(
            &program,
            &RegexText::new(source),
            RegexPosition {
                lnum: 1,
                col: start,
                byte: start,
            },
        );
        Ok(found.map(|matched| (matched.start.byte, matched.end.byte)))
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
            let Some(matched) = regex_exec_at(
                &program,
                &regex_text,
                RegexPosition { lnum: 1, col: cursor, byte: cursor },
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

fn call_user_function_with_self<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    name: &str,
    args: Vec<Typval>,
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
        match eval_text(runtime, editor, scope, None, expression) {
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
                push_text_message(editor, text, false, false);
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
    name: &str,
    args: &str,
) -> Flow {
    let expressions = split_expressions(args);
    let mut pieces = Vec::new();
    for expression in expressions {
        let value = match eval_text(runtime, editor, scope, None, expression) {
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
    let mut pieces = Vec::new();
    for expression in split_expressions(args) {
        match eval_text(runtime, editor, scope, lua, expression) {
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

fn command_normal<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, args: &str) -> Flow {
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
        let Some(matched) = regex_exec_at(
            program,
            &text,
            RegexPosition { lnum: 1, col: cursor, byte: cursor },
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
    let text = match runtime.scripts.io().read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return error_flow(runtime, "E484", format!("Can't open file {}: {error}", path.display())),
    };
    let buffer_text = match Buffer::from_bytes(text.as_bytes()) {
        Ok(buffer) => buffer,
        Err(error) => return error_flow(runtime, "E474", error.to_string()),
    };
    let handle = match editor.create_buffer_with(buffer_text, true) {
        Ok(handle) => handle,
        Err(error) => return error_flow(runtime, "E948", error.to_string()),
    };
    if let Ok(buffer) = editor.buffer_mut(handle) {
        buffer.set_name(OxStr::from(path.to_string_lossy().as_ref()));
        buffer.mark_saved();
    }
    if editor.current_window().is_none() {
        match editor.create_tabpage(handle, crate::Geometry { row: 0, col: 0, width: 80, height: 24 }) {
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
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, "E90", error.to_string()),
    }
}

fn command_write<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
    let new_buffer = if command.args.trim().is_empty() {
        buffer
    } else {
        let path = PathBuf::from(command.args.trim());
        let text = match runtime.scripts.io().read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return error_flow(runtime, "E484", format!("Can't open file {}: {error}", path.display())),
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
            state.set_name(OxStr::from(path.to_string_lossy().as_ref()));
            state.mark_saved();
        }
        handle
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

fn command_close<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand, quit: bool) -> Flow {
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
    if editor.windows().len() == 1 {
        return if quit { Flow::Quit(0) } else { error_flow(runtime, "E444", "Cannot close last window") };
    }
    match editor.close_window(tab, window, true) {
        Ok(_) => Flow::Normal,
        Err(error) => error_flow(runtime, "E444", error.to_string()),
    }
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

fn command_buffer<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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

fn command_delete<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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

fn command_yank<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
fn command_print<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
        push_text_message(editor, message, false, false);
    }
    if let Some(window) = editor.current_window() {
        if let Err(error) = editor.set_window_cursor(window, Position { lnum: last, col: 0 }) {
            return error_flow(runtime, "E16", error.to_string());
        }
    }
    Flow::Normal
}

fn command_mark<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
    let Some(name) = command.args.trim().chars().next() else { return error_flow(runtime, "E191", "Argument must be a letter or forward/backward quote") };
    let buffer = match editor.current_buffer() { Some(buffer) => buffer, None => return error_flow(runtime, "E20", "Mark not set") };
    let position = editor.current_window().and_then(|window| editor.window(window).ok()).map_or(Position { lnum: 1, col: 0 }, |window| window.cursor);
    match editor.set_local_mark(buffer, name, position) { Ok(_) => Flow::Normal, Err(error) => error_flow(runtime, "E191", error.to_string()) }
}

fn command_marks<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor) -> Flow {
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

fn command_registers<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, args: &str) -> Flow {
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
            scheme = Some((vim, false));
            break;
        }
        let lua = base.with_extension("lua");
        if runtime.scripts.io().exists(&lua) {
            scheme = Some((lua, true));
            break;
        }
    }

    let flow = if let Some((path, false)) = scheme {
        match source_path(runtime, editor, scope, lua, &path, false) {
            Ok(Flow::Finish) => Flow::Normal,
            Ok(flow) => flow,
            Err(error) => exec_error_flow(runtime, error),
        }
    } else if let Some((path, true)) = scheme {
        let Some(lua) = lua else { return Flow::NotImplemented("luafile".to_owned()) };
        if let Err(error) = sync_scope_into_editor(editor, scope) {
            return exec_error_flow(runtime, error);
        }
        let result = lua.borrow_mut().execute_file(editor, &path);
        let sync = sync_editor_into_scope(editor, scope);
        match (result, sync) {
            (Err(error), _) => lua_error_flow(runtime, error, "E5112", "E5113"),
            (Ok(()), Err(error)) => exec_error_flow(runtime, error),
            (Ok(()), Ok(())) => Flow::Normal,
        }
    } else {
        return error_flow(runtime, "E185", format!("Cannot find color scheme '{name}'"));
    };
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
    runtime: &ExRuntime<F>,
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

fn command_highlight<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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
    if runtime.user_commands.commands.contains_key(name) && !command.bang { return error_flow(runtime, "E174", "Command already exists: add ! to replace it") }
    let body = words.collect::<Vec<_>>().join(" ");
    runtime.user_commands.commands.insert(name.to_owned(), UserCommand { name: name.to_owned(), body, nargs, accepts_bang, accepts_range, accepts_register });
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

fn command_map<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, command: &ExCommand) -> Flow {
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

fn sync_editor_into_scope(editor: &Editor, scope: &mut Scope) -> Result<(), ExecError> {
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

fn sync_scope_into_editor(editor: &mut Editor, scope: &Scope) -> Result<(), ExecError> {
    *editor.gvars_mut() = scope_to_dict(&scope.global);
    if let Some(buffer) = editor.current_buffer() { *editor.buffer_mut(buffer).map_err(|error| ExecError::Editor(error.to_string()))?.variables_mut() = scope_to_dict(&scope.buffer); }
    if let Some(window) = editor.current_window() { *editor.window_variables_mut(window).map_err(|error| ExecError::Editor(error.to_string()))? = scope_to_dict(&scope.window); }
    if let Some(tab) = editor.current_tabpage() { *editor.tabpage_variables_mut(tab).map_err(|error| ExecError::Editor(error.to_string()))? = scope_to_dict(&scope.tab); }
    *editor.vvars_mut() = scope_to_dict(&scope.vim);
    Ok(())
}

fn assign_target<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, target: &str, value: Typval, _constant: bool) -> Result<(), Flow> {
    let target = target.trim();
    if let Some(register) = target.strip_prefix('@').and_then(|name| name.chars().next()) {
        let content = RegisterContent::characterwise(typval_to_text(&value).as_bytes()).map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
        editor.registers_mut().set(register, content).map_err(|error| error_flow(runtime, "E354", error.to_string()))?;
        scope.set_register(&[register as u8], value);
        return Ok(());
    }
    if let Some(environment) = target.strip_prefix('$') {
        scope.set_env(
            environment.as_bytes(),
            Typval::String(OxStr(typval_to_text(&value).into_bytes())),
        );
        return Ok(());
    }
    if let Some(option) = target.strip_prefix('&') { return assign_option(runtime, editor, scope, option, value); }
    let (kind, name) = parse_scope_name(target);
    if kind == Some(ScopeKind::Vim) && vim_variable_is_writable(name.as_bytes()) {
        replace_scope_pair(&mut scope.vim, &name, value);
    } else if let Some(kind) = kind {
        scope.set_scoped(kind, name.as_bytes(), 0, value).map_err(|error| eval_error_flow(runtime, error))?;
    } else {
        scope.set(name.as_bytes(), value);
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

fn read_target<F: FileIO>(runtime: &ExRuntime<F>, editor: &Editor, scope: &Scope, target: &str) -> Result<Typval, Flow> {
    let target = target.trim();
    if let Some(register) = target.strip_prefix('@').and_then(|name| name.chars().next()) { return Ok(scope.get_register(&[register as u8])); }
    if let Some(environment) = target.strip_prefix('$') { return Ok(scope.get_env(environment.as_bytes())); }
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
    if let Some(environment) = target.strip_prefix('$') { return remove_scope_pair(&mut scope.env, environment); }
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

fn apply_assignment_operator<F: FileIO>(runtime: &ExRuntime<F>, left: Typval, right: Typval, operator: &str) -> Result<Typval, Flow> {
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
fn apply_option_assignment_operator<F: FileIO>(runtime: &ExRuntime<F>, current: Typval, operand: Typval, operator: &str) -> Result<Typval, Flow> {
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
    let current = editor.current_window().and_then(|window| editor.window(window).ok()).map_or(1, |window| window.cursor.lnum);
    let last = editor.current_buffer().and_then(|buffer| editor.buffer(buffer).ok()).and_then(|state| state.text().ok()).map_or(1, Buffer::line_count);
    let Some(range) = &command.range else { return Ok((current, current)); };
    if matches!(range.kind, RangeKind::WholeBuffer) { return Ok((1, last)); }
    let start = range.start.as_ref().map_or(Ok(current), |address| resolve_address(editor, address, current, last))?;
    let end = range.end.as_ref().map_or(Ok(start), |address| resolve_address(editor, address, current, last))?;
    if start > end { return Err("Invalid range".to_owned()); }
    Ok((start.max(1), end.min(last)))
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

fn split_expressions(source: &str) -> Vec<&str> {
    split_top_level(source, b' ', false)
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
    while cursor <= source.len() { let Some(matched) = regex_exec_at(&program, &text, RegexPosition { lnum: 1, col: cursor, byte: cursor }) else { break; }; output.push_str(&source[previous..matched.start.byte]); let mut groups = vec![source[matched.start.byte..matched.end.byte].to_owned()]; for capture in &matched.captures { groups.push(capture.as_ref().map_or_else(String::new, |capture| source[capture.start.byte..capture.end.byte].to_owned())); } output.push_str(&expand_replacement(replacement, &groups)); previous = matched.end.byte; if !global { break; } cursor = if matched.start.byte == matched.end.byte { next_boundary(source, matched.end.byte) } else { matched.end.byte }; if cursor > source.len() { break; } }
    output.push_str(&source[previous..]); Ok(output)
}

fn next_boundary(text: &str, at: usize) -> usize { if at >= text.len() { return text.len().saturating_add(1); } at + text[at..].chars().next().map_or(1, char::len_utf8) }

fn typval_to_text(value: &Typval) -> String { match value { Typval::String(value) => value.to_string_lossy().into_owned(), _ => typval_to_display(value, false) } }

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

fn typval_number(value: &Typval) -> Option<i64> { match value { Typval::Number(value) => Some(*value), Typval::Bool(value) => Some(i64::from(*value)), Typval::String(value) => value.to_string_lossy().parse().ok(), Typval::Channel(value) | Typval::Job(value) => i64::try_from(*value).ok(), _ => None } }
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

fn option_to_typval(value: &OptionValue) -> Typval { match value { OptionValue::Boolean(value) => Typval::Number(i64::from(*value)), OptionValue::Number(value) => Typval::Number(*value), OptionValue::String(value) => Typval::String(OxStr::from(value.as_str())) } }

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

fn typval_to_option(value: &Typval, value_type: OptionType) -> Result<OptionValue, String> { match value_type { OptionType::Boolean => typval_number(value).map(|value| OptionValue::Boolean(value != 0)).ok_or_else(|| "Number required".to_owned()), OptionType::Number => typval_number(value).map(OptionValue::Number).ok_or_else(|| "Number required".to_owned()), OptionType::String => Ok(OptionValue::String(typval_to_text(value))) } }

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
    if let Some(name) = raw.strip_suffix('?') { if let Some(text) = display_option(editor, name, layer) { editor.push_message(Message { kind: MessageKind::Echo, content: Object::String(OxStr(text.into_bytes())), history: false }); return Ok(()); } return Err(("E518", format!("Unknown option: {name}"))); }
    if let Some(name) = raw.strip_suffix("&vim").or_else(|| raw.strip_suffix('&')) { let metadata = crate::option_metadata(name).ok_or_else(|| ("E518", format!("Unknown option: {name}")))?; let value = metadata.default.value.map(OptionValue::from).ok_or_else(|| ("E474", format!("No literal default for {name}")))?; return set_and_mirror(editor, scope, metadata.name, value, layer); }
    for operator in ["+=", "-=", "^=", "="] { if let Some((name, value)) = raw.split_once(operator) { let metadata = crate::option_metadata(name).ok_or_else(|| ("E518", format!("Unknown option: {name}")))?; let mut next = match metadata.value_type { OptionType::Boolean => OptionValue::Boolean(matches!(value, "1" | "true" | "on")), OptionType::Number => OptionValue::Number(value.parse().map_err(|_| ("E521", format!("Number required after =: {value}")))?), OptionType::String => OptionValue::String(if metadata.expand { expand_set_value(value) } else { value.to_owned() }) }; if operator != "=" { let current = option_value(editor, metadata.name, layer).cloned().unwrap_or_else(|| metadata.default.value.map(OptionValue::from).unwrap_or(OptionValue::String(String::new()))); next = modify_option(current, next, operator, metadata.list)?; } return set_and_mirror(editor, scope, metadata.name, next, layer); } }
    let (name, value) = if let Some(name) = raw.strip_prefix("no") { (name, false) } else if let Some(name) = raw.strip_prefix("inv") { let current = option_value(editor, name, layer).and_then(|value| match value { OptionValue::Boolean(value) => Some(*value), _ => None }).unwrap_or(false); (name, !current) } else if let Some(name) = raw.strip_suffix('!') { let current = option_value(editor, name, layer).and_then(|value| match value { OptionValue::Boolean(value) => Some(*value), _ => None }).unwrap_or(false); (name, !current) } else { (raw, true) };
    let metadata = crate::option_metadata(name).ok_or_else(|| ("E518", format!("Unknown option: {name}")))?;
    if metadata.value_type != OptionType::Boolean { if let Some(text) = display_option(editor, name, layer) { editor.push_message(Message { kind: MessageKind::Echo, content: Object::String(OxStr(text.into_bytes())), history: false }); return Ok(()); } }
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

/// `:set` value expansion for `expand`-flag options (option.c
/// `stropt_expand_envvar` → `expand_env_esc`): a leading `~` resolves through
/// `$HOME`, and each `$NAME`/`${NAME}` resolves through the process
/// environment. An unset variable stays literal, matching upstream
/// `vim_getenv` returning NULL; substituted text is never rescanned.
fn expand_set_value(value: &str) -> String {
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

fn buffer_lines(editor: &Editor, buffer: BufHandle) -> Result<Vec<Vec<u8>>, String> {
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
fn object_to_typval(value: &Object) -> Typval {
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
fn typval_to_object(value: &Typval) -> Object {
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

fn replace_scope_pair(map: &mut ScopeMap, name: &str, value: Typval) -> Option<Typval> {
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

fn push_text_message(editor: &mut Editor, text: String, error: bool, history: bool) {
    editor.push_message(Message {
        kind: if error { MessageKind::Error } else { MessageKind::Echo },
        content: Object::String(OxStr(text.into_bytes())),
        history,
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
fn exec_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: ExecError) -> Flow {
    match error {
        ExecError::Vim(exception) => Flow::Exception(exception),
        ExecError::NotImplemented(name) => Flow::NotImplemented(name),
        ExecError::Eval(error) => eval_error_flow(runtime, error),
        ExecError::Parse(error) => error_flow(runtime, error.code.as_str(), error.message),
        ExecError::Io { path, message } => error_flow(runtime, "E484", format!("{}: {message}", path.display())),
        ExecError::Editor(message) => error_flow(runtime, "E605", message),
    }
}
fn flow_to_eval_error(flow: Flow, name: &str) -> EvalError {
    match flow {
        Flow::Exception(exception) => {
            EvalError::new("E605", 0, exception.message())
        }
        Flow::NotImplemented(name) => EvalError::not_implemented(OxStr(name.into_bytes())),
        _ => EvalError::new("E117", 0, format!("Unknown function: {name}")),
    }
}

fn command_lua<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, command: &ExCommand) -> Flow {
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

fn command_luafile<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, command: &ExCommand) -> Flow {
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

fn command_luado<F: FileIO>(runtime: &ExRuntime<F>, editor: &mut Editor, scope: &mut Scope, lua: Option<&Rc<RefCell<dyn LuaExec>>>, command: &ExCommand) -> Flow {
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

fn lua_error_flow<F: FileIO>(runtime: &ExRuntime<F>, error: LuaExecError, load_code: &'static str, runtime_code: &'static str) -> Flow {
    match error {
        LuaExecError::Load(message) => error_flow(runtime, load_code, message),
        LuaExecError::Runtime(message) | LuaExecError::Conversion(message) => {
            error_flow(runtime, runtime_code, message)
        }
    }
}

fn exists_with_editor<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &Editor,
    scope: &Scope,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    let value = args.first().cloned().unwrap_or(Typval::String(OxStr::from("")));
    let operand = typval_to_text(&value);
    let result = if let Some(option) = operand.strip_prefix('&').or_else(|| operand.strip_prefix('+')) {
        let option = option.strip_prefix("g:").or_else(|| option.strip_prefix("l:")).unwrap_or(option);
        i64::from(crate::options::OptionStore::metadata(option).is_ok())
    } else if let Some(name) = operand.strip_prefix('*') {
        let sid = runtime.functions.active_sid().or_else(|| runtime.scripts.current_sid()).unwrap_or(0);
        i64::from(builtin_spec(name).is_some() || runtime.functions.contains(name, sid))
    } else if let Some(name) = operand.strip_prefix(':') {
        match resolve_command(name, &runtime.user_commands) {
            Ok(command) => if command.name() == name { 2 } else { 1 },
            Err(ResolveError::AmbiguousUserCommand) => 3,
            Err(ResolveError::NotFound) => 0,
        }
    } else if let Some(event) = operand.strip_prefix("##") {
        i64::from(Event::from_name(event).is_some())
    } else if let Some(query) = operand.strip_prefix('#') {
        i64::from(editor.autocmds().exists(query))
    } else {
        return exists_in_scope(&value, scope);
    };
    Ok(Typval::Number(result))
}