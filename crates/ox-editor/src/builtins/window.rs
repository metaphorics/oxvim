//! Window and screen-cell builtins: window geometry, window identity, and the
//! rendered cell queries (upstream `eval/window.c`, `screen.c`).

use crate::Editor;
use crate::editor::LOWEST_WINDOW_ID;
use crate::excmd_exec::ExEditorAccess;
use crate::options::OptionValue;
use crate::script::FileIO;
use ox_eval::EvalError;
use ox_eval::scope::{ScopeKind, scope_var_entry};
use ox_types::{OxStr, Typval};

use crate::builtins::position::{cursor_vcol, number_value, tabstop as position_tabstop};
use crate::excmd_exec::{EvalHost, typval_number, typval_to_text};
use crate::layout::Frame;

fn one_based_index(number: i64) -> Option<usize> {
    usize::try_from(number).ok()?.checked_sub(1)
}

/// Routes one window or screen-cell builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    match name {
        "screenattr" | "screenchar" | "screenchars" | "screenstring" => Ok(host
            .access
            .with_ex_editor(|editor| call_screen_builtin(editor, name, args))),
        "screencol" => host
            .access
            .with_ex_editor(|editor| call_screencol(editor, args)),
        "screenrow" => host
            .access
            .with_ex_editor(|editor| call_screenrow(editor, args)),
        "tabpagenr" => host
            .access
            .with_ex_editor(|editor| call_tabpagenr_builtin(editor, args)),
        "tabpagewinnr" => host
            .access
            .with_ex_editor(|editor| call_tabpagewinnr_builtin(editor, args)),
        "win_getid" | "winheight" | "winwidth" => Ok(host
            .access
            .with_ex_editor(|editor| call_window_builtin(editor, name, args))),
        "win_gotoid" => host
            .access
            .with_ex_editor(|editor| call_win_gotoid_builtin(editor, args)),
        "winbufnr" => host
            .access
            .with_ex_editor(|editor| call_winbufnr_builtin(editor, args)),
        "winnr" => host
            .access
            .with_ex_editor(|editor| call_winnr_builtin(editor, args)),
        "winsaveview" => host
            .access
            .with_ex_editor(|editor| call_winsaveview(editor, args)),
        "winrestview" => host
            .access
            .with_ex_editor(|editor| call_winrestview(editor, args)),
        "winline" => host
            .access
            .with_ex_editor(|editor| call_winline(editor, args)),
        "wincol" => host
            .access
            .with_ex_editor(|editor| call_wincol(editor, args)),
        "getwinvar" => host
            .access
            .with_ex_editor(|editor| call_getwinvar(editor, args)),
        "setwinvar" => host
            .access
            .with_ex_editor(|editor| call_setwinvar(editor, args)),
        "getwininfo" => host
            .access
            .with_ex_editor(|editor| call_getwininfo(editor, args)),
        "winlayout" => Ok(host
            .access
            .with_ex_editor(|editor| call_winlayout(editor, args))),
        _ => unreachable!("window builtin route and dispatcher disagree"),
    }
}

/// `winnr()`: the current window's position in the tabpage, or the window
/// count for `$` (`f_winnr` → `get_winnr`).
///
/// Upstream counts within one tabpage: `get_winnr`
/// (`eval/window.c:278-292`) walks `tp`'s windows and uses `tp_lastwin` for
/// `$`. Counting every window in the editor only agreed with that while a
/// single tabpage was the only reachable state.
fn call_winnr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: winnr",
        ));
    }
    let windows = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_windows(tab).ok())
        .unwrap_or_default();
    let number = if args
        .first()
        .is_some_and(|value| typval_to_text(value) == "$")
    {
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
    let number = if args
        .first()
        .is_some_and(|value| typval_to_text(value) == "$")
    {
        tabs.len()
    } else {
        editor
            .current_tabpage()
            .and_then(|current| tabs.iter().position(|tab| *tab == current))
            .map_or(0, |index| index + 1)
    };
    Ok(Typval::Number(i64::try_from(number).unwrap_or(i64::MAX)))
}

/// `tabpagewinnr({tabnr} [, {arg}])`: a window number in one tabpage.
fn call_tabpagewinnr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: tabpagewinnr",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: tabpagewinnr",
        ));
    }
    let tab_number = typval_number(&args[0]).unwrap_or(0);
    let tabs = editor.tabpages();
    let Some(tab) = one_based_index(tab_number).and_then(|index| tabs.get(index).copied()) else {
        return Ok(Typval::Number(0));
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let Some(mut target) = editor
        .tabpage(tab)
        .ok()
        .map(crate::layout::TabpageState::current_window)
    else {
        return Ok(Typval::Number(0));
    };
    let Some(selector) = args.get(1) else {
        return Ok(window_number(&windows, target));
    };
    let selector = typval_to_text(selector);
    if selector == "$" {
        return Ok(Typval::Number(
            i64::try_from(windows.len()).unwrap_or(i64::MAX),
        ));
    }
    if selector == "#" {
        let previous = editor
            .tabpage(tab)
            .ok()
            .and_then(crate::layout::TabpageState::previous_window);
        return Ok(previous.map_or(Typval::Number(0), |window| window_number(&windows, window)));
    }

    let direction_start = selector
        .as_bytes()
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(selector.len());
    let (count, direction) = selector.split_at(direction_start);
    let Some(direction) = direction
        .strip_prefix(['h', 'j', 'k', 'l'])
        .filter(|rest| rest.is_empty())
        .and_then(|_| selector.chars().last())
    else {
        return Err(EvalError::new(
            "E15",
            0,
            format!("Invalid expression: \"{selector}\""),
        ));
    };
    let count = count.parse::<usize>().unwrap_or(0).max(1);
    for _ in 0..count {
        target = crate::excmd_exec::directional_window(editor, target, &windows, direction)
            .unwrap_or(target);
    }
    Ok(window_number(&windows, target))
}

fn window_number(windows: &[ox_types::WinHandle], target: ox_types::WinHandle) -> Typval {
    let number = windows
        .iter()
        .position(|window| *window == target)
        .map_or(0, |index| index + 1);
    Typval::Number(i64::try_from(number).unwrap_or(i64::MAX))
}
/// `winbufnr({nr})`: the buffer displayed by a window number or id.
fn call_winbufnr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: winbufnr",
        ));
    }
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: winbufnr",
        ));
    }

    let number = number_value(&args[0])?;
    let window = if number < 0 {
        None
    } else if number == 0 {
        editor.current_window()
    } else if number < LOWEST_WINDOW_ID {
        editor.current_tabpage().and_then(|tab| {
            let windows = editor.tabpage_windows(tab).ok()?;
            one_based_index(number).and_then(|index| windows.get(index).copied())
        })
    } else {
        editor.find_window_by_id(number)
    };
    let buffer = window
        .and_then(|window| editor.window(window).ok())
        .map_or(-1, |state| i64::from(state.buffer));
    Ok(Typval::Number(buffer))
}

/// `getwininfo([{winid}])` (`eval/window.c:f_getwininfo`): one dict of window
/// facts per window. Without an argument that is every window of every
/// tabpage; with an id it narrows to that one window, and an unknown id
/// yields an empty list like upstream `win_id2wp` returning NULL.
fn call_getwininfo(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: getwininfo",
        ));
    }
    // `win_id2wp` (`eval/window.c:118-137`): the argument is a window id, so
    // non-positive numbers and unknown ids produce an empty list.
    let target = match args.first() {
        None => None,
        Some(value) => {
            let id = number_value(value)?;
            let found = if id > 0 {
                editor.find_window_by_id(id)
            } else {
                None
            };
            if found.is_none() {
                return Ok(Typval::list(Vec::new()));
            }
            found
        }
    };
    let mut entries = Vec::new();
    for (tab_index, tab) in editor.tabpages().into_iter().enumerate() {
        let tabnr = Typval::Number(i64::try_from(tab_index + 1).unwrap_or(i64::MAX));
        let windows = editor.tabpage_windows(tab).unwrap_or_default();
        for &window in &windows {
            if target.is_some_and(|found| window != found) {
                continue;
            }
            entries.push(win_info(
                editor,
                window,
                window_number(&windows, window),
                tabnr.clone(),
            ));
            if target.is_some() {
                return Ok(Typval::list(entries));
            }
        }
    }
    Ok(Typval::list(entries))
}

/// `get_win_info` (`eval/window.c:347-377`), restricted to the fields this
/// port derives from real editor state. Upstream's screen-position fields
/// (`status_height`, `winrow`, `topline`, `botline`, `leftcol`, `winbar`,
/// `wincol`, `textoff`) and `variables` are left out rather than invented,
/// and `terminal`/`quickfix`/`loclist` are constant 0 because no such
/// window families exist here.
fn win_info(editor: &Editor, window: ox_types::WinHandle, winnr: Typval, tabnr: Typval) -> Typval {
    let buffer = editor
        .window(window)
        .map_or(0, |state| i64::from(state.buffer));
    let topline = editor.window(window).map_or(1, |state| state.topline);
    let (height, width, row, col) =
        editor
            .window_geometry(window)
            .map_or((-1, -1, 0, 0), |geometry| {
                (
                    i64::try_from(geometry.height).unwrap_or(i64::MAX),
                    i64::try_from(geometry.width).unwrap_or(i64::MAX),
                    geometry.row,
                    geometry.col,
                )
            });
    // `w_botline` needs the full scroll model; the last visible row is the
    // honest bound this port can state. No window carries a status line,
    // winbar, number column, or leftward scroll in the model, so those
    // report their zero-height/zero-offset values (`get_win_info`,
    // eval/window.c:347-377).
    let botline =
        (topline.saturating_add(usize::try_from(height.max(0)).unwrap_or(0))).saturating_sub(1);
    let variables = editor.window_variables(window).map_or_else(
        |_| Typval::dict(Vec::new()),
        |vars| {
            Typval::dict(
                vars.0
                    .iter()
                    .map(|(key, value)| (key.clone(), crate::excmd_exec::object_to_typval(value)))
                    .collect(),
            )
        },
    );
    Typval::dict(vec![
        (OxStr::from("tabnr"), tabnr),
        (OxStr::from("winnr"), winnr),
        (OxStr::from("winid"), Typval::Number(i64::from(window))),
        (OxStr::from("height"), Typval::Number(height)),
        (OxStr::from("status_height"), Typval::Number(0)),
        (
            OxStr::from("winrow"),
            Typval::Number(i64::try_from(row.saturating_add(1)).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("topline"),
            Typval::Number(i64::try_from(topline).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("botline"),
            Typval::Number(i64::try_from(botline).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("leftcol"), Typval::Number(0)),
        (OxStr::from("winbar"), Typval::Number(0)),
        (OxStr::from("width"), Typval::Number(width)),
        (OxStr::from("bufnr"), Typval::Number(buffer)),
        (
            OxStr::from("wincol"),
            Typval::Number(i64::try_from(col.saturating_add(1)).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("textoff"), Typval::Number(0)),
        (OxStr::from("terminal"), Typval::Number(0)),
        (OxStr::from("quickfix"), Typval::Number(0)),
        (OxStr::from("loclist"), Typval::Number(0)),
        (OxStr::from("variables"), variables),
    ])
}

/// `win_gotoid({expr})`: focus a live window by its global handle.
fn call_win_gotoid_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: win_gotoid",
        ));
    }
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: win_gotoid",
        ));
    }

    let id = number_value(&args[0])?;
    if id <= 0 {
        return Ok(Typval::Number(0));
    }
    let Some(window) = editor.find_window_by_id(id) else {
        return Ok(Typval::Number(0));
    };
    if editor.current_window() != Some(window) {
        editor
            .set_current_window(window)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    }
    Ok(Typval::Number(1))
}

fn call_window_builtin(editor: &Editor, name: &str, args: &[Typval]) -> Typval {
    if name == "win_getid" {
        let window_number = args.first().and_then(typval_number).unwrap_or(0);
        let tab_number = args.get(1).and_then(typval_number).unwrap_or(0);
        let tab = if tab_number <= 0 {
            editor.current_tabpage()
        } else {
            one_based_index(tab_number).and_then(|index| editor.tabpages().get(index).copied())
        };
        let window = tab.and_then(|tab| {
            let windows = editor.tabpage_windows(tab).ok()?;
            if window_number <= 0 {
                editor
                    .current_window()
                    .filter(|window| windows.contains(window))
            } else {
                one_based_index(window_number).and_then(|index| windows.get(index).copied())
            }
        });
        return Typval::Number(window.map_or(0, i64::from));
    }

    let number = args.first().and_then(typval_number).unwrap_or(-1);
    let Some(tab) = editor.current_tabpage() else {
        return Typval::Number(-1);
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let window = if number == 0 {
        editor.current_window()
    } else {
        one_based_index(number).and_then(|index| windows.get(index).copied())
    };
    let value = window
        .and_then(|window| editor.window_geometry(window).ok())
        .map_or(-1, |geometry| {
            i64::try_from(if name == "winwidth" {
                geometry.width
            } else {
                geometry.height
            })
            .unwrap_or(i64::MAX)
        });
    Typval::Number(value)
}

fn call_screen_builtin(editor: &Editor, name: &str, args: &[Typval]) -> Typval {
    let row = args.first().and_then(typval_number).unwrap_or(0);
    let column = args.get(1).and_then(typval_number).unwrap_or(0);
    let cell = screen_cell(editor, row, column);
    match name {
        "screenattr" => Typval::Number(if cell.is_some() { 0 } else { -1 }),
        "screenchar" => Typval::Number(
            cell.as_ref()
                .and_then(|text| text.chars().next())
                .map_or(-1, |character| i64::from(character as u32)),
        ),
        "screenchars" => Typval::list(
            cell.as_ref()
                .and_then(|text| text.chars().next())
                .map(|character| vec![Typval::Number(i64::from(character as u32))])
                .unwrap_or_default(),
        ),
        "screenstring" => Typval::String(OxStr::from(cell.as_deref().unwrap_or(""))),
        _ => unreachable!(),
    }
}

/// `screencol()` (`eval/funcs.c:f_screencol`): the cursor's one-based screen
/// column. Upstream returns `ui_current_col() + 1`, which is the window's
/// screen column plus the cursor's virtual column within the window.
fn call_screencol(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: screencol",
        ));
    }
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let geometry = editor
        .window_geometry(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let buffer = editor
        .buffer(state.buffer)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let text = buffer
        .text()
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let line = text
        .line(state.cursor.lnum)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let virtual_column = cursor_vcol(&line, state.cursor.col, position_tabstop(editor));
    let coladd = usize::try_from(state.coladd.max(0)).unwrap_or(usize::MAX);
    let column = geometry
        .col
        .saturating_add(virtual_column)
        .saturating_add(coladd)
        .saturating_add(1);
    Ok(Typval::Number(i64::try_from(column).unwrap_or(i64::MAX)))
}

/// `screenrow()` (`eval/funcs.c:f_screenrow`): the cursor's one-based screen
/// row. Upstream returns `ui_current_row() + 1`, which is the window's
/// screen row plus the cursor's row within the window.
fn call_screenrow(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: screenrow",
        ));
    }
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let geometry = editor
        .window_geometry(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let row_in_window = state.cursor.lnum.saturating_sub(state.topline);
    let row = geometry.row.saturating_add(row_in_window).saturating_add(1);
    Ok(Typval::Number(i64::try_from(row).unwrap_or(i64::MAX)))
}

/// Resolves one screen cell to the character the covering window shows.
///
/// Only the current tabpage is on screen, so only its windows can own a cell.
/// Windows in other tabpages have overlapping geometries and would otherwise
/// win this search and return a character from something invisible.
fn screen_cell(editor: &Editor, row: i64, column: i64) -> Option<String> {
    let row = usize::try_from(row.checked_sub(1)?).ok()?;
    let column = usize::try_from(column.checked_sub(1)?).ok()?;
    let windows = editor
        .current_tabpage()
        .and_then(|tab| editor.tabpage_windows(tab).ok())
        .unwrap_or_default();
    for window in windows {
        let geometry = editor.window_geometry(window).ok()?;
        if row < geometry.row
            || row >= geometry.row + geometry.height
            || column < geometry.col
            || column >= geometry.col + geometry.width
        {
            continue;
        }
        let state = editor.window(window).ok()?;
        let line = state.topline + row - geometry.row;
        let bytes = editor
            .buffer(state.buffer)
            .ok()?
            .text()
            .ok()?
            .line(line)
            .ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let cell = text
            .chars()
            .nth(column - geometry.col)
            .map_or_else(|| " ".to_owned(), |character| character.to_string());
        return Some(cell);
    }
    None
}

/// `winsaveview()` (`eval/window.c:f_winsaveview`): returns a dict with
/// `lnum`, `col`, `coladd`, `curswant`, `topline`, `topfill`, `leftcol`,
/// `skipcol` from the current window's state.
fn call_winsaveview(editor: &Editor, _args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let cursor_col = i64::try_from(state.cursor.col).unwrap_or(i64::MAX);
    let curswant = if state.set_curswant {
        // Recompute from cursor virtual column (move.c:update_curswant)
        let lines = crate::excmd_exec::buffer_lines(editor, state.buffer)
            .map_err(|error| EvalError::new("E16", 0, error))?;
        let line = lines
            .get(state.cursor.lnum.saturating_sub(1))
            .map_or(&[][..], Vec::as_slice);
        i64::try_from(cursor_vcol(
            line,
            state.cursor.col,
            position_tabstop(editor),
        ))
        .unwrap_or(i64::MAX)
    } else {
        state.curswant
    };
    Ok(Typval::dict(vec![
        (
            OxStr::from("lnum"),
            Typval::Number(i64::try_from(state.cursor.lnum).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("col"), Typval::Number(cursor_col)),
        (OxStr::from("coladd"), Typval::Number(state.coladd)),
        (OxStr::from("curswant"), Typval::Number(curswant)),
        (
            OxStr::from("topline"),
            Typval::Number(i64::try_from(state.topline).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("topfill"), Typval::Number(0)),
        (OxStr::from("leftcol"), Typval::Number(0)),
        (OxStr::from("skipcol"), Typval::Number(0)),
    ]))
}

/// `winrestview({dict})` (`eval/window.c:f_winrestview`): restores the
/// view from a dict. Only the restorable subset is applied.
fn call_winrestview(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let Typval::Dict(dict_ref) = &args[0] else {
        return Err(EvalError::new("E1297", 0, "Dictionary required"));
    };
    let dict = dict_ref
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let mut lnum = None;
    let mut col = None;
    let mut coladd = None;
    let mut curswant = None;
    let mut topline = None;
    for entry in &dict.entries {
        match entry.key.as_bytes() {
            b"lnum" => lnum = typval_number(&entry.value),
            b"col" => col = typval_number(&entry.value),
            b"coladd" => coladd = typval_number(&entry.value),
            b"curswant" => curswant = typval_number(&entry.value),
            b"topline" => topline = typval_number(&entry.value),
            _ => {}
        }
    }
    if let Some(topline) = topline {
        let topline = usize::try_from(topline.max(1)).unwrap_or(usize::MAX);
        editor
            .set_window_topline(window, topline)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    }
    if let Some(lnum) = lnum {
        let state = editor
            .window(window)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
        let col = match col {
            Some(col) => usize::try_from(col.max(0)).unwrap_or(usize::MAX),
            None => state.cursor.col,
        };
        let coladd = coladd.unwrap_or(state.coladd);
        let position = ox_text::Position {
            lnum: usize::try_from(lnum.max(1)).unwrap_or(usize::MAX),
            col,
        };
        editor
            .set_window_cursor(window, position)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
        if let Ok(state_mut) = editor.window_mut(window) {
            state_mut.coladd = coladd;
            if let Some(curswant) = curswant {
                state_mut.curswant = curswant;
                state_mut.set_curswant = false;
            }
        }
    } else if coladd.is_some() || curswant.is_some() {
        // `winrestview` with only `coladd`/`curswant` (no `lnum`) still
        // applies those fields to the current cursor position.
        if let Ok(state_mut) = editor.window_mut(window) {
            if let Some(coladd) = coladd {
                state_mut.coladd = coladd;
            }
            if let Some(curswant) = curswant {
                state_mut.curswant = curswant;
                state_mut.set_curswant = false;
            }
        }
    }
    Ok(Typval::Number(0))
}

/// `wincol()` (`eval/window.c:f_wincol`): the cursor's one-based window column.
fn call_wincol(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: wincol",
        ));
    }
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let cursor = state.cursor;
    let coladd = state.coladd;
    let buffer = editor
        .buffer(state.buffer)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let text = buffer
        .text()
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let line = text
        .line(cursor.lnum)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let virtual_column = cursor_vcol(&line, cursor.col, position_tabstop(editor));
    let coladd = usize::try_from(coladd.max(0)).unwrap_or(usize::MAX);
    let column = virtual_column.saturating_add(coladd).saturating_add(1);
    Ok(Typval::Number(i64::try_from(column).unwrap_or(i64::MAX)))
}

/// `winline()` (`eval/window.c:f_winline`): the cursor's window row (1-based).
/// Computed from cursor lnum minus the window's topline.
fn call_winline(editor: &Editor, _args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window"))?;
    let state = editor
        .window(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    let row = state
        .cursor
        .lnum
        .saturating_sub(state.topline)
        .saturating_add(1);
    Ok(Typval::Number(i64::try_from(row).unwrap_or(i64::MAX)))
}

fn call_getwinvar(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: getwinvar",
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: getwinvar",
        ));
    }
    let fallback = args
        .get(2)
        .cloned()
        .unwrap_or(Typval::String(OxStr::from("")));
    let number = typval_number(&args[0]).unwrap_or(0);
    let Some(tab) = editor.current_tabpage() else {
        return Ok(fallback);
    };
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let window = if number <= 0 {
        editor.current_window()
    } else {
        one_based_index(number).and_then(|index| windows.get(index).copied())
    };
    let Some(window) = window else {
        return Ok(fallback);
    };
    let name = crate::excmd_exec::typval_to_text(&args[1]);
    if let Some(option) = name.strip_prefix('&') {
        let Some(metadata) = crate::option_metadata(option) else {
            return Ok(fallback);
        };
        let value = if metadata
            .scopes
            .contains(&crate::options::OptionScope::Window)
        {
            editor.options().get_window(window, metadata.name).ok()
        } else {
            editor.options().get_global(metadata.name).ok()
        };
        return Ok(value.map_or(fallback, |value| match value {
            crate::options::OptionValue::Boolean(flag) => Typval::Number(i64::from(*flag)),
            crate::options::OptionValue::Number(number) => Typval::Number(*number),
            crate::options::OptionValue::String(text) => Typval::String(OxStr::from(text.as_str())),
        }));
    }
    let Ok(variables) = editor.window_variables(window) else {
        return Ok(fallback);
    };
    if name.is_empty() {
        return Ok(Typval::dict_with_entries(
            variables
                .0
                .iter()
                .map(|(key, value)| {
                    scope_var_entry(
                        ScopeKind::Window,
                        key,
                        &crate::excmd_exec::object_to_typval(value),
                    )
                })
                .collect(),
        ));
    }
    Ok(variables
        .0
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .map_or(fallback, |(_, value)| {
            crate::excmd_exec::object_to_typval(value)
        }))
}

/// `winlayout([{tabnr}])` (`eval/window.c:f_winlayout`): returns the tiled
/// window layout of the current or named tabpage as a nested list.
///
/// Each leaf frame yields `['leaf', winid]`; a row of vertical splits yields
/// `['row', [children]]`; a column of horizontal splits yields
/// `['col', [children]]`. An unknown tabpage number returns an empty list,
/// matching upstream `find_tabpage` returning `NULL`.
fn call_winlayout(editor: &Editor, args: &[Typval]) -> Typval {
    let tab = if args.is_empty() {
        editor.current_tabpage()
    } else {
        let number = typval_number(&args[0]).unwrap_or(0);
        match number.cmp(&0) {
            std::cmp::Ordering::Equal => editor.current_tabpage(),
            std::cmp::Ordering::Greater => {
                one_based_index(number).and_then(|index| editor.tabpages().get(index).copied())
            }
            std::cmp::Ordering::Less => None,
        }
    };
    let Some(tab) = tab else {
        return Typval::list(Vec::new());
    };
    let Ok(tabpage) = editor.tabpage(tab) else {
        return Typval::list(Vec::new());
    };
    frame_layout(tabpage.layout().root())
}

/// Recursively converts a tiled frame into the `winlayout()` nested-list shape
/// (`eval/window.c:get_framelayout`).
fn frame_layout(frame: &Frame) -> Typval {
    match frame {
        Frame::Leaf(leaf) => Typval::list(vec![
            Typval::String(OxStr::from("leaf")),
            Typval::Number(i64::from(leaf.window)),
        ]),
        Frame::Row { children, .. } => Typval::list(vec![
            Typval::String(OxStr::from("row")),
            Typval::list(children.iter().map(frame_layout).collect()),
        ]),
        Frame::Column { children, .. } => Typval::list(vec![
            Typval::String(OxStr::from("col")),
            Typval::list(children.iter().map(frame_layout).collect()),
        ]),
    }
}

/// `f_setwinvar` (`eval/window.c:862-885`): set one window-local variable
/// (or `&option`) in the numbered window, current when the number is 0.
/// An unresolvable window fails E957 like upstream's `find_win_by_nr`.
fn call_setwinvar(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 3 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: setwinvar",
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: setwinvar",
        ));
    }
    let number = typval_number(&args[0]).unwrap_or(0);
    let tab = editor
        .current_tabpage()
        .ok_or_else(|| EvalError::new("E957", 0, "Invalid window number"))?;
    let windows = editor.tabpage_windows(tab).unwrap_or_default();
    let window = if number <= 0 {
        editor.current_window()
    } else {
        one_based_index(number).and_then(|index| windows.get(index).copied())
    };
    let Some(window) = window else {
        return Err(EvalError::new("E957", 0, "Invalid window number"));
    };
    let name = typval_to_text(&args[1]);
    if let Some(option) = name.strip_prefix('&') {
        let Some(metadata) = crate::option_metadata(option) else {
            return Ok(Typval::Number(0));
        };
        if metadata
            .scopes
            .contains(&crate::options::OptionScope::Window)
        {
            let value = match args[2].clone() {
                Typval::Number(value) => {
                    if metadata.value_type == crate::options::OptionType::Boolean {
                        OptionValue::Boolean(value != 0)
                    } else {
                        OptionValue::Number(value)
                    }
                }
                Typval::String(text) => OptionValue::String(text.to_string_lossy().into_owned()),
                other => {
                    return Err(EvalError::new(
                        "E728",
                        0,
                        format!("Use a Number or a String for a window option: {other:?}"),
                    ));
                }
            };
            editor
                .options_mut()
                .set_window(window, metadata.name, value)
                .map_err(|error| EvalError::new("E355", 0, error.to_string()))?;
        }
        return Ok(Typval::Number(0));
    }
    let object = crate::excmd_exec::typval_to_object(&args[2]);
    editor
        .window_variables_mut(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?
        .insert(OxStr::from(name.as_str()), object);
    Ok(Typval::Number(0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use ox_text::Buffer;

    use super::*;

    /// One listed buffer shown in an 80x24 tabpage: the fresh editor state
    /// the other builtin test modules build.
    fn editor_with_window() -> Editor {
        let mut editor = Editor::new();
        let buffer = editor
            .create_buffer_with(Buffer::from_bytes(b"alpha\nbravo\n").unwrap(), true)
            .unwrap();
        editor
            .create_tabpage(buffer, crate::Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        editor
    }

    /// `f_getwininfo` (`eval/window.c:431-464`): no argument covers every
    /// window of every tabpage; an id narrows to exactly that window, and
    /// the entry reports the shown buffer.
    #[test]
    fn getwininfo_lists_windows() {
        let editor = editor_with_window();
        let current = editor.current_window().unwrap();
        let winid = i64::from(current);

        let all = call_getwininfo(&editor, &[]).unwrap();
        let Typval::List(list) = &all else {
            panic!("getwininfo() must return a list, got {all:?}")
        };
        assert!(!list.borrow().items.is_empty());

        let one = call_getwininfo(&editor, &[Typval::Number(winid)]).unwrap();
        let Typval::List(list) = &one else {
            panic!("getwininfo(winid) must return a list, got {one:?}")
        };
        let items = list.borrow().items.clone();
        assert_eq!(items.len(), 1);
        let Typval::Dict(dict) = &items[0] else {
            panic!("getwininfo entry must be a dict")
        };
        let field = |key: &[u8]| {
            dict.borrow()
                .entries
                .iter()
                .find(|entry| entry.key.as_bytes() == key)
                .map(|entry| entry.value.clone())
        };
        let expected_buffer = i64::from(editor.window(current).unwrap().buffer);
        assert_eq!(field(b"bufnr"), Some(Typval::Number(expected_buffer)));
        assert_eq!(field(b"winid"), Some(Typval::Number(winid)));

        let unknown = call_getwininfo(&editor, &[Typval::Number(9_999_999)]).unwrap();
        let Typval::List(list) = &unknown else {
            panic!("getwininfo(unknown id) must return a list")
        };
        assert!(list.borrow().items.is_empty());
    }
}
