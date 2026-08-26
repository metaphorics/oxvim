//! Expression- and script-evaluating builtins: they re-enter the parser, the
//! Ex interpreter, the Lua host, or the typeahead queue (upstream `eval.c`,
//! `userfunc.c`).

use std::cell::RefCell;
use std::rc::Rc;
use ox_eval::builtin_spec;
use ox_eval::exists as exists_in_scope;
use ox_eval::Evaluator;
use ox_eval::Parser as ExprParser;
use ox_eval::EvalError;
use ox_eval::Scope;
use ox_excmd::{resolve_command, ResolveError, ResolvedCommand};
use ox_types::{Funcref, Object, OxStr, Special, Typval};
use crate::script::{FileIO, LogicalLine};
use crate::autocmd::Event;
use crate::typeahead::Keys;
use crate::{Editor, ModeMachine};

use crate::excmd_exec::{EvalHost, drain_typeahead, ExRuntime, Flow, LuaExec, LuaExecError, VimRegex, exec_error_flow, expand_env_esc, flow_to_eval_error, parse_program, run_program, sync_editor_into_scope, sync_scope_into_editor, typval_number, typval_to_text};
use super::{input_string_arg};

/// Routes one expression- or script-evaluating builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "eval" => call_eval_builtin(host, args, scope),
        "execute" => call_execute_builtin(host.runtime, host.editor, scope, host.lua, args),
        "exists" => exists_with_editor(host.runtime, host.editor, scope, args),
        "expand" => call_expand_builtin(host.runtime, host.editor, args),
        "feedkeys" => call_feedkeys_builtin(host.runtime, host.editor, scope, host.lua, args),
        "fullcommand" => call_fullcommand_builtin(host.runtime, args),
        "function" | "funcref" => call_function_builtin(host.runtime, name, args),
        "luaeval" => call_luaeval_builtin(host.runtime, host.editor, scope, host.lua, args),
        "submatch" => Ok(call_submatch_builtin(host.submatches.as_deref(), &args)),
        _ => unreachable!("eval builtin route and dispatcher disagree"),
    }
}

/// `eval()`: parse the argument and evaluate it against this same host, so the
/// editor seams stay reachable from the nested expression (`f_eval`).
fn call_eval_builtin<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let [Typval::String(source)] = args.as_slice() else {
        return Err(EvalError::new("E119", 0, "One string argument required"));
    };
    let expression = ExprParser::new(source.as_bytes()).parse()?;
    let regex = VimRegex;
    Evaluator::new(host, &regex).eval(&expression, scope)
}

/// `submatch()`: the groups captured by the `:substitute` whose replacement
/// expression is running; outside one every index reads empty (`f_submatch`).
fn call_submatch_builtin(submatches: Option<&[String]>, args: &[Typval]) -> Typval {
    let index = args.first().and_then(typval_number).unwrap_or(0).max(0) as usize;
    let value = submatches
        .and_then(|groups| groups.get(index))
        .cloned()
        .unwrap_or_default();
    Typval::String(OxStr(value.into_bytes()))
}

fn call_execute_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new("E119", 0, "Not enough arguments for function: execute"));
    }
    if args.len() > 2 {
        return Err(EvalError::new("E118", 0, "Too many arguments for function: execute"));
    }
    // A List argument is not stringified: `execute_common` (`eval/funcs.c`
    // 1206-1216) hands `do_cmdline` a `get_list_line` cookie, so every item is
    // its own source line and multi-line constructs such as `:if`/`:endif`
    // work. Only the non-list form goes through `do_cmdline_cmd` as one line.
    let logical = match &args[0] {
        Typval::List(items) => {
            let text = items
                .borrow()
                .items
                .iter()
                .map(typval_to_text)
                .collect::<Vec<String>>()
                .join("\n");
            runtime
                .scripts
                .join_logical_lines(&text)
                .map_err(|error| EvalError::new("E488", 0, error.to_string()))?
        }
        command => vec![LogicalLine {
            text: typval_to_text(command),
            first_line: runtime.scripts.current_line(),
        }],
    };
    let program = parse_program(&runtime.user_commands, &logical)
        .map_err(|error| EvalError::new("E488", 0, error.to_string()))?;
    let message_start = editor.messages().len();
    let flow = run_program(runtime, editor, scope, lua, &program, 0, program.len());
    if !matches!(flow, Flow::Normal) {
        return Err(flow_to_eval_error(flow, "execute"));
    }
    let mut output = String::new();
    for message in &editor.messages()[message_start..] {
        let Object::String(text) = &message.content else { continue };
        output.push('\n');
        output.push_str(&text.to_string_lossy());
    }
    editor.truncate_messages(message_start);
    Ok(Typval::String(OxStr(output.into_bytes())))
}

/// `luaeval({expr}[, {arg}])`: eval/funcs.c `f_luaeval` → lua/executor.c
/// `nlua_call_luaeval`. The host compiles `local _A=select(1,...) return
/// (<expr>)` and converts the argument and result with typval semantics.
/// Errors surface as E5107 (load) / E5108 (runtime) with the upstream
/// `Lua:` message prefix.
fn call_luaeval_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    let Some(lua) = lua else {
        return Err(EvalError::not_implemented(OxStr::from("luaeval")));
    };
    let Some(spec) = builtin_spec("luaeval") else {
        return Err(EvalError::not_implemented(OxStr::from("luaeval")));
    };
    if args.len() < spec.min_args {
        return Err(EvalError::new("E119", 0, "Not enough arguments for function: luaeval"));
    }
    if spec.max_args.is_some_and(|maximum| args.len() > maximum) {
        return Err(EvalError::new("E118", 0, "Too many arguments for function: luaeval"));
    }
    // f_luaeval reads the expression through tv_get_string_chk.
    let expression = match &args[0] {
        Typval::String(value) => value.to_string_lossy().into_owned(),
        Typval::Number(value) => value.to_string(),
        Typval::Bool(value) => OxStr::from(if *value { "v:true" } else { "v:false" }).to_string_lossy().into_owned(),
        Typval::Special(Special::Null) => "v:null".to_owned(),
        // `tv_get_string_buf_chk` (`typval.c:4684-4685`) renders a Float with
        // `%g`; E806 belongs only to `check_can_index` (`eval.c:3225-3229`).
        Typval::Float(number) => ox_eval::float_as_string(*number).to_string_lossy().into_owned(),
        Typval::List(_) => {
            return Err(EvalError::new("E730", 0, "Using a List as a String"));
        }
        Typval::Dict(_) => {
            return Err(EvalError::new("E731", 0, "Using a Dictionary as a String"));
        }
        _ => return Err(EvalError::new("E729", 0, "Using invalid value as a String")),
    };
    // The Lua host reads and writes editor variables (`vim.g` inside the
    // expression), so live Ex variables are synchronized in and back out
    // exactly like the `:lua` command path.
    if let Err(error) = sync_scope_into_editor(editor, scope) {
        return Err(flow_to_eval_error(exec_error_flow(runtime, error), "luaeval"));
    }
    let result = lua.borrow_mut().eval_expression(editor, &expression, args.get(1));
    let sync = sync_editor_into_scope(editor, scope);
    match (result, sync) {
        (Err(LuaExecError::Load(message)), _) => {
            Err(EvalError::new("E5107", 0, format!("Lua: {message}")))
        }
        (Err(LuaExecError::Runtime(message)) | Err(LuaExecError::Conversion(message)), _) => {
            Err(EvalError::new("E5108", 0, format!("Lua: {message}")))
        }
        (Ok(_), Err(error)) => Err(flow_to_eval_error(exec_error_flow(runtime, error), "luaeval")),
        (Ok(value), Ok(())) => Ok(value),
    }
}

/// Leading cmdline sp_token table for `expand()` (`find_cmdline_var`,
/// ex_docmd.c:7488), limited to the tokens this port resolves. Ordered
/// longest-first so a later `<argname>`-style addition cannot be shadowed.
const EXPAND_SPECIAL_TOKENS: &[&str] = &["<amatch>", "<afile>", "<abuf>", "<SID>", "%"];

/// The `eval_vars` (ex_docmd.c:7551) bases behind `expand()`'s special
/// tokens. A token this port does not know yields an empty base.
fn expand_special_base<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &Editor,
    token: &str,
) -> String {
    match token {
        "%" => editor
            .current_buffer()
            .and_then(|buffer| editor.buffer(buffer).ok())
            .map_or_else(String::new, |buffer| buffer.name().to_string_lossy().into_owned()),
        "<SID>" => runtime
            .functions
            .active_sid()
            .or_else(|| runtime.scripts.current_sid())
            .map_or_else(String::new, |sid| format!("<SNR>{sid}_")),
        "<amatch>" => runtime.active_autocmd.matched.clone(),
        "<afile>" => runtime.active_autocmd.file.clone(),
        "<abuf>" => runtime
            .active_autocmd
            .buffer
            .map_or_else(String::new, |buffer| i64::from(buffer).to_string()),
        _ => String::new(),
    }
}

/// `expand()` (`f_expand`).
///
/// `%`, `#` and a `<...>` keyword go to `eval_vars`; this port resolves
/// `%`, `<SID>`, `<amatch>`, `<afile>` and `<abuf>` as leading tokens with
/// an optional trailing `:`-modifier chain applied through
/// `ox_eval::apply_filename_modifiers` (`modify_fname` parity), so
/// `expand('%:p')` is the absolute current-buffer name. Anything else is a
/// file pattern handed to `ExpandOne`, which resolves `~` and `$NAME`
/// through `expand_env_esc` before matching. Returning such a pattern
/// verbatim leaves `expand('~')` as the literal `~`, and callers that hand
/// the result to a shell -- `runtest.vim`'s `system('rm -rf  ' .. file)` --
/// then let the *shell* expand it against its own environment.
///
/// Named gap: the wildcard half of `ExpandOne` is not here, so a pattern
/// with `*` or `?` still comes back as itself; `glob()` is where this port
/// matches files. `#` and `<cword>`/`<sfile>`-style tokens are likewise
/// not recognized and pass through verbatim, and the `%<` extension-strip
/// form is not implemented.
fn call_expand_builtin<F: FileIO>(
    runtime: &ExRuntime<F>,
    editor: &Editor,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    let [Typval::String(value), ..] = args.as_slice() else {
        return Err(EvalError::new("E730", 0, "Using a List as a String"));
    };
    let text = value.to_string_lossy();
    let text: &str = text.as_ref();
    let special = EXPAND_SPECIAL_TOKENS
        .iter()
        .find_map(|token| text.strip_prefix(token).map(|rest| (*token, rest)));
    let expanded = match special {
        // Exact token or token-plus-`:`-modifier chain (`eval_vars` +
        // `modify_fname`).
        Some((token, rest)) if rest.is_empty() || rest.starts_with(':') => {
            let base = expand_special_base(runtime, editor, token);
            // `expand()` on an unnamed source yields "" (upstream f_expand:
            // eval_vars marks the result invalid and f_expand returns "").
            if rest.is_empty() || base.is_empty() {
                base
            } else {
                ox_eval::apply_filename_modifiers(Some(&VimRegex), &base, rest.as_bytes())?
            }
        }
        _ => expand_env_esc(text),
    };
    Ok(Typval::String(OxStr(expanded.into_bytes())))
}

fn call_function_builtin<F: FileIO>(runtime: &mut ExRuntime<F>, kind: &str, mut args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 3 {
        return Err(EvalError::new(if args.is_empty() { "E119" } else { "E118" }, 0, format!("Invalid arguments for {kind}")));
    }
    let first = args.remove(0);
    let mut function = match first {
        Typval::Funcref(function) | Typval::Partial(function) => function,
        Typval::String(name) => {
            let text = name.to_string_lossy();
            if text.is_empty() || text.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                return Err(EvalError::new("E475", 0, format!("Invalid argument: {text}")));
            }
            let sid = runtime.functions.active_sid().or_else(|| runtime.scripts.current_sid()).unwrap_or(0);
            if builtin_spec(&text).is_none() && !runtime.functions.contains(&text, sid) && !text.contains('#') {
                return Err(EvalError::new("E700", 0, format!("Unknown function: {text}")));
            }
            Funcref { name, args: Vec::new(), dict: None, registry: None }
        }
        other => {
            let name = input_string_arg(&other)?;
            return Err(EvalError::new("E475", 0, format!("Invalid argument: {}", name.to_string_lossy())));
        }
    };

    let mut bound = None;
    let mut dictionary = None;
    if let Some(second) = args.first() {
        match second {
            Typval::List(reference) => {
                bound = Some(reference.try_borrow().map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?.items.clone());
            }
            Typval::Dict(reference) if args.len() == 1 => {
                dictionary = Some(reference.try_borrow().map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?.entries.clone());
            }
            Typval::Dict(_) => return Err(EvalError::new("E923", 0, "Second argument of function() must be a list or a dict")),
            _ => return Err(EvalError::new("E923", 0, "Second argument of function() must be a list or a dict")),
        }
    }
    if let Some(third) = args.get(1) {
        let Typval::Dict(reference) = third else { return Err(EvalError::new("E922", 0, "Expected a dict")); };
        dictionary = Some(reference.try_borrow().map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?.entries.clone());
    }
    if let Some(mut values) = bound { function.args.append(&mut values); }
    if dictionary.is_some() { function.dict = dictionary; }
    let partial = kind == "funcref" || !function.args.is_empty() || function.dict.is_some();
    Ok(if partial { Typval::Partial(function) } else { Typval::Funcref(function) })
}

fn call_fullcommand_builtin<F: FileIO>(runtime: &ExRuntime<F>, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    let Some(Typval::String(command)) = args.first() else { return Ok(Typval::String(OxStr(Vec::new()))); };
    let command = command.to_string_lossy();
    let resolved = resolve_command(&command, &runtime.user_commands).ok().map_or_else(String::new, |command| command.name().to_owned());
    Ok(Typval::String(OxStr::from(resolved.as_str())))
}

fn call_feedkeys_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::new(if args.is_empty() { "E119" } else { "E118" }, 0, "Invalid arguments for feedkeys"));
    }
    let keys = input_string_arg(&args[0])?;
    let mode = args.get(1).map(input_string_arg).transpose()?.unwrap_or_else(|| OxStr::from("m"));
    let execute = editor.typeahead_mut().feedkeys(&Keys::from_encoded(keys.as_bytes().to_vec()).map_err(|error| EvalError::new("E475", 0, error.to_string()))?, &mode.to_string_lossy()).map_err(|error| EvalError::new("E475", 0, error.to_string()))?;
    if execute {
        let mut machine = ModeMachine::default();
        let flow = drain_typeahead(runtime, editor, scope, lua, &mut machine);
        if !matches!(flow, Flow::Normal) {
            return Err(flow_to_eval_error(flow, "feedkeys"));
        }
    }
    Ok(Typval::Number(0))
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
        i64::from(is_callable_function(name) || runtime.functions.contains(name, sid))
    } else if let Some(name) = operand.strip_prefix(':') {
        match resolve_command(name, &runtime.user_commands) {
            Ok(ResolvedCommand::Builtin(spec)) if !is_executed_command(spec.name) => 0,
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

/// Whether a builtin function name can actually be *called* here.
///
/// `f_exists`'s `*` form asks `function_exists`, not the metadata table, so the
/// answer is the union of the two dispatchers that serve builtins: the
/// typval-only one in `ox-eval` ([`ox_eval::is_builtin_implemented`]) and the
/// editor-stateful families in [`crate::builtins::route`]. Every other name in
/// the generated `eval.lua` table resolves to an `E117: not implemented` arm,
/// and answering 1 for those makes `check.vim`'s `CheckFunction` inert: the
/// guarded file runs code that cannot work instead of skipping honestly.
fn is_callable_function(name: &str) -> bool {
    ox_eval::is_builtin_implemented(name) || crate::builtins::route(name).is_some()
}

/// Whether this port executes the Ex command `name`, as opposed to merely
/// resolving it out of the 564-entry generated `COMMANDS` table.
///
/// `cmd_exists` (`ex_docmd.c:3226`) answers from upstream's own table, where
/// resolving and executing are the same question. Here they are not: an
/// unhandled name reaches `Flow::NotImplemented` (`excmd_exec.rs:1037`), so
/// `exists(':wshada')` answering 2 makes `CheckCommand` inert the same way.
///
/// The three arms below are the three places a command name is served, and each
/// is derived from that place — re-derive them there when it changes:
///  * `dispatch` (`excmd_exec.rs:903-1034`), one arm per name;
///  * `run_program` (`excmd_exec.rs:682-872`), the control-flow openers it
///    interprets before `dispatch` is reached, plus the closers it consumes;
///  * [`ox_excmd::ModifierKind`], the modifiers the Ex parser recognises.
///    Upstream answers for modifiers out of `cmdmods` before it consults the
///    command table, and a modifier has no execution separate from the command
///    it decorates, so recognition is the whole question for them.
fn is_executed_command(name: &str) -> bool {
    matches!(name,
        "Next" | "argdelete" | "argdo" | "args" | "augroup" | "aunmenu" | "autocmd" |
        "bdelete" | "bnext" | "bprevious" | "break" | "buffer" | "bunload" | "bwipeout" |
        "call" | "cd" | "close" | "cmap" | "cmapclear" | "cnoremap" | "colorscheme" |
        "comclear" | "command" | "const" | "continue" | "cquit" | "cunmap" | "delcommand" |
        "delete" | "delfunction" | "display" | "echo" | "echoerr" | "echohl" | "echomsg" |
        "echon" | "edit" | "enew" | "eval" | "execute" | "filetype" | "finish" | "fold" |
        "foldclose" | "foldopen" | "global" | "hide" | "highlight" | "imap" | "imapclear" |
        "inoremap" | "insert" | "iunmap" | "k" | "language" | "lcd" | "let" | "lmap" |
        "lmapclear" | "lnoremap" | "lockvar" | "lua" | "luado" | "luafile" | "lunmap" | "map" |
        "mapclear" | "mark" | "marks" | "new" | "next" | "nmap" | "nmapclear" | "nnoremap" |
        "noremap" | "normal" | "nunmap" | "omap" | "omapclear" | "only" | "onoremap" |
        "ounmap" | "previous" | "print" | "put" | "qall" | "quit" | "read" | "redir" | "redo" |
        "redraw" | "redrawstatus" | "redrawtabline" | "registers" | "resize" | "retab" |
        "return" | "scriptencoding" | "set" | "setglobal" | "setlocal" | "sleep" | "smap" |
        "smapclear" | "snoremap" | "source" | "split" | "substitute" | "sunmap" | "swapname" |
        "syntax" | "tabedit" | "tabnew" | "tabonly" | "throw" | "tlunmenu" | "tmap" |
        "tmapclear" | "tnoremap" | "tunmap" | "undo" | "undojoin" | "unlet" | "unlockvar" |
        "unmap" | "vglobal" | "vmap" | "vmapclear" | "vnew" | "vnoremap" | "vsplit" |
        "vunmap" | "wincmd" | "wq" | "write" | "xit" | "xmap" | "xmapclear" | "xnoremap" |
        "xunmap" | "yank" | "z"
    ) || matches!(name,
        "catch" | "else" | "elseif" | "endfor" | "endfunction" | "endif" | "endtry" |
        "endwhile" | "finally" | "for" | "function" | "if" | "try" | "while"
    ) || matches!(name,
        "aboveleft" | "belowright" | "botright" | "browse" | "confirm" | "filter" |
        "horizontal" | "keepalt" | "keepjumps" | "keepmarks" | "keeppatterns" | "leftabove" |
        "lockmarks" | "noautocmd" | "noswapfile" | "rightbelow" | "sandbox" | "silent" |
        "tab" | "topleft" | "unsilent" | "verbose" | "vertical"
    )
}
