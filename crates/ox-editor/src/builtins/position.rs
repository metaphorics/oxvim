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

use crate::excmd_exec::ExEditorAccess;
use ox_eval::EvalError;
use ox_text::Position;
use ox_types::{BufHandle, OxStr, Typval, WinHandle};
use unicode_width::UnicodeWidthChar;

use crate::excmd_exec::{EvalHost, buffer_lines};
use crate::options::OptionValue;
use crate::script::FileIO;
use crate::{Editor, MarkLocation, Mode};

use super::input_string_arg;

/// Upstream `MAXCOL` (`pos_defs.h`): one past every representable column.
const MAXCOL: i64 = 0x7fff_ffff;

/// Converts an in-memory index into Vim's signed Number domain.
fn vim_number(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Routes one position builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    // Extract the visual anchor from the mode machine so getpos('v') and
    // getregion can resolve the start of the visual selection. The mode
    // machine lives on ExRuntime, not on Editor, so it must be read here
    // before entering with_ex_editor.
    let visual_anchor = host.runtime.mode_machine.as_ref().and_then(|machine| {
        let machine = machine.borrow();
        if let Mode::Visual(state) = machine.mode() {
            Some(FPos {
                lnum: vim_number(state.anchor.lnum),
                col: vim_number(state.anchor.col),
                coladd: 0,
            })
        } else {
            None
        }
    });
    host.access.with_ex_editor(|editor| match name {
        "charcol" => get_col(editor, args, true),
        "col" => get_col(editor, args, false),
        "cursor" => set_cursorpos(editor, args, false),
        "getcharpos" => getpos_both(editor, args, false, true, visual_anchor),
        "getcurpos" => getpos_both(editor, args, true, false, visual_anchor),
        "getcursorcharpos" => getpos_both(editor, args, true, true, visual_anchor),
        "getpos" => getpos_both(editor, args, false, false, visual_anchor),
        "getregion" => get_region(editor, args),
        "getregionpos" => get_region_pos(editor, args),
        "line" => call_line(editor, args),
        "line2byte" => call_line2byte(editor, args),
        "setcharpos" => set_position(editor, args, true),
        "setcursorcharpos" => set_cursorpos(editor, args, true),
        "setpos" => set_position(editor, args, false),
        "virtcol" => call_virtcol(editor, args),
        _ => unreachable!("position builtin route and dispatcher disagree"),
    })
}

/// Enforces the `eval.lua` argument counts the way upstream's function table
/// does before a builtin body runs.
fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = ox_eval::builtin_spec(name)
        .ok_or_else(|| EvalError::new("E117", 0, format!("Unknown function: {name}")))?;
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
    visual_anchor: Option<FPos>,
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
            visual_anchor: None,
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
        vim_number(self.lines.len())
    }

    /// Upstream `w_botline - 1`, the last line the window displays. ox-editor
    /// tracks only `topline`, so the last row is derived the way the `H`/`L`
    /// motions derive it, without wrap or fold accounting.
    fn botline(&self, editor: &Editor) -> i64 {
        let height = editor
            .window_geometry(self.window)
            .map_or(1, |geometry| geometry.height);
        let last = self.topline.saturating_add(height.saturating_sub(1));
        vim_number(last).min(self.line_count().max(1))
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
            vim_number(char_count(line))
        } else {
            vim_number(line.len())
        };
        let maximum_col = len.saturating_add(1);
        // A column of "$" asks for the last column of the line.
        if matches!(items.get(1), Some(Typval::String(text)) if text.as_bytes() == b"$") {
            col = maximum_col;
        }
        if col == 0 || col > maximum_col {
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
        lnum: vim_number(win.cursor.lnum),
        col: vim_number(win.cursor.col),
        coladd: win.coladd,
    };
    let mut pos = FPos::default();
    match bytes.first() {
        Some(b'.') => pos = cursor,
        // Visual anchor from the mode machine, or cursor when not in Visual.
        Some(b'v') if bytes.len() == 1 => pos = win.visual_anchor.unwrap_or(cursor),
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
                lnum: vim_number(position.lnum),
                col: vim_number(position.col),
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
                pos.lnum = vim_number(win.topline.max(1));
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
                vim_number(char_count(line))
            } else {
                vim_number(line.len())
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
            vim_number(win.cursor.lnum)
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
        col = charidx_to_byteidx(line, col).saturating_add(1);
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
    visual_anchor: Option<FPos>,
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
    let win = window
        .map(|window| PosWin::new(editor, window))
        .transpose()?
        .map(|mut win| {
            win.visual_anchor = visual_anchor;
            win
        });
    let pos = match (&win, getcurpos) {
        (None, _) => None,
        (Some(win), true) => {
            let mut col = vim_number(win.cursor.col);
            if charcol {
                col = byteidx_to_charidx(win.line(vim_number(win.cursor.lnum)), col);
            }
            Some(FPos {
                lnum: vim_number(win.cursor.lnum),
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
                pos.col.saturating_add(1)
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
                    wanted.saturating_add(1)
                }
            }
        };
        items.push(Typval::Number(curswant));
    }
    Ok(Typval::list(items))
}

// ---------------------------------------------------------------------------
// Region extraction
// ---------------------------------------------------------------------------

/// Selection type from the `{opts}` Dict of `getregion()`/`getregionpos()`
/// (`funcs.c:getregionpos`). `"v"` is charwise, `"V"` is linewise, and
/// `"\x16"` (Ctrl-V) optionally followed by a width is blockwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegionType {
    Char,
    Line,
    Block,
}

/// Parsed `{opts}` Dict for `getregion()`/`getregionpos()`.
struct RegionOpts {
    region_type: RegionType,
    exclusive: bool,
    block_width: i64,
    allow_eol: bool,
}

impl Default for RegionOpts {
    fn default() -> Self {
        Self {
            region_type: RegionType::Char,
            exclusive: false,
            block_width: 0,
            allow_eol: false,
        }
    }
}

/// `funcs.c:getregionpos` argument parsing: both positions must be 4-element
/// lists, the optional third argument is a Dict with `type`, `exclusive`,
/// and (for `getregionpos`) `eol`.
fn parse_region_opts(args: &[Typval]) -> ox_eval::Result<RegionOpts> {
    let mut opts = RegionOpts::default();
    let Some(dict) = args.get(2) else {
        return Ok(opts);
    };
    let Typval::Dict(reference) = dict else {
        return Err(EvalError::new(
            "E1206",
            0,
            "Dictionary required for argument 3",
        ));
    };
    let dict = reference
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    if let Some(value) = dict.get(b"exclusive") {
        opts.exclusive = value.is_truthy();
    }
    if let Some(value) = dict.get(b"eol") {
        opts.allow_eol = value.is_truthy();
    }
    if let Some(Typval::String(text)) = dict.get(b"type") {
        let bytes = text.as_bytes();
        match bytes.first() {
            Some(b'v') if bytes.len() == 1 => opts.region_type = RegionType::Char,
            Some(b'V') if bytes.len() == 1 => opts.region_type = RegionType::Line,
            Some(&0x16) => {
                opts.region_type = RegionType::Block;
                if bytes.len() > 1 {
                    let rest = &bytes[1..];
                    let width = std::str::from_utf8(rest)
                        .ok()
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(0);
                    if width > 0 {
                        opts.block_width = width;
                    }
                }
            }
            _ => {
                return Err(EvalError::new("E474", 0, "Invalid argument: type"));
            }
        }
    }
    Ok(opts)
}

/// Extracts `[bufnum, lnum, col, off]` from a 4-element position list,
/// mirroring `funcs.c:list2fpos` with `with_fnum = true`. Returns the
/// buffer number, a one-based line, a one-based column, and coladd.
fn parse_pos_list(value: &Typval) -> ox_eval::Result<Option<(i64, i64, i64, i64)>> {
    let Typval::List(reference) = value else {
        return Ok(None);
    };
    let list = reference
        .try_borrow()
        .map_err(|_| EvalError::new("E742", 0, "Cannot change value"))?;
    let items = &list.items;
    // Upstream `list2fpos` takes `[fnum, lnum, col, off]` plus an ignored
    // fifth slot when curswant is NULL (`funcs.c:2070` passes NULL).
    if items.len() < 3 || items.len() > 5 {
        return Ok(None);
    }
    let bufnum = list_nr(items, 0)?.unwrap_or(0);
    let lnum = list_nr(items, 1)?.unwrap_or(0);
    let col = list_nr(items, 2)?.unwrap_or(0);
    let off = list_nr(items, 3)?.unwrap_or(0);
    Ok(Some((bufnum, lnum, col, off)))
}

/// `funcs.c:f_getregion`: returns the list of strings from `pos1` to `pos2`
/// in the buffer named by the positions. The positions are `[bufnum, lnum,
/// col, off]` lists as produced by `getpos()`.
///
/// Upstream normalizes the two positions so `p1` is upper-left, adjusts
/// columns from 1-based to 0-based, then iterates lines extracting the
/// selected text. Charwise selections take a byte slice of each line;
/// linewise selections take the whole line; blockwise selections take a
/// column range across each line.
fn get_region(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let Some((p1, p2, _buf, lines)) = resolve_region(editor, args)? else {
        return Ok(Typval::list(Vec::new()));
    };
    let opts = parse_region_opts(args)?;
    let tab_width = tabstop(editor);
    let (block_start, block_end) = if opts.region_type == RegionType::Block {
        block_columns(
            &lines,
            &p1,
            &p2,
            opts.block_width,
            opts.exclusive,
            tab_width,
        )
    } else {
        (0, 0)
    };
    let mut result = Vec::new();
    for lnum in p1.lnum..=p2.lnum {
        let line_idx = usize::try_from(lnum)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| lines.get(n));
        let text: Vec<u8> = match (opts.region_type, line_idx) {
            (RegionType::Line, _) => line_idx.map_or(Vec::new(), Clone::clone),
            (RegionType::Char | RegionType::Block, None) => Vec::new(),
            (RegionType::Char, Some(line)) => {
                // Upstream f_getregion: first line starts at p1.col, last
                // line ends at p2.col, middle lines are taken whole.
                let start = if lnum == p1.lnum {
                    usize::try_from(p1.col.max(0)).unwrap_or(0)
                } else {
                    0
                };
                let mut end = if lnum == p2.lnum {
                    usize::try_from(p2.col.max(0)).unwrap_or(0)
                } else {
                    line.len()
                };
                if !opts.exclusive && lnum == p2.lnum {
                    end = end.saturating_add(1);
                }
                let start = start.min(line.len());
                let end = end.min(line.len()).max(start);
                line[start..end].to_vec()
            }
            (RegionType::Block, Some(line)) => {
                let block = block_prep(line, block_start, block_end, tab_width);
                block_text(line, &block)
            }
        };
        result.push(Typval::String(OxStr(text)));
    }
    Ok(Typval::list(result))
}

/// `funcs.c:f_getregionpos`: returns a list of `[[start_pos, end_pos], ...]`
/// pairs, one per line in the region. Each position is `[bufnum, lnum, col,
/// off]` with 1-based `col`.
fn position_column(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[expect(clippy::too_many_lines, reason = "position conversion mirrors funcs.c")]
fn get_region_pos(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let Some((p1, p2, buf, lines)) = resolve_region(editor, args)? else {
        return Ok(Typval::list(Vec::new()));
    };
    let opts = parse_region_opts(args)?;
    let tab_width = tabstop(editor);
    let (block_start, block_end) = if opts.region_type == RegionType::Block {
        block_columns(
            &lines,
            &p1,
            &p2,
            opts.block_width,
            opts.exclusive,
            tab_width,
        )
    } else {
        (0, 0)
    };
    let bufnum = i64::from(buf);
    let mut result = Vec::new();
    for lnum in p1.lnum..=p2.lnum {
        let line_idx = usize::try_from(lnum)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| lines.get(n));
        let line_len = line_idx.map_or(0, Vec::len);
        let line_cap = i64::try_from(line_len).unwrap_or(i64::MAX);
        let (start_col, end_col, start_coladd, end_coladd) = match opts.region_type {
            RegionType::Line => (1, MAXCOL, 0, 0),
            RegionType::Char => {
                // Per-line: first line starts at p1.col+1, last line ends
                // at p2.col+1 (inclusive) or p2.col+1 (exclusive), middle
                // lines span the whole line.
                let s = if lnum == p1.lnum {
                    p1.col.max(0) + 1
                } else {
                    1
                };
                let e = if lnum == p2.lnum {
                    p2.col + 1
                } else {
                    line_cap + 1
                };
                (s, e, 0, 0)
            }
            RegionType::Block => {
                let mut block = line_idx.map_or_else(
                    || block_prep(&[], block_start, block_end, tab_width),
                    |line| block_prep(line, block_start, block_end, tab_width),
                );
                let line = line_idx.map_or(&[][..], |line| line.as_slice());
                let (mut start, mut start_add) = if block.is_one_char {
                    (
                        position_column(previous_char_start(line, block.textcol)) + 1,
                        position_column(block.start_char_vcols)
                            - (position_column(block.start_vcol) - position_column(block_start)),
                    )
                } else if block.start_vcol < block_start {
                    block.is_one_char = true;
                    (
                        MAXCOL,
                        position_column(block_start) - position_column(block.start_vcol),
                    )
                } else if block.startspaces > 0 {
                    (
                        position_column(previous_char_start(line, block.textcol)) + 1,
                        position_column(block.start_char_vcols)
                            - position_column(block.startspaces),
                    )
                } else {
                    (position_column(block.textcol) + 1, 0)
                };
                let (mut end, mut end_add) = if block.is_one_char {
                    (
                        start,
                        start_add
                            + position_column(block.startspaces)
                            + position_column(block.endspaces),
                    )
                } else if block.endspaces > 0 {
                    (
                        position_column(block.textcol) + position_column(block.textlen) + 1,
                        position_column(block.endspaces),
                    )
                } else {
                    (
                        position_column(block.textcol) + position_column(block.textlen),
                        0,
                    )
                };
                if !opts.allow_eol && start > line_cap {
                    start = 0;
                    start_add = 0;
                }
                if !opts.allow_eol && end > line_cap {
                    end = if start == 0 { 0 } else { line_cap };
                    end_add = 0;
                }
                (start, end, start_add, end_add)
            }
        };
        let (start_col, end_col, start_coladd, end_coladd) = if opts.allow_eol {
            (start_col, end_col, start_coladd, end_coladd)
        } else {
            let s = if start_col > line_cap + 1 {
                line_cap + 1
            } else {
                start_col
            };
            let e = if end_col > line_cap {
                if s == 0 { 0 } else { line_cap }
            } else {
                end_col
            };
            let start_add = if s == 0 { 0 } else { start_coladd };
            let end_add = if e == 0 || e == line_cap {
                0
            } else {
                end_coladd
            };
            (s, e, start_add, end_add)
        };
        let start_pos = Typval::list(vec![
            Typval::Number(bufnum),
            Typval::Number(lnum),
            Typval::Number(start_col),
            Typval::Number(start_coladd),
        ]);
        let end_pos = Typval::list(vec![
            Typval::Number(bufnum),
            Typval::Number(lnum),
            Typval::Number(end_col),
            Typval::Number(end_coladd),
        ]);
        result.push(Typval::list(vec![start_pos, end_pos]));
    }
    Ok(Typval::list(result))
}

fn block_columns(
    lines: &[Vec<u8>],
    p1: &FPos,
    p2: &FPos,
    width: i64,
    exclusive: bool,
    tab_width: usize,
) -> (usize, usize) {
    let first = lines
        .get(usize::try_from(p1.lnum.saturating_sub(1)).unwrap_or(0))
        .map_or((0, 0), |line| getvvcol(line, p1.col, p1.coladd, tab_width));
    let second = lines
        .get(usize::try_from(p2.lnum.saturating_sub(1)).unwrap_or(0))
        .map_or(first, |line| getvvcol(line, p2.col, p2.coladd, tab_width));
    let start = first.0.min(second.0);
    let mut end = first.1.max(second.1);
    if width > 0 {
        end = start.saturating_add(usize::try_from(width - 1).unwrap_or(0));
    } else if exclusive && first.1 < second.0 && second.0 > 0 && second.1 > first.1 {
        end = second.0 - 1;
    }
    (start, end)
}

#[derive(Clone, Copy, Debug, Default)]
struct BlockDef {
    start_vcol: usize,
    end_vcol: usize,
    start_char_vcols: usize,
    end_char_vcols: usize,
    startspaces: usize,
    endspaces: usize,
    textcol: usize,
    textlen: usize,
    is_short: bool,
    is_one_char: bool,
}

fn block_prep(line: &[u8], start_vcol: usize, end_vcol: usize, tab_width: usize) -> BlockDef {
    let mut block = BlockDef {
        start_vcol,
        end_vcol: 0,
        ..BlockDef::default()
    };
    let mut index = 0;
    let mut vcol = 0;
    let mut incr = 0;
    while vcol < start_vcol && index < line.len() {
        let (character, _) = decode_char(&line[index..]);
        let length = cluster_len(line, index);
        incr = cell_width(character, vcol, tab_width);
        vcol += incr;
        index += length;
    }
    block.start_vcol = vcol;
    block.start_char_vcols = incr;
    let pstart = index;
    if block.start_vcol < start_vcol {
        block.end_vcol = block.start_vcol;
        block.is_short = true;
        block.endspaces = end_vcol - start_vcol + 1;
    } else {
        block.startspaces = block.start_vcol - start_vcol;
        let mut pend = pstart;
        block.end_vcol = block.start_vcol;
        if block.end_vcol > end_vcol {
            block.is_one_char = true;
            block.startspaces = end_vcol - start_vcol + 1;
        } else {
            let mut previous_end = pend;
            while block.end_vcol <= end_vcol && index < line.len() {
                previous_end = index;
                let (character, _) = decode_char(&line[index..]);
                let length = cluster_len(line, index);
                incr = cell_width(character, block.end_vcol, tab_width);
                block.end_vcol += incr;
                index += length;
                pend = index;
            }
            if block.end_vcol <= end_vcol {
                block.is_short = true;
            } else {
                let mut spaces = block.end_vcol - end_vcol - 1;
                if spaces != 0 {
                    spaces = incr - spaces;
                    if pend != pstart {
                        pend = previous_end;
                    }
                }
                block.endspaces = spaces;
            }
        }
        block.end_char_vcols = incr;
        block.textlen = pend - pstart;
    }
    block.textcol = pstart;
    block
}

fn block_text(line: &[u8], block: &BlockDef) -> Vec<u8> {
    let mut output = Vec::with_capacity(block.startspaces + block.textlen + block.endspaces);
    output.extend(std::iter::repeat_n(b' ', block.startspaces));
    output.extend_from_slice(&line[block.textcol..block.textcol + block.textlen]);
    output.extend(std::iter::repeat_n(b' ', block.endspaces));
    output
}

fn previous_char_start(line: &[u8], index: usize) -> usize {
    let mut previous = 0;
    let mut current = 0;
    while current < index {
        previous = current;
        current += cluster_len(line, current);
    }
    previous
}

/// A resolved `getregion`/`getregionpos` span: normalized endpoints, the
/// buffer they live in, and that buffer's lines.
type RegionSpan = (FPos, FPos, BufHandle, Vec<Vec<u8>>);

/// Shared position parsing for `getregion`/`getregionpos`: extracts two
/// `[bufnum, lnum, col, off]` lists, normalizes so `p1` is upper-left, and
/// loads the buffer's lines. A `bufnum` of 0 names the current buffer, as
/// `list2fpos` resolves it. Positions in different buffers yield no region
/// (`Ok(None)`): upstream allocates the empty return list first, so the
/// `fnum1 != fnum2` FAIL answers `[]`, not an error.
fn resolve_region(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Option<RegionSpan>> {
    let (buf1, lnum1, col1, off1) = parse_pos_list(&args[0])?
        .ok_or_else(|| EvalError::new("E1211", 0, "List required for argument 1"))?;
    let (buf2, lnum2, col2, off2) = parse_pos_list(&args[1])?
        .ok_or_else(|| EvalError::new("E1211", 0, "List required for argument 2"))?;
    let window = editor
        .current_window()
        .ok_or_else(|| EvalError::new("E957", 0, "no window"))?;
    let win = PosWin::new(editor, window)?;
    let current = i64::from(win.buffer);
    let fnum1 = if buf1 == 0 { current } else { buf1 };
    let fnum2 = if buf2 == 0 { current } else { buf2 };
    if fnum1 != fnum2 {
        return Ok(None);
    }
    let buffer = if buf1 != 0 {
        buflist_findnr(editor, buf1).ok_or_else(|| EvalError::new("E121", 0, "buffer not found"))?
    } else if buf2 != 0 {
        buflist_findnr(editor, buf2).ok_or_else(|| EvalError::new("E121", 0, "buffer not found"))?
    } else {
        win.buffer
    };
    let lines = buffer_lines(editor, buffer).map_err(|error| EvalError::new("E16", 0, error))?;
    let line_count = vim_number(lines.len());
    if lnum1 < 1 || lnum1 > line_count {
        return Err(EvalError::new(
            "E966",
            0,
            format!("Invalid line number: {lnum1}"),
        ));
    }
    if lnum2 < 1 || lnum2 > line_count {
        return Err(EvalError::new(
            "E966",
            0,
            format!("Invalid line number: {lnum2}"),
        ));
    }
    // Convert to 0-based columns (upstream adjusts after validation).
    let mut p1 = FPos {
        lnum: lnum1,
        col: col1.saturating_sub(1).max(0),
        coladd: off1,
    };
    let mut p2 = FPos {
        lnum: lnum2,
        col: col2.saturating_sub(1).max(0),
        coladd: off2,
    };
    // Normalize: swap so p1 is upper-left.
    if p1.lnum > p2.lnum || (p1.lnum == p2.lnum && p1.col > p2.col) {
        std::mem::swap(&mut p1, &mut p2);
    }
    Ok(Some((p1, p2, buffer, lines)))
}

/// `move.c:update_curswant` without its side effect: a window that has asked
/// for a refresh answers the cursor's virtual column, and any other window
/// answers the column it was last told to want. Upstream only refreshes the
/// current window, so a `getcurpos(winid)` on a background window reads
/// `w_curswant` raw.
fn effective_curswant(editor: &Editor, win: &PosWin) -> i64 {
    let state = editor.window(win.window);
    let (curswant, set_curswant) =
        state.map_or((0, false), |state| (state.curswant, state.set_curswant));
    if !set_curswant || editor.current_window() != Some(win.window) {
        return curswant;
    }
    let line = win.line(vim_number(win.cursor.lnum));
    vim_number(cursor_vcol(line, win.cursor.col, tabstop(editor)))
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
            vim_number(win.line(pos.lnum).len()).saturating_add(1)
        } else {
            MAXCOL
        }
    } else {
        pos.col.saturating_add(1)
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

/// `line2byte()` (`funcs.c:f_line2byte`). Returns the byte count from the
/// start of the buffer for line `lnum`, including the end-of-line character.
/// The first line returns 1. Uses `ml_find_line_or_offset` with `no_ff=false`
/// (upstream `eval.lua:6691`), so `'fileformat'` diverges from
/// `nvim_buf_get_offset`. An empty buffer (memline-empty: one line, zero
/// bytes) returns -1 for any lnum, matching the `ml_usedchunks == -1` branch
/// (`memline.c:4042-4045`).
fn call_line2byte(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let window = match args.get(1) {
        Some(value) => resolve_window_id(editor, value)?,
        None => editor.current_window(),
    };
    let Some(window) = window else {
        return Ok(Typval::Number(-1));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = 0;
    let Some(pos) = var2fpos(editor, &args[0], true, false, &win, &mut fnum)? else {
        return Ok(Typval::Number(-1));
    };
    let lnum = pos.lnum;
    let line_count = win.line_count();
    // Empty memline: one line, zero content. Upstream returns -1 for any
    // lnum because `ml_usedchunks == -1` and `no_ff` is false (the
    // `lnum == 1 || lnum == 2` quirk only applies with `no_ff = true`).
    if line_count <= 1 && win.line(1).is_empty() {
        return Ok(Typval::Number(-1));
    }
    if lnum < 1 || lnum > line_count + 1 {
        return Ok(Typval::Number(-1));
    }
    let state = editor
        .buffer(win.buffer)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let text = state
        .text()
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    let offset = text
        .byte_of_line(usize::try_from(lnum).unwrap_or(0))
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    // line2byte is 1-based: the first line returns 1, not 0.
    Ok(Typval::Number(vim_number(offset) + 1))
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
    let col = pos.col.clamp(0, vim_number(line.len()));
    let (start, end) = getvcol(line, col, tabstop(editor));
    let (start, end) = wrap_showbreak(editor, window, start + 1, end + 1);
    Ok(pair(vim_number(start), vim_number(end)))
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// `cursor()` and `setcursorcharpos()` (`funcs.c:set_cursorpos`).
fn set_cursorpos(editor: &mut Editor, args: &[Typval], charcol: bool) -> ox_eval::Result<Typval> {
    let Some(window) = editor.current_window() else {
        return Ok(Typval::Number(-1));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = i64::from(win.buffer);
    let mut curswant = -1;
    let mut set_curswant = true;
    let (mut lnum, mut col, coladd);
    if matches!(args[0], Typval::List(_)) {
        let Some(pos) = list2fpos(
            editor,
            &args[0],
            false,
            charcol,
            &win,
            &mut fnum,
            &mut curswant,
        )?
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
            lnum = vim_number(win.cursor.lnum);
        }
        col = number_value(&args[1])?;
        if charcol {
            col = charidx_to_byteidx(win.line(lnum), col).saturating_add(1);
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
        lnum: if lnum > 0 {
            lnum
        } else {
            vim_number(win.cursor.lnum)
        },
        col: if col == MAXCOL { col } else { (col - 1).max(0) },
        coladd,
    };
    check_cursor(&win, &mut pos);
    place_cursor(editor, window, pos, curswant, Some(set_curswant))?;
    Ok(Typval::Number(0))
}

/// `setpos()` and `setcharpos()` (`funcs.c:set_position`).
fn set_position(editor: &mut Editor, args: &[Typval], charcol: bool) -> ox_eval::Result<Typval> {
    let name = input_string_arg(&args[0])?;
    let Some(window) = editor.current_window() else {
        return Ok(Typval::Number(-1));
    };
    let win = PosWin::new(editor, window)?;
    let mut fnum = i64::from(win.buffer);
    let mut curswant = -1;
    let Some(mut pos) = list2fpos(
        editor,
        &args[1],
        true,
        charcol,
        &win,
        &mut fnum,
        &mut curswant,
    )?
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

/// `mark.c:setmark_pos` for local, operator, previous-context, global, and
/// numbered marks.
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
    } else {
        // Lowercase and editor-maintained special marks live in the buffer
        // that `fnum` names.
        editor.set_local_mark(buffer, name, position).is_ok()
    };
    if stored { 0 } else { -1 }
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
    let len = vim_number(line.len());
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
    let Ok(col) = usize::try_from(pos.col) else {
        pos.col = 0;
        return;
    };
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
    pos.col = vim_number(index);
}

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

/// `plines.c:getvcol`: the zero-based first and last virtual column of the
/// character at byte `col`, with the NUL past the end of the line counting as
/// one cell.
pub(crate) fn getvcol(line: &[u8], col: i64, tabstop: usize) -> (usize, usize) {
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

fn getvvcol(line: &[u8], col: i64, coladd: i64, tabstop: usize) -> (usize, usize) {
    let (start, end) = getvcol(line, col, tabstop);
    let offset = usize::try_from(coladd).unwrap_or(0);
    let target = usize::try_from(col).unwrap_or(0);
    if target < line.len() {
        let (character, _) = decode_char(&line[target..]);
        let width = cell_width(character, start, tabstop);
        if character != '\t' && width > 1 && offset < width {
            return (start, end);
        }
        if character == '\t' {
            let column = start.saturating_add(offset);
            return (column, column);
        }
    }
    (start.saturating_add(offset), end.saturating_add(offset))
}

/// Returns the cursor cell selected by Normal mode.
///
/// A tab selects its final display cell; every other character selects its
/// first cell. Keep this rule shared by cursor-state and screen-position reads.
pub(crate) fn cursor_vcol(line: &[u8], col: usize, tabstop: usize) -> usize {
    let (start, end) = getvcol(line, vim_number(col), tabstop);
    if line.get(col) == Some(&b'\t') {
        end
    } else {
        start
    }
}

/// Display cells one character cluster occupies
/// (`plines.c:charsize_fast_impl`), upstream's `win_chartabsize`.
pub(crate) fn cell_width(character: char, vcol: usize, tabstop: usize) -> usize {
    match character {
        '\t' => tabstop - (vcol % tabstop),
        control if (control as u32) < 0x20 || control as u32 == 0x7f => 2,
        _ => UnicodeWidthChar::width(character).unwrap_or(1).max(1),
    }
}

/// Adds the `'showbreak'` cells that precede every continuation row a long
/// line wraps onto.
fn wrap_showbreak(editor: &Editor, window: WinHandle, start: usize, end: usize) -> (usize, usize) {
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
pub(crate) fn tabstop(editor: &Editor) -> usize {
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
        if let Ok(text) = std::str::from_utf8(&bytes[..width])
            && let Some(character) = text.chars().next()
        {
            return (character, width);
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
        remaining = remaining.saturating_sub(1);
        if index >= line.len() || remaining <= 0 {
            return vim_number(index);
        }
        index += cluster_len(line, index);
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// `typval.c:tv_get_number_chk`: the numeric value of a typval, or the error
/// upstream raises for a type that has none.
pub(super) fn number_value(value: &Typval) -> ox_eval::Result<i64> {
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
        Typval::Funcref(_) | Typval::Partial(_) => {
            Err(EvalError::new("E703", 0, "Using a Funcref as a Number"))
        }
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

/// `typval.c:tv_get_lnum` resolved against the current window, for the
/// builtins outside this module that take a `{lnum}` argument. Answers `0`,
/// which every caller treats as out of range, when there is no window whose
/// cursor and marks the expression forms could name.
pub(super) fn current_lnum_arg(editor: &Editor, value: &Typval) -> ox_eval::Result<i64> {
    let Some(window) = editor.current_window() else {
        return Ok(0);
    };
    let win = PosWin::new(editor, window)?;
    get_lnum(editor, value, &win)
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
fn resolve_window_nr_or_id(editor: &Editor, value: &Typval) -> ox_eval::Result<Option<WinHandle>> {
    if number_value(value)? == 0 {
        return Ok(editor.current_window());
    }
    resolve_window_id(editor, value)
}
