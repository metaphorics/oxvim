//! Position builtins: the cursor, the marks it can be placed from, and the
//! columns derived from either.
//!
//! Upstream serves `cursor`, `setcursorcharpos`, `getpos`, `getcharpos`,
//! `getcurpos`, `getcursorcharpos`, `setpos`, `setcharpos`, `col`, `charcol`,
//! `line` and `virtcol` from `eval/funcs.c`, but every one of them resolves
//! its expression through `eval.c:var2fpos` and its list forms through
//! `eval.c:list2fpos`. Both are ported here so the forms stay in one place.
//! Virtual columns follow `plines.c:getvcol`, and the wanted column follows
//! `move.c:update_curswant`.

use ox_eval::EvalError;
use ox_text::Position;
use ox_types::{BufHandle, Typval, WinHandle};
use unicode_width::UnicodeWidthChar;

use crate::excmd_exec::{buffer_lines, EvalHost};
use crate::options::OptionValue;
use crate::script::FileIO;
use crate::{Editor, MarkLocation};

use super::input_string_arg;

/// Upstream `MAXCOL` (`pos_defs.h`): one past every representable column.
const MAXCOL: i64 = 0x7fff_ffff;

/// Routes one position builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    let editor = &mut *host.editor;
    match name {
        "charcol" => get_col(editor, &args, true),
        "col" => get_col(editor, &args, false),
        "cursor" => set_cursorpos(editor, &args, false),
        "getcharpos" => getpos_both(editor, &args, false, true),
        "getcurpos" => getpos_both(editor, &args, true, false),
        "getcursorcharpos" => getpos_both(editor, &args, true, true),
        "getpos" => getpos_both(editor, &args, false, false),
        "line" => call_line(editor, &args),
        "setcharpos" => set_position(editor, &args, true),
        "setcursorcharpos" => set_cursorpos(editor, &args, true),
        "setpos" => set_position(editor, &args, false),
        "virtcol" => call_virtcol(editor, &args),
        _ => unreachable!("position builtin route and dispatcher disagree"),
    }
}

/// Enforces the `eval.lua` argument counts the way upstream's function table
/// does before a builtin body runs.
fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = ox_eval::builtin_spec(name).expect("position builtins come from eval.lua");
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

// ---------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------

/// Upstream `pos_T`: a one-based line, a zero-based column that is a byte
/// index, a character index or [`MAXCOL`], and the virtual offset past it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FPos {
    lnum: i64,
    col: i64,
    coladd: i64,
}

/// The window a builtin resolves against, together with its buffer text. Every
/// entry point takes this snapshot once so `var2fpos` never re-reads the
/// buffer per expression form.
struct PosWin {
    window: WinHandle,
    buffer: BufHandle,
    cursor: Position,
    coladd: i64,
    topline: usize,
    lines: Vec<Vec<u8>>,
}

impl PosWin {
    /// Snapshots `window`'s cursor and buffer text.
    fn new(editor: &Editor, window: WinHandle) -> ox_eval::Result<Self> {
        let state = editor
            .window(window)
            .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
        let (buffer, cursor, coladd, topline) =
            (state.buffer, state.cursor, state.coladd, state.topline);
        let lines =
            buffer_lines(editor, buffer).map_err(|error| EvalError::new("E16", 0, error))?;
        Ok(Self {
            window,
            buffer,
            cursor,
            coladd,
            topline,
            lines,
        })
    }

    /// The text of a one-based line, empty when the line is out of range.
    fn line(&self, lnum: i64) -> &[u8] {
        usize::try_from(lnum)
            .ok()
            .and_then(|lnum| self.lines.get(lnum.wrapping_sub(1)))
            .map_or(&[], Vec::as_slice)
    }

    /// Upstream `b_ml.ml_line_count`.
    fn line_count(&self) -> i64 {
        self.lines.len() as i64
    }

    /// Upstream `w_botline - 1`, the last line the window displays. ox-editor
    /// tracks only `topline`, so the last row is derived the way the `H`/`L`
    /// motions derive it, without wrap or fold accounting.
    fn botline(&self, editor: &Editor) -> i64 {
        let height = editor
            .window_geometry(self.window)
            .map_or(1, |geometry| geometry.height);
        let last = self.topline.saturating_add(height.saturating_sub(1));
        (last as i64).min(self.line_count().max(1))
    }
}

/// `eval.c:var2fpos`. Resolves a `[lnum, col, off]` list or one of the
/// documented expression forms into a position in the window's buffer, or
/// `None` when the expression names no position. `fnum` receives the buffer
/// number of a global or numbered mark, which is the only case upstream
/// reports one.
fn var2fpos(
    editor: &Editor,
    value: &Typval,
    dollar_lnum: bool,
    charcol: bool,
    win: &PosWin,
    fnum: &mut i64,
) -> ox_eval::Result<Option<FPos>> {
    if let Typval::List(reference) = value {
        let list = reference
            .try_borrow()
            .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
        let items = &list.items;
        let Some(lnum) = list_nr(items, 0)? else {
            return Ok(None);
        };
        if lnum <= 0 || lnum > win.line_count() {
            return Ok(None);
        }
        let Some(mut col) = list_nr(items, 1)? else {
            return Ok(None);
        };
        let line = win.line(lnum);
        let len = if charcol {
            char_count(line) as i64
        } else {
            line.len() as i64
        };
        // A column of "$" asks for the last column of the line.
        if matches!(items.get(1), Some(Typval::String(text)) if text.as_bytes() == b"$") {
            col = len + 1;
        }
        if col == 0 || col > len + 1 {
            return Ok(None);
        }
        let coladd = list_nr(items, 2)?.unwrap_or(0);
        return Ok(Some(FPos {
            lnum,
            col: col - 1,
            coladd,
        }));
    }

    let name = input_string_arg(value)?;
    let bytes = name.as_bytes();
    let cursor = FPos {
        lnum: win.cursor.lnum as i64,
        col: win.cursor.col as i64,
        coladd: win.coladd,
    };
    let mut pos = FPos::default();
    match bytes.first() {
        Some(b'.') => pos = cursor,
        // Visual state lives in the mode machine rather than in editor state,
        // so the eval host only ever reaches upstream's Visual-inactive branch.
        Some(b'v') if bytes.len() == 1 => pos = cursor,
        Some(b'\'') => {
            let Some(mark) = bytes.get(1).copied().map(char::from) else {
                return Ok(None);
            };
            let Some(position) = mark_position(editor, win.buffer, mark, fnum) else {
                return Ok(None);
            };
            if position.lnum == 0 {
                return Ok(None);
            }
            pos = FPos {
                lnum: position.lnum as i64,
                col: position.col as i64,
                coladd: 0,
            };
        }
        _ => {}
    }
    if pos.lnum != 0 {
        if charcol {
            pos.col = byteidx_to_charidx(win.line(pos.lnum), pos.col);
        }
        return Ok(Some(pos));
    }

    if bytes.first() == Some(&b'w') && dollar_lnum {
        pos.col = 0;
        match bytes.get(1) {
            Some(b'0') => {
                pos.lnum = (win.topline.max(1)) as i64;
                return Ok(Some(pos));
            }
            Some(b'$') => {
                pos.lnum = win.botline(editor);
                return Ok(Some(pos));
            }
            _ => {}
        }
    } else if bytes.first() == Some(&b'$') {
        if dollar_lnum {
            pos.lnum = win.line_count();
            pos.col = 0;
        } else {
            pos.lnum = cursor.lnum;
            let line = win.line(pos.lnum);
            pos.col = if charcol {
                char_count(line) as i64
            } else {
                line.len() as i64
            };
        }
        return Ok(Some(pos));
    }
    Ok(None)
}

/// `eval.c:list2fpos`. Converts `[bufnum, lnum, col, off, curswant]` — the
/// leading buffer number only when `fnum` is wanted — into a position whose
/// column is still one-based, the way upstream leaves it for its callers.
fn list2fpos(
    editor: &Editor,
    value: &Typval,
    with_fnum: bool,
    charcol: bool,
    win: &PosWin,
    fnum: &mut i64,
    curswant: &mut i64,
) -> ox_eval::Result<Option<FPos>> {
    let Typval::List(reference) = value else {
        return Ok(None);
    };
    let list = reference
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    let items = &list.items;
    let (minimum, maximum) = if with_fnum { (3, 5) } else { (2, 4) };
    if items.len() < minimum || items.len() > maximum {
        return Ok(None);
    }
    let mut index = 0;
    if with_fnum {
        let number = list_nr(items, index)?.unwrap_or(-1);
        index += 1;
        if number < 0 {
            return Ok(None);
        }
        *fnum = if number == 0 {
            i64::from(win.buffer)
        } else {
            number
        };
    }
    let Some(lnum) = list_nr(items, index)? else {
        return Ok(None);
    };
    index += 1;
    if lnum < 0 {
        return Ok(None);
    }
    let Some(mut col) = list_nr(items, index)? else {
        return Ok(None);
    };
    index += 1;
    if col < 0 {
        return Ok(None);
    }
    if charcol {
        // Upstream converts the character index against the buffer `fnum`
        // names, not the current one, and falls back to the cursor line only
        // when `lnum` is zero. A buffer it cannot load fails the conversion.
        let Some(buffer) = buflist_findnr(editor, *fnum) else {
            return Ok(None);
        };
        let lnum = if lnum == 0 {
            win.cursor.lnum as i64
        } else {
            lnum
        };
        let foreign;
        let line: &[u8] = if buffer == win.buffer {
            win.line(lnum)
        } else {
            let Ok(lines) = buffer_lines(editor, buffer) else {
                return Ok(None);
            };
            foreign = lines;
            usize::try_from(lnum)
                .ok()
                .and_then(|lnum| foreign.get(lnum.wrapping_sub(1)))
                .map_or(&[], Vec::as_slice)
        };
        col = charidx_to_byteidx(line, col) + 1;
    }
    let coladd = list_nr(items, index)?.unwrap_or(-1).max(0);
    *curswant = list_nr(items, index + 1)?.unwrap_or(-1);
    Ok(Some(FPos { lnum, col, coladd }))
}

/// `mark.c:mark_get` restricted to the marks ox-editor models: the
/// buffer-local marks, and the global `A-Z`/`0-9` marks, which also report the
/// buffer they live in.
fn mark_position(
    editor: &Editor,
    buffer: BufHandle,
    name: char,
    fnum: &mut i64,
) -> Option<Position> {
    if let Ok(Some(position)) = editor.local_mark(buffer, name) {
        return Some(position);
    }
    if !(name.is_ascii_uppercase() || name.is_ascii_digit()) {
        return None;
    }
    let location = editor.global_marks().get(name).ok().flatten()?;
    *fnum = location.buffer().map_or(0, i64::from);
    Some(location.position)
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

/// `getpos()`, `getcharpos()`, `getcurpos()` and `getcursorcharpos()`
/// (`funcs.c:getpos_both`). The cursor forms answer a fifth element, the
/// wanted column, that the expression forms never carry.
fn getpos_both(
    editor: &Editor,
    args: &[Typval],
    getcurpos: bool,
    charcol: bool,
) -> ox_eval::Result<Typval> {
    let mut fnum = -1;
    let window = if getcurpos {
        match args.first() {
            Some(value) => resolve_window_nr_or_id(editor, value)?,
            None => editor.current_window(),
        }
    } else {
        editor.current_window()
    };
    let win = window.map(|window| PosWin::new(editor, window)).transpose()?;
    let pos = match (&win, getcurpos) {
        (None, _) => None,
        (Some(win), true) => {
            let mut col = win.cursor.col as i64;
            if charcol {
                col = byteidx_to_charidx(win.line(win.cursor.lnum as i64), col);
            }
            Some(FPos {
                lnum: win.cursor.lnum as i64,
                col,
                coladd: win.coladd,
            })
        }
        (Some(win), false) => var2fpos(editor, &args[0], true, charcol, win, &mut fnum)?,
    };

    let mut items = vec![
        Typval::Number(if fnum == -1 { 0 } else { fnum }),
        Typval::Number(pos.map_or(0, |pos| pos.lnum)),
        Typval::Number(pos.map_or(0, |pos| {
            if pos.col == MAXCOL {
                MAXCOL
            } else {
                pos.col + 1
            }
        })),
        Typval::Number(pos.map_or(0, |pos| pos.coladd)),
    ];
    if getcurpos {
        let curswant = match &win {
            None => 0,
            Some(win) => {
                let wanted = effective_curswant(editor, win);
                if wanted == MAXCOL {
                    MAXCOL
                } else {
                    wanted + 1
                }
            }
        };
        items.push(Typval::Number(curswant));
    }
    Ok(Typval::list(items))
}

/// `move.c:update_curswant` without its side effect: a window that has asked
/// for a refresh answers the cursor's virtual column, and any other window
/// answers the column it was last told to want. Upstream only refreshes the
/// current window, so a `getcurpos(winid)` on a background window reads
/// `w_curswant` raw.
fn effective_curswant(editor: &Editor, win: &PosWin) -> i64 {
    let state = editor.window(win.window);
    let (curswant, set_curswant) = state.map_or((0, false), |state| {
        (state.curswant, state.set_curswant)
    });
    if !set_curswant || editor.current_window() != Some(win.window) {
        return curswant;
    }
    let line = win.line(win.cursor.lnum as i64);
    let col = win.cursor.col as i64;
    let (start, end) = getvcol(line, col, tabstop(editor));
    // `getvcol` puts the cursor at the end of a tab and at the start of
    // anything else.
    let vcol = if line.get(win.cursor.col) == Some(&b'\t') {
        end
    } else {
        start
    };
    vcol as i64
}

/// `col()` and `charcol()` (`funcs.c:get_col`).
fn get_col(editor: &Editor, args: &[Typval], charcol: bool) -> ox_eval::Result<Typval> {
    if !matches!(args[0], Typval::String(_) | Typval::List(_)) {
        return Err(EvalError::new(
            "E1222",
            0,
            "String or List required for argument 1",
        ));
    }
    let window = match args.get(1) {
        Some(value) => {
            require_number(value, 2)?;
            resolve_window_id(editor, value)?
        }
        None => editor.current_window(),
    };
    let Some(window) = window else {
        return Ok(Typval::Number(0));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = i64::from(win.buffer);
    let Some(pos) = var2fpos(editor, &args[0], false, charcol, &win, &mut fnum)? else {
        return Ok(Typval::Number(0));
    };
    if fnum != i64::from(win.buffer) {
        return Ok(Typval::Number(0));
    }
    let col = if pos.col == MAXCOL {
        // A `'>` mark can hold MAXCOL; answer the length of the line instead.
        if pos.lnum <= win.line_count() {
            win.line(pos.lnum).len() as i64 + 1
        } else {
            MAXCOL
        }
    } else {
        pos.col + 1
    };
    Ok(Typval::Number(col))
}

/// `line()` (`funcs.c:f_line`).
fn call_line(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = match args.get(1) {
        Some(value) => resolve_window_id(editor, value)?,
        None => editor.current_window(),
    };
    let Some(window) = window else {
        return Ok(Typval::Number(0));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = 0;
    let pos = var2fpos(editor, &args[0], true, false, &win, &mut fnum)?;
    Ok(Typval::Number(pos.map_or(0, |pos| pos.lnum)))
}

/// `virtcol()` (`funcs.c:f_virtcol`). The optional third argument names a
/// window only when the second one was given, matching upstream's guard.
fn call_virtcol(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let list_result = args.get(1).is_some_and(Typval::is_truthy);
    let pair = |start: i64, end: i64| {
        if list_result {
            Typval::list(vec![Typval::Number(start), Typval::Number(end)])
        } else {
            Typval::Number(end)
        }
    };
    let window = match args.get(2) {
        Some(value) => resolve_window_id(editor, value)?,
        None => editor.current_window(),
    };
    let Some(window) = window else {
        return Ok(pair(0, 0));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = i64::from(win.buffer);
    let Some(pos) = var2fpos(editor, &args[0], false, false, &win, &mut fnum)? else {
        return Ok(pair(0, 0));
    };
    if pos.lnum > win.line_count() || fnum != i64::from(win.buffer) {
        return Ok(pair(0, 0));
    }
    let line = win.line(pos.lnum);
    // getvcol() does not range-check, so upstream clamps the column first.
    let col = pos.col.clamp(0, line.len() as i64);
    let (start, end) = getvcol(line, col, tabstop(editor));
    let (start, end) = wrap_showbreak(editor, window, start + 1, end + 1);
    Ok(pair(start as i64, end as i64))
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// `cursor()` and `setcursorcharpos()` (`funcs.c:set_cursorpos`).
fn set_cursorpos(
    editor: &mut Editor,
    args: &[Typval],
    charcol: bool,
) -> ox_eval::Result<Typval> {
    let Some(window) = editor.current_window() else {
        return Ok(Typval::Number(-1));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = i64::from(win.buffer);
    let mut curswant = -1;
    let mut set_curswant = true;
    let (mut lnum, mut col, coladd);
    if matches!(args[0], Typval::List(_)) {
        let Some(pos) =
            list2fpos(editor, &args[0], false, charcol, &win, &mut fnum, &mut curswant)?
        else {
            return Err(EvalError::new("E474", 0, "Invalid argument"));
        };
        lnum = pos.lnum;
        col = pos.col;
        coladd = pos.coladd;
        if curswant >= 0 {
            set_curswant = false;
        }
    } else if matches!(args[0], Typval::Number(_) | Typval::String(_))
        && matches!(args.get(1), Some(Typval::Number(_) | Typval::String(_)))
    {
        lnum = get_lnum(editor, &args[0], &win)?;
        if lnum < 0 {
            return Err(EvalError::new(
                "E475",
                0,
                format!(
                    "Invalid argument: {}",
                    input_string_arg(&args[0])?.to_string_lossy()
                ),
            ));
        }
        if lnum == 0 {
            lnum = win.cursor.lnum as i64;
        }
        col = number_value(&args[1])?;
        if charcol {
            col = charidx_to_byteidx(win.line(lnum), col) + 1;
        }
        coladd = match args.get(2) {
            Some(value) => number_value(value)?,
            None => 0,
        };
    } else {
        return Err(EvalError::new("E474", 0, "Invalid argument"));
    }
    if lnum < 0 || col < 0 || coladd < 0 {
        return Ok(Typval::Number(-1));
    }
    let mut pos = FPos {
        lnum: if lnum > 0 { lnum } else { win.cursor.lnum as i64 },
        col: if col != MAXCOL { (col - 1).max(0) } else { col },
        coladd,
    };
    check_cursor(&win, &mut pos);
    place_cursor(editor, window, pos, curswant, Some(set_curswant))?;
    Ok(Typval::Number(0))
}

/// `setpos()` and `setcharpos()` (`funcs.c:set_position`).
fn set_position(
    editor: &mut Editor,
    args: &[Typval],
    charcol: bool,
) -> ox_eval::Result<Typval> {
    let name = input_string_arg(&args[0])?;
    let Some(window) = editor.current_window() else {
        return Ok(Typval::Number(-1));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = i64::from(win.buffer);
    let mut curswant = -1;
    let Some(mut pos) =
        list2fpos(editor, &args[1], true, charcol, &win, &mut fnum, &mut curswant)?
    else {
        return Ok(Typval::Number(-1));
    };
    if pos.col != MAXCOL {
        pos.col = (pos.col - 1).max(0);
    }
    let bytes = name.as_bytes();
    if bytes == b"." {
        check_cursor(&win, &mut pos);
        // Upstream only touches `w_set_curswant` when the list carried a
        // wanted column; the four-element form leaves it alone.
        place_cursor(editor, window, pos, curswant, None)?;
        return Ok(Typval::Number(0));
    }
    if bytes.len() == 2 && bytes[0] == b'\'' {
        return Ok(Typval::Number(set_mark(
            editor,
            &win,
            char::from(bytes[1]),
            pos,
            fnum,
        )));
    }
    Err(EvalError::new("E474", 0, "Invalid argument"))
}

/// `mark.c:setmark_pos` for the marks ox-editor models: `a-z`, the previous
/// context marks `'` and `` ` ``, and the global `A-Z`/`0-9` marks. Upstream
/// also writes `[`, `]`, `<`, `>` and `"`, which ox-editor does not store, so
/// those answer the failure it answers for a name it cannot set.
fn set_mark(editor: &mut Editor, win: &PosWin, name: char, pos: FPos, fnum: i64) -> i64 {
    let position = Position {
        lnum: usize::try_from(pos.lnum).unwrap_or(0),
        col: usize::try_from(pos.col).unwrap_or(0),
    };
    // Upstream answers the previous-context marks before it looks the buffer
    // up, so a nonexistent `fnum` never reaches them. They live in the window
    // (`w_pcmark`), which ox-editor models as a mark in the current buffer.
    if name == '\'' || name == '`' {
        return if editor.set_local_mark(win.buffer, name, position).is_ok() {
            0
        } else {
            -1
        };
    }
    // `mark.c`: a mark cannot be set in a buffer that does not exist.
    let Some(buffer) = buflist_findnr(editor, fnum) else {
        return -1;
    };
    let stored = if name.is_ascii_uppercase() || name.is_ascii_digit() {
        let location = MarkLocation::in_buffer(buffer, position);
        editor.global_marks_mut().set(name, location).is_ok()
    } else if name.is_ascii_lowercase() {
        // Upstream writes `b_namedm` of the buffer `fnum` names, not the
        // current one.
        editor.set_local_mark(buffer, name, position).is_ok()
    } else {
        false
    };
    if stored {
        0
    } else {
        -1
    }
}

/// `buffer.c:buflist_findnr`: the live buffer with this number, or `None`.
fn buflist_findnr(editor: &Editor, fnum: i64) -> Option<BufHandle> {
    let buffer = BufHandle::try_from(fnum).ok()?;
    editor.buffer(buffer).ok().map(|_| buffer)
}

/// Writes a resolved position into the window, together with the wanted
/// column upstream derives from the same call. `set_curswant` is `None` for
/// the callers upstream leaves `w_set_curswant` untouched in.
fn place_cursor(
    editor: &mut Editor,
    window: WinHandle,
    pos: FPos,
    curswant: i64,
    set_curswant: Option<bool>,
) -> ox_eval::Result<()> {
    let state = editor
        .window_mut(window)
        .map_err(|error| EvalError::new("E957", 0, error.to_string()))?;
    state.cursor = Position {
        lnum: usize::try_from(pos.lnum).unwrap_or(1),
        col: usize::try_from(pos.col).unwrap_or(0),
    };
    state.coladd = pos.coladd;
    if curswant >= 0 {
        state.curswant = curswant - 1;
        state.set_curswant = false;
    } else if let Some(flag) = set_curswant {
        state.set_curswant = flag;
    }
    Ok(())
}

/// `cursor.c:check_cursor` followed by `mbyte.c:mb_adjust_cursor`, for the
/// only mode the eval host runs in: Normal mode with `'virtualedit'` unset,
/// where the cursor may not sit past the last character of the line.
fn check_cursor(win: &PosWin, pos: &mut FPos) {
    pos.lnum = pos.lnum.clamp(1, win.line_count().max(1));
    let line = win.line(pos.lnum);
    let len = line.len() as i64;
    if len == 0 {
        pos.col = 0;
    } else if pos.col >= len {
        pos.col = len - 1;
    } else if pos.col < 0 {
        pos.col = 0;
    }
    if pos.col == MAXCOL {
        pos.coladd = 0;
    }
    adjust_to_head_byte(line, pos);
}

/// `mark.c:mark_mb_adjustpos`: pull a column that landed inside a multibyte
/// character back onto that character's first byte.
fn adjust_to_head_byte(line: &[u8], pos: &mut FPos) {
    if pos.col <= 0 {
        return;
    }
    let col = pos.col as usize;
    if line.is_empty() || line.len() < col {
        pos.col = 0;
        return;
    }
    let mut index = 0;
    while index < line.len() {
        let length = cluster_len(line, index);
        if index + length > col {
            break;
        }
        index += length;
    }
    pos.col = index as i64;
}

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

/// `plines.c:getvcol`: the zero-based first and last virtual column of the
/// character at byte `col`, with the NUL past the end of the line counting as
/// one cell.
fn getvcol(line: &[u8], col: i64, tabstop: usize) -> (usize, usize) {
    let target = usize::try_from(col).unwrap_or(0);
    let mut vcol = 0usize;
    let mut index = 0usize;
    let width = loop {
        if index >= line.len() {
            break 1;
        }
        let (character, _) = decode_char(&line[index..]);
        let length = cluster_len(line, index);
        let width = cell_width(character, vcol, tabstop);
        if index + length > target {
            break width;
        }
        vcol += width;
        index += length;
    };
    (vcol, vcol + width - 1)
}

/// Display cells one character cluster occupies
/// (`plines.c:charsize_fast_impl`).
fn cell_width(character: char, vcol: usize, tabstop: usize) -> usize {
    match character {
        '\t' => tabstop - (vcol % tabstop),
        control if (control as u32) < 0x20 || control as u32 == 0x7f => 2,
        _ => UnicodeWidthChar::width(character).unwrap_or(1).max(1),
    }
}

/// Adds the `'showbreak'` cells that precede every continuation row a long
/// line wraps onto.
fn wrap_showbreak(
    editor: &Editor,
    window: WinHandle,
    start: usize,
    end: usize,
) -> (usize, usize) {
    let showbreak = match editor.options().get_window(window, "showbreak") {
        Ok(OptionValue::String(value)) => value.chars().count(),
        _ => 0,
    };
    if showbreak == 0 {
        return (start, end);
    }
    let width = editor
        .window_geometry(window)
        .map_or(0, |geometry| geometry.width);
    let continuation = width.saturating_sub(showbreak).max(1);
    let wrapped = |column: usize| {
        if column <= width {
            column
        } else {
            let rows = 1 + (column - width - 1) / continuation;
            column.saturating_add(rows.saturating_mul(showbreak))
        }
    };
    (wrapped(start), wrapped(end))
}

/// The effective `'tabstop'`.
fn tabstop(editor: &Editor) -> usize {
    match editor.options().get_global("tabstop") {
        Ok(OptionValue::Number(value)) => usize::try_from((*value).max(1)).unwrap_or(8),
        _ => 8,
    }
}

// ---------------------------------------------------------------------------
// Characters
// ---------------------------------------------------------------------------

/// Decodes the scalar at the front of `bytes`, or a single replacement byte
/// when the encoding is invalid, the way Vim treats a stray byte.
fn decode_char(bytes: &[u8]) -> (char, usize) {
    for width in 1..=bytes.len().min(4) {
        if let Ok(text) = std::str::from_utf8(&bytes[..width]) {
            if let Some(character) = text.chars().next() {
                return (character, width);
            }
        }
    }
    (char::REPLACEMENT_CHARACTER, 1)
}

/// Length in bytes of the character cluster at `index`: one base character
/// plus the composing characters that follow it (`mbyte.c:utfc_ptr2len`).
fn cluster_len(line: &[u8], index: usize) -> usize {
    let (_, mut length) = decode_char(&line[index..]);
    loop {
        let next = index + length;
        if next >= line.len() {
            return length;
        }
        let (character, size) = decode_char(&line[next..]);
        if UnicodeWidthChar::width(character) != Some(0) {
            return length;
        }
        length += size;
    }
}

/// Length of a line in characters (`mbyte.c:mb_charlen`).
fn char_count(line: &[u8]) -> usize {
    let mut index = 0;
    let mut count = 0;
    while index < line.len() {
        index += cluster_len(line, index);
        count += 1;
    }
    count
}

/// `eval.c:buf_byteidx_to_charidx`.
fn byteidx_to_charidx(line: &[u8], byteidx: i64) -> i64 {
    if line.is_empty() {
        return 0;
    }
    let target = usize::try_from(byteidx).unwrap_or(0);
    let mut index = 0usize;
    let mut count = 0i64;
    while index < line.len() && index <= target {
        index += cluster_len(line, index);
        count += 1;
    }
    if index >= line.len() && byteidx != 0 && index == target {
        count += 1;
    }
    count - 1
}

/// `eval.c:buf_charidx_to_byteidx`.
fn charidx_to_byteidx(line: &[u8], charidx: i64) -> i64 {
    let mut index = 0usize;
    let mut remaining = charidx;
    loop {
        remaining -= 1;
        if index >= line.len() || remaining <= 0 {
            return index as i64;
        }
        index += cluster_len(line, index);
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// `typval.c:tv_get_number_chk`: the numeric value of a typval, or the error
/// upstream raises for a type that has none.
fn number_value(value: &Typval) -> ox_eval::Result<i64> {
    match value {
        Typval::Number(number) => Ok(*number),
        Typval::Bool(flag) => Ok(i64::from(*flag)),
        Typval::Special(_) => Ok(0),
        Typval::String(text) => Ok(leading_number(&text.to_string_lossy())),
        Typval::Channel(id) | Typval::Job(id) => Ok(i64::try_from(*id).unwrap_or(i64::MAX)),
        Typval::List(_) => Err(EvalError::new("E745", 0, "Using a List as a Number")),
        Typval::Dict(_) => Err(EvalError::new("E728", 0, "Using a Dictionary as a Number")),
        Typval::Float(_) => Err(EvalError::new("E805", 0, "Using a Float as a Number")),
        Typval::Blob(_) => Err(EvalError::new("E974", 0, "Using a Blob as a Number")),
        Typval::Funcref(_) | Typval::Partial(_) => Err(EvalError::new(
            "E703",
            0,
            "Using a Funcref as a Number",
        )),
    }
}

/// `charset.c:vim_str2nr` as `tv_get_number_chk` uses it: the decimal prefix
/// of a string, or zero when it has none.
fn leading_number(text: &str) -> i64 {
    let bytes = text.trim_start().as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == digits {
        return 0;
    }
    std::str::from_utf8(&bytes[..end])
        .ok()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

/// `typval.c:tv_list_find_nr`: the number at `index`, or `None` when the index
/// is absent. A present item with no numeric value raises upstream's error.
fn list_nr(items: &[Typval], index: usize) -> ox_eval::Result<Option<i64>> {
    match items.get(index) {
        None => Ok(None),
        Some(value) => number_value(value).map(Some),
    }
}

/// `typval.c:tv_check_for_number_arg`.
fn require_number(value: &Typval, argument: usize) -> ox_eval::Result<()> {
    if matches!(value, Typval::Number(_)) {
        return Ok(());
    }
    Err(EvalError::new(
        "E1210",
        0,
        format!("Number required for argument {argument}"),
    ))
}

/// `typval.c:tv_get_lnum`: a line number, falling back to the expression
/// forms when the value is not already a positive number.
fn get_lnum(editor: &Editor, value: &Typval, win: &PosWin) -> ox_eval::Result<i64> {
    let lnum = number_value(value)?;
    if lnum > 0 || matches!(value, Typval::Number(_)) {
        return Ok(lnum);
    }
    let mut fnum = 0;
    let pos = var2fpos(editor, value, true, false, win, &mut fnum)?;
    Ok(pos.map_or(lnum, |pos| pos.lnum))
}

/// `window.c:win_id2wp_tp`: a window id names a window, and `0` names none.
fn resolve_window_id(editor: &Editor, value: &Typval) -> ox_eval::Result<Option<WinHandle>> {
    let id = number_value(value)?;
    Ok(WinHandle::try_from(id)
        .ok()
        .filter(|window| !window.is_current() && editor.window(*window).is_ok()))
}

/// `eval/window.c:find_win_by_nr_or_id`: `0` names the current window.
/// ox-editor gives windows one identifier rather than upstream's separate
/// number and id spaces, so every other value is looked up as a handle.
fn resolve_window_nr_or_id(
    editor: &Editor,
    value: &Typval,
) -> ox_eval::Result<Option<WinHandle>> {
    if number_value(value)? == 0 {
        return Ok(editor.current_window());
    }
    resolve_window_id(editor, value)
}
