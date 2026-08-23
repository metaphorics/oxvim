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
use ox_excmd::{resolve_command, ResolveError};
use ox_types::{Funcref, Object, OxStr, Special, Typval};
use crate::script::{FileIO, LogicalLine};
use crate::autocmd::Event;
use crate::typeahead::Keys;
use crate::mapping::MappingAction;
use crate::{Editor, ModeMachine};

use crate::excmd_exec::{EvalHost, program_from_commands, ExRuntime, Flow, LuaExec, LuaExecError, VimRegex, exec_error_flow, expand_env_esc, flow_to_eval_error, parse_program, run_program, sync_editor_into_scope, sync_scope_into_editor, typval_number, typval_to_text};
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
    let command = typval_to_text(&args[0]);
    let logical = vec![LogicalLine {
        text: command,
        first_line: runtime.scripts.current_line(),
    }];
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
        Typval::Float(_) => {
            return Err(EvalError::new("E806", 0, "Using a Float as a String"));
        }
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

/// `expand()` (`f_expand`).
///
/// `%`, `#` and a `<...>` keyword go to `eval_vars`; anything else is a file
/// pattern handed to `ExpandOne`, which resolves `~` and `$NAME` through
/// `expand_env_esc` before matching. Returning such a pattern verbatim leaves
/// `expand('~')` as the literal `~`, and callers that hand the result to a
/// shell -- `runtest.vim`'s `system('rm -rf  ' .. file)` -- then let the
/// *shell* expand it against its own environment.
///
/// Named gap: the wildcard half of `ExpandOne` is not here, so a pattern with
/// `*` or `?` still comes back as itself; `glob()` is where this port matches
/// files.
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
        pattern => expand_env_esc(pattern),
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
        while !editor.typeahead().is_empty() {
            if !machine.run_once(editor).map_err(|error| EvalError::new("E523", 0, error.to_string()))? { break; }
            if let Some(command) = machine.take_ex_command() {
                let logical = vec![LogicalLine { text: command, first_line: runtime.scripts.current_line().max(1) }];
                let program = parse_program(&runtime.user_commands, &logical)
                    .map_err(|error| EvalError::new("E488", 0, error.to_string()))?;
                if let Flow::Exception(exception) = run_program(runtime, editor, scope, lua, &program, 0, program.len()) {
                    return Err(EvalError::new("E605", 0, exception.message()));
                }
            }
            // A mapping whose right-hand side is an Ex command reaches the
            // host as a pending action rather than as keys, so it has to be
            // run here too; without this the mapping is silently dropped and
            // `feedkeys()` cannot observe a mapped `:call`. `oxvim`'s server
            // loop does the same at `server.rs:612-627`.
            if let Some(MappingAction::ExCommands(commands)) = machine.take_mapping_action() {
                let program =
                    program_from_commands(&commands, runtime.scripts.current_line().max(1));
                if let Flow::Exception(exception) =
                    run_program(runtime, editor, scope, lua, &program, 0, program.len())
                {
                    return Err(EvalError::new("E605", 0, exception.message()));
                }
            }
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
