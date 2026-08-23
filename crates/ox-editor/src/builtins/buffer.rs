//! Buffer-state builtins: buffer-local variables, buffer identity, and the
//! line seams `getline`/`setline`/`append`/`line` reach through the current
//! buffer (upstream `eval/buffer.c`).

use ox_eval::scope::OptionScope as EvalOptionScope;
use ox_eval::call_buffer_builtin;
use ox_eval::BufferHost;
use ox_eval::EvalError;
use ox_eval::Scope;
use ox_types::{OxStr, Typval};
use crate::script::FileIO;
use ox_text::Position;

use crate::options::OptionScope;
use crate::Editor;

use crate::excmd_exec::{CurrentBuffer, EvalHost, object_to_typval, option_to_typval, replace_scope_pair, resolve_buffer_argument, typval_number, typval_to_object, typval_to_option, typval_to_text};

/// Routes one buffer-state builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        // `getline`/`setline` reach the current buffer through
        // `ox_eval::BufferHost`; the typval-only dispatcher has no buffer.
        "getline" | "setline" => {
            let mut seam = CurrentBuffer(host.editor);
            call_buffer_builtin(&mut seam, name, args)
        }
        "append" => call_append_builtin(host.editor, args),
        "bufexists" => call_bufexists_builtin(host.editor, &args),
        "bufname" | "bufnr" => call_buffer_identity_builtin(host.editor, name, &args),
        "getbufvar" => call_getbufvar_builtin(host.editor, scope, args),
        "last_buffer_nr" => call_last_buffer_nr_builtin(host.editor, &args),
        "line" => call_line_builtin(host.editor, args),
        "setbufvar" => call_setbufvar_builtin(host.editor, scope, args),
        _ => unreachable!("buffer builtin route and dispatcher disagree"),
    }
}

/// `bufexists()`: buffer number 0 never exists, every other resolvable
/// argument does (`f_bufexists` → `buflist_find_nr`).
fn call_bufexists_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        let (code, message) = if args.is_empty() {
            ("E119", "Not enough arguments for function: bufexists")
        } else {
            ("E118", "Too many arguments for function: bufexists")
        };
        return Err(EvalError::new(code, 0, message));
    }
    let exists = !matches!(args.first(), Some(Typval::Number(0)))
        && resolve_buffer_argument(editor, args.first()).is_some();
    Ok(Typval::Number(i64::from(exists)))
}

/// `bufname()` and `bufnr()`: `bufnr("$")` answers the highest buffer number
/// ever used, an unresolvable argument answers -1 (`f_bufnr`, `f_bufname`).
fn call_buffer_identity_builtin(
    editor: &Editor,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    let buffer = resolve_buffer_argument(editor, args.first());
    if name == "bufnr" {
        if args.first().is_some_and(|value| typval_to_text(value) == "$") {
            return Ok(Typval::Number(editor.last_buffer_nr()));
        }
        return Ok(Typval::Number(buffer.map_or(-1, i64::from)));
    }
    let name = buffer
        .and_then(|handle| editor.buffer(handle).ok())
        .map_or_else(|| OxStr::from(""), |state| state.name().clone());
    Ok(Typval::String(name))
}

/// `last_buffer_nr()`: the highest buffer number ever used.
fn call_last_buffer_nr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: last_buffer_nr",
        ));
    }
    Ok(Typval::Number(editor.last_buffer_nr()))
}

fn call_getbufvar_builtin(editor: &Editor, scope: &Scope, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    let fallback = args.get(2).cloned().unwrap_or_else(|| Typval::String(OxStr(Vec::new())));
    let Some(buffer) = resolve_buffer_argument(editor, args.first()) else { return Ok(fallback); };
    let name = args.get(1).map(typval_to_text).unwrap_or_default();
    let state = editor.buffer(buffer).map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
    if let Some(option) = name.strip_prefix('&') {
        let Some(metadata) = crate::option_metadata(option) else { return Ok(fallback); };
        let value = if metadata.scopes.contains(&OptionScope::Buffer) {
            editor.options().get_buffer(buffer, metadata.name).ok()
        } else {
            editor.options().get_global(metadata.name).ok()
        };
        return Ok(value.map_or(fallback, option_to_typval));
    }
    if name.is_empty() {
        let mut entries = if editor.current_buffer() == Some(buffer) {
            scope.buffer.clone()
        } else {
            state.variables().0.iter().map(|(key, value)| (key.clone(), object_to_typval(value))).collect::<Vec<_>>()
        };
        entries.retain(|(key, _)| key.as_bytes() != b"changedtick");
        entries.push((OxStr::from("changedtick"), Typval::Number(i64::try_from(state.changedtick()).unwrap_or(i64::MAX))));
        return Ok(Typval::dict(entries));
    }
    if editor.current_buffer() == Some(buffer) {
        return Ok(scope.buffer.iter().find(|(key, _)| key.as_bytes() == name.as_bytes()).map_or(fallback, |(_, value)| value.clone()));
    }
    Ok(state.variables().0.iter().find(|(key, _)| key.as_bytes() == name.as_bytes()).map_or(fallback, |(_, value)| object_to_typval(value)))
}

fn call_setbufvar_builtin(editor: &mut Editor, scope: &mut Scope, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.len() != 3 {
        return Err(EvalError::new(
            if args.len() < 3 { "E119" } else { "E118" },
            0,
            "Invalid arguments for setbufvar",
        ));
    }
    let buffer = resolve_buffer_argument(editor, args.first())
        .ok_or_else(|| EvalError::new("E86", 0, "Buffer does not exist"))?;
    let name = typval_to_text(&args[1]);
    let value = args[2].clone();

    if let Some(option) = name.strip_prefix('&') {
        let metadata = crate::option_metadata(option)
            .ok_or_else(|| EvalError::new("E518", 0, format!("Unknown option: {option}")))?;
        if !metadata.scopes.contains(&OptionScope::Buffer) {
            return Err(EvalError::new("E355", 0, format!("Unknown option: {option}")));
        }
        let converted = typval_to_option(&value, metadata.value_type)
            .map_err(|message| EvalError::new("E474", 0, message))?;
        editor
            .options_mut()
            .set_buffer(buffer, metadata.name, converted)
            .map_err(|error| EvalError::new("E474", 0, error.to_string()))?;
        if editor.current_buffer() == Some(buffer) {
            scope.set_option(EvalOptionScope::Local, metadata.name.as_bytes(), value);
        }
        return Ok(Typval::Number(0));
    }

    if editor.current_buffer() == Some(buffer) {
        replace_scope_pair(&mut scope.buffer, &name, value.clone());
    }
    let variables = editor
        .buffer_mut(buffer)
        .map_err(|error| EvalError::new("E86", 0, error.to_string()))?
        .variables_mut();
    variables.0.retain(|(key, _)| key.as_bytes() != name.as_bytes());
    variables.0.push((OxStr::from(name.as_str()), typval_to_object(&value)));
    Ok(Typval::Number(0))
}

fn call_append_builtin(editor: &mut Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new("E119", 0, "Not enough arguments for function: append"));
    }
    if args.len() > 2 {
        return Err(EvalError::new("E118", 0, "Too many arguments for function: append"));
    }
    let after = current_line_address(editor, &args[0])?;
    let to_line = |value: &Typval| {
        let mut line = typval_to_text(value).into_bytes();
        for byte in &mut line {
            if *byte == b'\n' {
                *byte = 0;
            }
        }
        line
    };
    let lines = match &args[1] {
        Typval::List(values) => values
            .borrow()
            .items
            .iter()
            .map(to_line)
            .collect::<Vec<_>>(),
        value => vec![to_line(value)],
    };
    let buffer = editor
        .current_buffer()
        .ok_or_else(|| EvalError::new("E749", 0, "Empty buffer"))?;
    let cursor = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .map_or(Position { lnum: after.saturating_add(1), col: 0 }, |window| window.cursor);
    editor
        .append_buffer_lines(buffer, after, &lines, cursor, 0)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    Ok(Typval::Number(0))
}

fn call_line_builtin(editor: &mut Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new("E119", 0, "Not enough arguments for function: line"));
    }
    if args.len() > 2 {
        return Err(EvalError::new("E118", 0, "Too many arguments for function: line"));
    }
    current_line_address(editor, &args[0]).map(|line| Typval::Number(line as i64))
}

fn current_line_address(editor: &mut Editor, value: &Typval) -> ox_eval::Result<usize> {
    let seam = CurrentBuffer(editor);
    let line = match value {
        Typval::String(address) if address.as_bytes() == b"$" => seam.line_count()? as i64,
        Typval::String(address) => seam
            .address_line(&address.to_string_lossy())?
            .unwrap_or(0),
        _ => typval_number(value).unwrap_or(0),
    };
    Ok(usize::try_from(line.max(0)).unwrap_or(usize::MAX))
}
