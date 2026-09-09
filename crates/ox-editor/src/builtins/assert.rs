//! `assert_*` builtins: they evaluate a claim, append the failure text to
//! `v:errors`, and echo it (upstream `testing.c`).

use crate::editor::Editor;
use crate::excmd_exec::ExEditorAccess;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::script::{FileIO, LogicalLine};
use ox_eval::EvalError;
use ox_eval::RegexEngine;
use ox_eval::Scope;
use ox_eval::ScopeKind;
use ox_eval::builtin_spec;
use ox_types::{OxStr, Typval};

use crate::excmd_exec::{
    EvalHost, ExRuntime, Flow, LuaExec, VimRegex, parse_program, push_text_message, run_program,
    typval_to_text,
};

/// Routes one `assert_*` builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        "assert_fails" => {
            call_assert_fails_builtin(host.runtime, host.access, scope, host.lua, args)
        }
        "assert_beeps" | "assert_nobeep" => {
            call_assert_beeps_builtin(host.runtime, host.access, scope, host.lua, name, args)
        }
        _ => call_assert_builtin(host.runtime, host.access, name, args, scope),
    }
}

fn call_assert_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    check_assert_arity(name, args.len())?;

    let failure = assertion_failure(runtime, name, args, scope)?;

    let Some(mut message) = failure else {
        return Ok(Typval::Number(0));
    };
    let message_index = match name {
        "assert_equal" | "assert_notequal" | "assert_match" | "assert_notmatch"
        | "assert_equalfile" => 2,
        "assert_true" | "assert_false" | "assert_exception" => 1,
        "assert_inrange" => 3,
        _ => usize::MAX,
    };
    if let Some(prefix) = args
        .get(message_index)
        .map(typval_to_text)
        .filter(|text| !text.is_empty())
    {
        message = format!("{prefix}: {message}");
    }
    let location = runtime.throwpoint();
    if location != "command line" {
        message = format!("{location}: {message}");
    }
    append_assertion_failure(scope, &message);
    access.with_ex_editor(|editor| push_text_message(editor, message, true, true));
    Ok(Typval::Number(1))
}

fn assertion_failure<F: FileIO>(
    runtime: &ExRuntime<F>,
    name: &str,
    args: &[Typval],
    scope: &Scope,
) -> ox_eval::Result<Option<String>> {
    match name {
        "assert_equal" if args[0] != args[1] => Ok(Some(assert_equal_message(&args[0], &args[1]))),
        "assert_notequal" if args[0] == args[1] => Ok(Some(format!(
            "Expected not equal to {}",
            assertion_value(&args[0])
        ))),
        "assert_true" if !assertion_boolean(&args[0], true) => Ok(Some(format!(
            "Expected True but got {}",
            assertion_value(&args[0])
        ))),
        "assert_false" if !assertion_boolean(&args[0], false) => Ok(Some(format!(
            "Expected False but got {}",
            assertion_value(&args[0])
        ))),
        "assert_match" | "assert_notmatch" => {
            let pattern_text = typval_to_text(&args[0]);
            let actual_text = typval_to_text(&args[1]);
            let pattern = OxStr::from(pattern_text.as_str());
            let actual = OxStr::from(actual_text.as_str());
            let matched = VimRegex.is_match(&actual, &pattern, false)?;
            Ok((matched != (name == "assert_match")).then(|| {
                format!(
                    "Pattern {} {} match {}",
                    assertion_value(&args[0]),
                    if name == "assert_match" {
                        "does not"
                    } else {
                        "does"
                    },
                    assertion_value(&args[1])
                )
            }))
        }
        "assert_inrange" => {
            let lower = assertion_number(&args[0])?;
            let upper = assertion_number(&args[1])?;
            let actual = assertion_number(&args[2])?;
            let float_output = args[..3]
                .iter()
                .any(|value| matches!(value, Typval::Float(_)));
            Ok((actual < lower || actual > upper).then(|| {
                format!(
                    "Expected range {} - {}, but got {}",
                    assertion_range_number(lower, float_output),
                    assertion_range_number(upper, float_output),
                    assertion_range_number(actual, float_output)
                )
            }))
        }
        "assert_exception" => {
            let expected = typval_to_text(&args[0]);
            let actual = scope
                .get_scoped(ScopeKind::Vim, b"exception", 0)
                .ok()
                .map(typval_to_text)
                .unwrap_or_default();
            Ok((!actual.contains(&expected))
                .then(|| format!("Expected {expected} but got {actual}")))
        }
        "assert_equalfile" => {
            let first = PathBuf::from(typval_to_text(&args[0]));
            let second = PathBuf::from(typval_to_text(&args[1]));
            Ok(equalfile_failure(runtime, &first, &second))
        }
        "assert_report" => Ok(Some(typval_to_text(&args[0]))),
        _ => Ok(None),
    }
}

fn call_assert_fails_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<dyn LuaExec>>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    check_assert_arity("assert_fails", args.len())?;
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
                .map_err(|_| {
                    EvalError::new(
                        "E742",
                        0,
                        "Cannot change value during recursive container access",
                    )
                })?
                .items
                .iter()
                .map(typval_to_text)
                .collect(),
        ),
        Some(_) => return Err(EvalError::new("E1174", 0, "String required for argument 2")),
    };
    let logical = vec![LogicalLine {
        text: command,
        first_line: runtime.scripts.current_line(),
    }];
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    let flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
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
        (_, true) => Some(format!(
            "command did not fail: {}",
            typval_to_text(&args[0])
        )),
        (None, false) => None,
        (Some(expected), false) => (!expected.iter().any(|candidate| actual.contains(candidate)))
            .then(|| format!("Expected {} but got {actual}", expected.join(", "))),
    };
    let Some(mut message) = failure else {
        return Ok(Typval::Number(0));
    };
    if let Some(prefix) = args
        .get(2)
        .map(typval_to_text)
        .filter(|text| !text.is_empty())
    {
        message = format!("{prefix}: {message}");
    }
    let location = runtime.throwpoint();
    if location != "command line" {
        message = format!("{location}: {message}");
    }
    append_assertion_failure(scope, &message);
    access.with_ex_editor(|editor| push_text_message(editor, message, true, true));
    Ok(Typval::Number(1))
}

fn call_assert_beeps_builtin<F: FileIO, E: ExEditorAccess>(
    runtime: &mut ExRuntime<F>,
    access: &E,
    scope: &mut Scope,
    lua: Option<&Rc<dyn LuaExec>>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    check_assert_arity(name, args.len())?;
    let command = match &args[0] {
        Typval::String(value) => value.to_string_lossy().into_owned(),
        _ => return Err(EvalError::new("E1174", 0, "String required for argument 1")),
    };
    let _ = access.with_ex_editor(Editor::take_beeped);
    let logical = vec![LogicalLine {
        text: command.clone(),
        first_line: runtime.scripts.current_line(),
    }];
    let program = parse_program(
        &runtime.user_commands,
        access.with_ex_editor(|editor| editor.current_buffer()),
        &logical,
    );
    let _flow = run_program(runtime, access, scope, lua, &program, 0, program.len());
    let beeped = access.with_ex_editor(Editor::take_beeped);
    let want_beep = name == "assert_beeps";
    if beeped == want_beep {
        return Ok(Typval::Number(0));
    }
    let mut message = if want_beep {
        format!("command did not beep: {command}")
    } else {
        format!("command did beep: {command}")
    };
    let location = runtime.throwpoint();
    if location != "command line" {
        message = format!("{location}: {message}");
    }
    append_assertion_failure(scope, &message);
    access.with_ex_editor(|editor| push_text_message(editor, message, true, true));
    Ok(Typval::Number(1))
}

fn equalfile_failure<F: FileIO>(
    runtime: &ExRuntime<F>,
    first_path: &Path,
    second_path: &Path,
) -> Option<String> {
    let Ok(first) = runtime.scripts.io().read_bytes(first_path) else {
        return Some(format!("E485: Can't read file {}", first_path.display()));
    };
    let Ok(second) = runtime.scripts.io().read_bytes(second_path) else {
        return Some(format!("E485: Can't read file {}", second_path.display()));
    };
    let shared = first.len().min(second.len());
    let Some(offset) = (0..shared).find(|&offset| first[offset] != second[offset]) else {
        return match first.len().cmp(&second.len()) {
            std::cmp::Ordering::Less => Some("first file is shorter".to_owned()),
            std::cmp::Ordering::Greater => Some("second file is shorter".to_owned()),
            std::cmp::Ordering::Equal => None,
        };
    };
    let line = first[..offset].split(|&byte| byte == b'\n').count();
    let line_start = first[..offset]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |position| position + 1);
    let context_start = line_start.max(offset.saturating_sub(100));
    let first_context = String::from_utf8_lossy(&first[context_start..=offset]);
    let second_context = String::from_utf8_lossy(&second[context_start..=offset]);
    let mut message = format!("difference at byte {offset}, line {line} after \"{first_context}");
    if first_context != second_context {
        let _ = write!(message, "\" vs \"{second_context}");
    }
    message.push('"');
    Some(message)
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
        Typval::Number(number) => {
            const RADIX: i64 = 1_i64 << 32;
            let high = i32::try_from(number.div_euclid(RADIX)).map_err(|_| {
                EvalError::not_implemented(OxStr::from("Number-to-Float conversion invariant"))
            })?;
            let low = u32::try_from(number.rem_euclid(RADIX)).map_err(|_| {
                EvalError::not_implemented(OxStr::from("Number-to-Float conversion invariant"))
            })?;
            Ok(f64::from(high).mul_add(4_294_967_296.0, f64::from(low)))
        }
        Typval::Float(number) => Ok(*number),
        _ => Err(EvalError::new("E1219", 0, "Float or Number required")),
    }
}
fn assertion_range_number(value: f64, float_output: bool) -> String {
    let mut text = value.to_string();
    if float_output && !text.contains(['.', 'e', 'E']) {
        text.push_str(".0");
    }
    text
}

fn check_assert_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = builtin_spec(name).ok_or_else(|| EvalError::not_implemented(OxStr::from(name)))?;
    if count < spec.min_args {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if spec.max_args.is_some_and(|maximum| count > maximum) {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    Ok(())
}

fn assert_equal_message(expected: &Typval, actual: &Typval) -> String {
    let (Typval::Dict(expected_ref), Typval::Dict(actual_ref)) = (expected, actual) else {
        return format!(
            "Expected {} but got {}",
            assertion_value(expected),
            assertion_value(actual)
        );
    };
    let (Ok(expected_data), Ok(actual_data)) = (expected_ref.try_borrow(), actual_ref.try_borrow())
    else {
        return format!(
            "Expected {} but got {}",
            assertion_value(expected),
            assertion_value(actual)
        );
    };
    let entries_equal = |left: &ox_types::DictEntry, right: &ox_types::DictEntry| {
        left.key == right.key && left.value == right.value
    };
    let equal = expected_data
        .entries
        .iter()
        .filter(|entry| {
            actual_data
                .entries
                .iter()
                .any(|other| entries_equal(entry, other))
        })
        .count();
    let mut expected_difference = expected_data
        .entries
        .iter()
        .filter(|entry| {
            !actual_data
                .entries
                .iter()
                .any(|other| entries_equal(entry, other))
        })
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect::<Vec<_>>();
    expected_difference.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut actual_difference = actual_data
        .entries
        .iter()
        .filter(|entry| {
            !expected_data
                .entries
                .iter()
                .any(|other| entries_equal(entry, other))
        })
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect::<Vec<_>>();
    actual_difference.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let expected_difference = Typval::dict(expected_difference);
    let actual_difference = Typval::dict(actual_difference);
    let mut message = format!(
        "Expected {} but got {}",
        assertion_value(&expected_difference),
        assertion_value(&actual_difference)
    );
    if equal != 0 {
        let noun = if equal == 1 { "item" } else { "items" };
        let _ = write!(message, " - {equal} equal {noun} omitted");
    }
    message
}

fn assertion_value(value: &Typval) -> String {
    let Typval::String(text) = value else {
        return typval_to_text(value);
    };
    let mut escaped = String::with_capacity(text.as_bytes().len() + 2);
    escaped.push('\'');
    let mut remaining = text.as_bytes();
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                append_assertion_text(&mut escaped, valid);
                break;
            }
            Err(error) => {
                let valid_bytes = error.valid_up_to();
                if valid_bytes != 0 {
                    if let Ok(valid) = std::str::from_utf8(&remaining[..valid_bytes]) {
                        append_assertion_text(&mut escaped, valid);
                    }
                    remaining = &remaining[valid_bytes..];
                }
                let invalid_bytes = error.error_len().unwrap_or(remaining.len());
                for byte in &remaining[..invalid_bytes] {
                    let _ = write!(escaped, "\\x{byte:02x}");
                }
                remaining = &remaining[invalid_bytes..];
            }
        }
    }
    escaped.push('\'');
    escaped
}

fn append_assertion_text(output: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '\u{08}' => output.push_str("\\b"),
            '\u{1b}' => output.push_str("\\e"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\t' => output.push_str("\\t"),
            '\r' => output.push_str("\\r"),
            '\\' => output.push_str("\\\\"),
            '\'' => output.push_str("''"),
            character if character.is_control() && u32::from(character) <= 0xff => {
                let _ = write!(output, "\\x{:02x}", u32::from(character));
            }
            character => output.push(character),
        }
    }
}

fn append_assertion_failure(scope: &mut Scope, message: &str) {
    if let Some(Typval::List(errors)) = scope
        .vim
        .iter()
        .find_map(|(name, value)| (name.as_bytes() == b"errors").then_some(value))
    {
        errors
            .borrow_mut()
            .items
            .push(Typval::String(OxStr::from(message.as_bytes())));
        // The push mutates the shared container, not the `v:` map, so the
        // dirty-mark that `Scope::replace_pair` would have set has to be
        // recorded here or the new entry never reaches the editor.
        scope.synced.mark_dirty(ScopeKind::Vim);
        return;
    }
    scope.replace_pair(
        ScopeKind::Vim,
        "errors",
        Typval::list(vec![Typval::String(OxStr::from(message.as_bytes()))]),
    );
}

#[cfg(test)]
mod tests {
    use super::assertion_value;
    use ox_types::{OxStr, Typval};

    #[test]
    fn assertion_strings_escape_invalid_utf8_bytes() {
        let value = Typval::String(OxStr(vec![0x80]));

        assert_eq!(assertion_value(&value), "'\\x80'");
    }
}
