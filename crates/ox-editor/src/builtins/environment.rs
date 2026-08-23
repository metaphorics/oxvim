//! Ambient-environment builtins: working directory, system clock, highlight
//! table, and event-loop state.

use ox_eval::EvalError;
use ox_types::{OxStr, Typval};
use crate::script::FileIO;
use crate::Editor;

use crate::excmd_exec::{EvalHost, ExRuntime, change_directory, typval_number, typval_to_text};
use super::{input_string_arg};

/// Routes one ambient-environment builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    match name {
        "chdir" => call_chdir_builtin(host.runtime, host.editor, args),
        // f_eventhandler: nothing in this host runs inside an event handler.
        "eventhandler" => Ok(Typval::Number(0)),
        "highlight_exists" | "hlexists" => call_hlexists_builtin(host.editor, args),
        "strftime" => call_strftime_builtin(args),
        _ => unreachable!("environment builtin route and dispatcher disagree"),
    }
}

fn call_chdir_builtin<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::new(if args.is_empty() { "E119" } else { "E118" }, 0, "Invalid arguments for chdir"));
    }
    let Typval::String(path) = &args[0] else { return Ok(Typval::String(OxStr::from(""))); };
    let local = match args.get(1) {
        None => runtime.local_dir.is_some(),
        Some(Typval::String(scope)) => match scope.as_bytes() {
            b"global" => false,
            b"window" | b"tabpage" | b"buffer" => true,
            _ => return Err(EvalError::new("E475", 0, format!("Invalid argument: scope {}", scope.to_string_lossy()))),
        },
        Some(other) => { input_string_arg(other)?; unreachable!("non-string conversion always errors above") }
    };
    let previous = change_directory(runtime, editor, &path.to_string_lossy(), local)?;
    Ok(Typval::String(OxStr::from(previous.to_string_lossy().as_ref())))
}

fn call_strftime_builtin(args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new("E119", 0, "Not enough arguments for function: strftime"));
    }
    if args.len() > 2 {
        return Err(EvalError::new("E118", 0, "Too many arguments for function: strftime"));
    }
    if typval_to_text(&args[0]) != "%H:%M:%S" {
        return Err(EvalError::not_implemented(OxStr::from("strftime format")));
    }
    let timestamp = args
        .get(1)
        .and_then(typval_number)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        });
    let seconds = timestamp.rem_euclid(86_400);
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    Ok(Typval::String(OxStr::from(format!("{hours:02}:{minutes:02}:{seconds:02}").as_str())))
}

fn call_hlexists_builtin(editor: &Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.len() != 1 { return Err(EvalError::new(if args.is_empty() { "E119" } else { "E118" }, 0, "hlexists() requires one argument")); }
    let name = input_string_arg(&args[0])?;
    let name = name.to_string_lossy();
    Ok(Typval::Number(i64::from(editor.highlights().keys().any(|candidate| candidate.eq_ignore_ascii_case(&name)))))
}
