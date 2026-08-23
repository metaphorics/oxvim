//! Cursor-position builtins: window cursor reads and writes, and the screen
//! column derived from a buffer line (upstream `eval/vars.c`, `plines.c`).

use ox_eval::EvalError;
use ox_text::Position;
use ox_types::{Typval, WinHandle};
use crate::script::FileIO;
use crate::options::OptionValue;
use crate::Editor;

use crate::excmd_exec::{EvalHost, buffer_lines, typval_number};
use super::{input_string_arg};

/// Routes one cursor-position builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    match name {
        "getcurpos" => call_getcurpos_builtin(host.editor, args),
        "setpos" => call_setpos_builtin(host.editor, args),
        "virtcol" => call_virtcol_builtin(host.editor, args),
        _ => unreachable!("position builtin route and dispatcher disagree"),
    }
}

fn call_getcurpos_builtin(editor: &Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.len() > 1 { return Err(EvalError::new("E118", 0, "Too many arguments for function: getcurpos")); }
    let Some(window) = resolve_position_window(editor, args.first()) else {
        return Ok(Typval::list(vec![Typval::Number(0); 5]));
    };
    let position = editor.window(window).map_err(|error| EvalError::new("E957", 0, error.to_string()))?.cursor;
    let column = i64::try_from(position.col.saturating_add(1)).unwrap_or(i64::MAX);
    Ok(Typval::list(vec![Typval::Number(0), Typval::Number(i64::try_from(position.lnum).unwrap_or(i64::MAX)), Typval::Number(column), Typval::Number(0), Typval::Number(column)]))
}

fn call_setpos_builtin(editor: &mut Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.len() != 2 { return Err(EvalError::new(if args.len() < 2 { "E119" } else { "E118" }, 0, "setpos() requires two arguments")); }
    if input_string_arg(&args[0])?.as_bytes() != b"." { return Ok(Typval::Number(-1)); }
    let Typval::List(reference) = &args[1] else { return Ok(Typval::Number(-1)); };
    let values = reference.try_borrow().map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    if values.items.len() < 4 { return Ok(Typval::Number(-1)); }
    let lnum = values.items.get(1).and_then(typval_number).unwrap_or(0);
    let col = values.items.get(2).and_then(typval_number).unwrap_or(0);
    let Some(window) = editor.current_window() else { return Ok(Typval::Number(-1)); };
    if lnum <= 0 || col <= 0 { return Ok(Typval::Number(-1)); }
    editor.set_window_cursor(window, Position { lnum: usize::try_from(lnum).unwrap_or(usize::MAX), col: usize::try_from(col - 1).unwrap_or(usize::MAX) }).map_err(|error| EvalError::new("E474", 0, error.to_string()))?;
    Ok(Typval::Number(0))
}

fn call_virtcol_builtin(editor: &Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 3 { return Err(EvalError::new(if args.is_empty() { "E119" } else { "E118" }, 0, "Invalid arguments for virtcol")); }
    let list_result = args.get(1).is_some_and(Typval::is_truthy);
    let window = resolve_position_window(editor, args.get(2));
    let zero = || if list_result { Typval::list(vec![Typval::Number(0), Typval::Number(0)]) } else { Typval::Number(0) };
    let Some(window) = window else { return Ok(zero()); };
    let state = editor.window(window).map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let position = match &args[0] {
        Typval::String(value) if value.as_bytes() == b"." => state.cursor,
        Typval::List(reference) => {
            let values = reference.try_borrow().map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
            let lnum = values.items.first().and_then(typval_number).unwrap_or(0);
            let col = values.items.get(1).and_then(typval_number).unwrap_or(0);
            if lnum <= 0 || col <= 0 { return Ok(zero()); }
            Position { lnum: lnum as usize, col: col.saturating_sub(1) as usize }
        }
        Typval::String(value) if value.as_bytes().is_empty() => return Ok(zero()),
        _ => return Ok(zero()),
    };
    let lines = buffer_lines(editor, state.buffer).map_err(|error| EvalError::new("E16", 0, error))?;
    let Some(line) = lines.get(position.lnum.saturating_sub(1)) else { return Ok(zero()); };
    let tabstop = match editor.options().get_global("tabstop") { Ok(OptionValue::Number(value)) => (*value).max(1) as usize, _ => 8 };
    let mut start = 0usize;
    let mut end = 0usize;
    for (index, byte) in line.iter().copied().enumerate() {
        start = end.saturating_add(1);
        end = if byte == b'\t' { ((end / tabstop) + 1) * tabstop } else { end.saturating_add(1) };
        if index >= position.col { break; }
    }
    let showbreak_width = match editor.options().get_window(window, "showbreak") {
        Ok(OptionValue::String(value)) => value.chars().count(),
        _ => 0,
    };
    if showbreak_width > 0 {
        let width = editor.window_geometry(window).map(|geometry| geometry.width).unwrap_or(0);
        let continuation = width.saturating_sub(showbreak_width).max(1);
        let wrapped_column = |column: usize| {
            if column <= width {
                column
            } else {
                let wraps = 1 + (column - width - 1) / continuation;
                column.saturating_add(wraps.saturating_mul(showbreak_width))
            }
        };
        start = wrapped_column(start);
        end = wrapped_column(end);
    }
    let start = Typval::Number(i64::try_from(start).unwrap_or(i64::MAX));
    let end = Typval::Number(i64::try_from(end).unwrap_or(i64::MAX));
    Ok(if list_result { Typval::list(vec![start, end]) } else { end })
}

fn resolve_position_window(editor: &Editor, value: Option<&Typval>) -> Option<WinHandle> {
    match value.and_then(typval_number) {
        None | Some(0) => editor.current_window(),
        Some(number) => WinHandle::try_from(number).ok().filter(|window| editor.window(*window).is_ok()),
    }
}
