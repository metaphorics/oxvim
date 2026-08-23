//! Window and screen-cell builtins: window geometry, window identity, and the
//! rendered cell queries (upstream `eval/window.c`, `screen.c`).

use ox_eval::EvalError;
use ox_types::{OxStr, Typval};
use crate::script::FileIO;
use crate::Editor;

use crate::excmd_exec::{EvalHost, typval_number, typval_to_text};

/// Routes one window or screen-cell builtin.
pub(crate) fn call<F: FileIO>(
    host: &EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    match name {
        "screenattr" | "screenchar" | "screenchars" | "screenstring" => {
            call_screen_builtin(host.editor, name, args)
        }
        "tabpagenr" => call_tabpagenr_builtin(host.editor, &args),
        "win_getid" | "winheight" | "winwidth" => call_window_builtin(host.editor, name, args),
        "winnr" => call_winnr_builtin(host.editor, &args),
        _ => unreachable!("window builtin route and dispatcher disagree"),
    }
}

/// `winnr()`: the current window's position in the tabpage, or the window
/// count for `$` (`f_winnr` → `get_winnr`).
fn call_winnr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: winnr",
        ));
    }
    let windows = editor.windows();
    let number = if args.first().is_some_and(|value| typval_to_text(value) == "$") {
        windows.len()
    } else {
        editor
            .current_window()
            .and_then(|current| windows.iter().position(|window| *window == current))
            .map_or(0, |index| index + 1)
    };
    Ok(Typval::Number(i64::try_from(number).unwrap_or(i64::MAX)))
}

/// `tabpagenr()`: the current tabpage's position, or the tabpage count for
/// `$` (`f_tabpagenr`).
fn call_tabpagenr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: tabpagenr",
        ));
    }
    let tabs = editor.tabpages();
    let number = if args.first().is_some_and(|value| typval_to_text(value) == "$") {
        tabs.len()
    } else {
        editor
            .current_tabpage()
            .and_then(|current| tabs.iter().position(|tab| *tab == current))
            .map_or(0, |index| index + 1)
    };
    Ok(Typval::Number(i64::try_from(number).unwrap_or(i64::MAX)))
}

fn call_window_builtin(editor: &Editor, name: &str, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if name == "win_getid" {
        let window_number = args.first().and_then(typval_number).unwrap_or(0);
        let tab_number = args.get(1).and_then(typval_number).unwrap_or(0);
        let tab = if tab_number <= 0 {
            editor.current_tabpage()
        } else {
            usize::try_from(tab_number - 1).ok().and_then(|index| editor.tabpages().get(index).copied())
        };
        let window = tab.and_then(|tab| {
            let windows = editor.tabpage_windows(tab).ok()?;
            if window_number <= 0 {
                editor.current_window().filter(|window| windows.contains(window))
            } else {
                usize::try_from(window_number - 1).ok().and_then(|index| windows.get(index).copied())
            }
        });
        return Ok(Typval::Number(window.map_or(0, i64::from)));
    }

    let number = args.first().and_then(typval_number).unwrap_or(-1);
    let Some(tab) = editor.current_tabpage() else { return Ok(Typval::Number(-1)); };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let window = if number == 0 {
        editor.current_window()
    } else {
        usize::try_from(number - 1).ok().and_then(|index| windows.get(index).copied())
    };
    let value = window
        .and_then(|window| editor.window_geometry(window).ok())
        .map_or(-1, |geometry| i64::try_from(if name == "winwidth" { geometry.width } else { geometry.height }).unwrap_or(i64::MAX));
    Ok(Typval::Number(value))
}

fn call_screen_builtin(editor: &Editor, name: &str, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    let row = args.first().and_then(typval_number).unwrap_or(0);
    let column = args.get(1).and_then(typval_number).unwrap_or(0);
    let cell = screen_cell(editor, row, column);
    Ok(match name {
        "screenattr" => Typval::Number(if cell.is_some() { 0 } else { -1 }),
        "screenchar" => Typval::Number(cell.as_ref().and_then(|text| text.chars().next()).map_or(-1, |character| i64::from(character as u32))),
        "screenchars" => Typval::list(cell.as_ref().and_then(|text| text.chars().next()).map(|character| vec![Typval::Number(i64::from(character as u32))]).unwrap_or_default()),
        "screenstring" => Typval::String(OxStr::from(cell.as_deref().unwrap_or(""))),
        _ => unreachable!(),
    })
}

fn screen_cell(editor: &Editor, row: i64, column: i64) -> Option<String> {
    let row = usize::try_from(row.checked_sub(1)?).ok()?;
    let column = usize::try_from(column.checked_sub(1)?).ok()?;
    for window in editor.windows() {
        let geometry = editor.window_geometry(window).ok()?;
        if row < geometry.row || row >= geometry.row + geometry.height || column < geometry.col || column >= geometry.col + geometry.width {
            continue;
        }
        let state = editor.window(window).ok()?;
        let line = state.topline + row - geometry.row;
        let bytes = editor.buffer(state.buffer).ok()?.text().ok()?.line(line).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let cell = text.chars().nth(column - geometry.col).map(|character| character.to_string()).unwrap_or_else(|| " ".to_owned());
        return Some(cell);
    }
    None
}
