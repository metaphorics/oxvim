//! `assert_*` builtins: they evaluate a claim, append the failure text to
//! `v:errors`, and echo it (upstream `testing.c`).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ox_eval::builtin_spec;
use ox_eval::EvalError;
use ox_eval::Scope;
use ox_eval::RegexEngine;
use ox_eval::ScopeKind;
use ox_types::{OxStr, Typval};
use crate::script::{FileIO, LogicalLine};
use crate::Editor;

use crate::excmd_exec::{EvalHost, ExRuntime, Flow, LuaExec, exec_error_flow, parse_program, push_text_message, replace_scope_pair, run_program, typval_to_text, VimRegex};

/// Routes one `assert_*` builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "assert_fails" => {
            call_assert_fails_builtin(host.runtime, host.editor, scope, host.lua, args)
        }
        _ => call_assert_builtin(host.runtime, host.editor, name, args, scope),
    }
}

fn call_assert_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    name: &str,
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    let spec = builtin_spec(name).expect("assert builtin metadata");
    if args.len() < spec.min_args {
        return Err(EvalError::new("E119", 0, format!("Not enough arguments for function: {name}")));
    }
    if spec.max_args.is_some_and(|maximum| args.len() > maximum) {
        return Err(EvalError::new("E118", 0, format!("Too many arguments for function: {name}")));
    }

    let failure = match name {
        "assert_equal" if args[0] != args[1] => Some(format!(
            "Expected {} but got {}",
            assertion_value(&args[0]),
            assertion_value(&args[1])
        )),
        "assert_notequal" if args[0] == args[1] => {
            Some(format!("Expected not equal to {}", assertion_value(&args[0])))
        }
        "assert_true" if !assertion_boolean(&args[0], true) => {
            Some(format!("Expected True but got {}", assertion_value(&args[0])))
        }
        "assert_false" if !assertion_boolean(&args[0], false) => {
            Some(format!("Expected False but got {}", assertion_value(&args[0])))
        }
        "assert_match" | "assert_notmatch" => {
            let pattern_text = typval_to_text(&args[0]);
            let actual_text = typval_to_text(&args[1]);
            let pattern = OxStr::from(pattern_text.as_str());
            let actual = OxStr::from(actual_text.as_str());
            let matched = VimRegex.is_match(&actual, &pattern, false)?;
            let fails = matched != (name == "assert_match");
            fails.then(|| {
                format!(
                    "Pattern {} {} match {}",
                    assertion_value(&args[0]),
                    if name == "assert_match" { "does not" } else { "does" },
                    assertion_value(&args[1])
                )
            })
        }
        "assert_inrange" => {
            let lower = assertion_number(&args[0])?;
            let upper = assertion_number(&args[1])?;
            let actual = assertion_number(&args[2])?;
            (actual < lower || actual > upper).then(|| {
                format!("Expected range {lower} - {upper}, but got {actual}")
            })
        }
        "assert_exception" => {
            let expected = typval_to_text(&args[0]);
            let actual = scope
                .get_scoped(ScopeKind::Vim, b"exception", 0)
                .ok()
                .map(typval_to_text)
                .unwrap_or_default();
            (!actual.contains(&expected)).then(|| format!("Expected {expected} but got {actual}"))
        }
        "assert_equalfile" => {
            let first = PathBuf::from(typval_to_text(&args[0]));
            let second = PathBuf::from(typval_to_text(&args[1]));
            let differs = match (
                runtime.scripts.io().read_to_string(&first),
                runtime.scripts.io().read_to_string(&second),
            ) {
                (Ok(first), Ok(second)) => first != second,
                _ => true,
            };
            differs.then(|| {
                format!("Files {} and {} differ", first.display(), second.display())
            })
        }
        "assert_report" => Some(typval_to_text(&args[0])),
        _ => None,
    };

    let Some(mut message) = failure else { return Ok(Typval::Number(0)) };
    let message_index = match name {
        "assert_equal" | "assert_notequal" | "assert_match" | "assert_notmatch" => 2,
        "assert_true" | "assert_false" | "assert_exception" => 1,
        "assert_inrange" => 3,
        _ => usize::MAX,
    };
    if let Some(prefix) = args.get(message_index).map(typval_to_text).filter(|text| !text.is_empty()) {
        message = format!("{prefix}: {message}");
    }
    let location = runtime.throwpoint();
    if location != "command line" {
        message = format!("{location}: {message}");
    }
    append_assertion_failure(scope, &message);
    push_text_message(editor, message, true, true);
    Ok(Typval::Number(1))
}

fn call_assert_fails_builtin<F: FileIO>(
    runtime: &mut ExRuntime<F>,
    editor: &mut Editor,
    scope: &mut Scope,
    lua: Option<&Rc<RefCell<dyn LuaExec>>>,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    let spec = builtin_spec("assert_fails").expect("assert_fails metadata");
    if args.len() < spec.min_args {
        return Err(EvalError::new("E119", 0, "Not enough arguments for function: assert_fails"));
    }
    if spec.max_args.is_some_and(|maximum| args.len() > maximum) {
        return Err(EvalError::new("E118", 0, "Too many arguments for function: assert_fails"));
    }
    let command = match &args[0] {
        Typval::String(value) => value.to_string_lossy().into_owned(),
        _ => return Err(EvalError::new("E1174", 0, "String required for argument 1")),
    };
    // The {error} argument is optional (`assert_fails({cmd} [, {error} [, ...`):
    // upstream only compares against it when `argvars[1]` is not UNKNOWN.
    let expected = match args.get(1) {
        None => None,
        Some(Typval::String(value)) => Some(vec![value.to_string_lossy().into_owned()]),
        Some(Typval::List(values)) => Some(
            values
                .try_borrow()
                .map_err(|_| EvalError::new("E742", 0, "Cannot change value during recursive container access"))?
                .items
                .iter()
                .map(typval_to_text)
                .collect(),
        ),
        Some(_) => return Err(EvalError::new("E1174", 0, "String required for argument 2")),
    };
    let logical = vec![LogicalLine { text: command, first_line: runtime.scripts.current_line() }];
    let flow = match parse_program(&runtime.user_commands, &logical) {
        Ok(program) => run_program(runtime, editor, scope, lua, &program, 0, program.len()),
        Err(error) => exec_error_flow(runtime, error),
    };
    let actual = match flow {
        Flow::Exception(exception) => exception.message(),
        Flow::NotImplemented(name) => format!("E117: not implemented: {name}"),
        Flow::Normal => String::new(),
        other => format!("{other:?}"),
    };
    // f_assert_fails reports three outcomes: a command that ran cleanly always
    // fails the assertion, a failure with no {error} always satisfies it, and a
    // failure with {error} must contain one of the expected strings.
    let failure = match (&expected, actual.is_empty()) {
        (_, true) => Some(format!("command did not fail: {}", typval_to_text(&args[0]))),
        (None, false) => None,
        (Some(expected), false) => (!expected.iter().any(|candidate| actual.contains(candidate)))
            .then(|| format!("Expected {} but got {actual}", expected.join(", "))),
    };
    let Some(mut message) = failure else { return Ok(Typval::Number(0)) };
    if let Some(prefix) = args.get(2).map(typval_to_text).filter(|text| !text.is_empty()) {
        message = format!("{prefix}: {message}");
    }
    let location = runtime.throwpoint();
    if location != "command line" {
        message = format!("{location}: {message}");
    }
    append_assertion_failure(scope, &message);
    push_text_message(editor, message, true, true);
    Ok(Typval::Number(1))
}

fn assertion_boolean(value: &Typval, expected: bool) -> bool {
    match value {
        Typval::Number(number) => (*number != 0) == expected,
        Typval::Bool(boolean) => *boolean == expected,
        _ => false,
    }
}

fn assertion_number(value: &Typval) -> ox_eval::Result<f64> {
    match value {
        Typval::Number(number) => Ok(*number as f64),
        Typval::Float(number) => Ok(*number),
        _ => Err(EvalError::new("E1219", 0, "Float or Number required")),
    }
}

fn assertion_value(value: &Typval) -> String {
    match value {
        Typval::String(text) => format!("'{}'", text.to_string_lossy().replace("'", "''")),
        _ => typval_to_text(value),
    }
}

fn append_assertion_failure(scope: &mut Scope, message: &str) {
    if let Some(Typval::List(errors)) = scope.vim.iter().find_map(|(name, value)| {
        (name.as_bytes() == b"errors").then_some(value)
    }) {
        errors.borrow_mut().items.push(Typval::String(OxStr::from(message.as_bytes())));
        return;
    }
    replace_scope_pair(
        &mut scope.vim,
        "errors",
        Typval::list(vec![Typval::String(OxStr::from(message.as_bytes()))]),
    );
}
