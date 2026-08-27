#![allow(clippy::unwrap_used)]

//! Behavioral tests for the position builtins: `cursor`, `setcursorcharpos`,
//! `getpos`, `getcharpos`, `getcurpos`, `getcursorcharpos`, `setpos`,
//! `setcharpos`, `col`, `charcol`, `line` and `virtcol`.
//!
//! Citations (READ-ONLY spec under `.references/neovim/`):
//! * `src/nvim/eval.c` — `var2fpos` expression forms (`.`, `v`, `'x`, `w0`,
//!   `w$`, `$`, `[lnum, col, off]`), `list2fpos`
//!   (`[bufnum, lnum, col, off, curswant]`, `buflist_findnr` for a character
//!   column), `buf_byteidx_to_charidx`, `buf_charidx_to_byteidx`.
//! * `src/nvim/eval/funcs.c` — `getpos_both`, `get_col`, `f_line`,
//!   `f_virtcol`, `set_cursorpos`, `set_position`.
//! * `src/nvim/mark.c` — `setmark_pos`: the previous-context marks answer
//!   before the buffer is looked up, a nonexistent `fnum` fails, and a
//!   lowercase mark is written into the buffer `fnum` names.
//! * `src/nvim/move.c` — `update_curswant`: `w_curswant` is refreshed from the
//!   cursor's virtual column only while `w_set_curswant` is set.
//! * `src/nvim/plines.c` — `getvcol`: tab expansion and the Normal-mode cursor
//!   sitting on a tab's last cell.
//! * `test/old/testdir/test_cursor_func.vim`, `test_marks.vim`,
//!   `test_functions.vim` — the observable shapes asserted here.

use ox_eval::ScopeKind;
use ox_text::{Buffer, Position};
use ox_types::{BufHandle, Typval, WinHandle};

use crate::excmd_exec::ExecError;
use crate::{BufferRelease, Editor, ExExecutor, Geometry, VimExceptionKind};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// An editor with one listed buffer shown in one 80x24 window.
fn editor_with_window() -> (Editor, BufHandle, WinHandle) {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    (editor, buffer, window)
}

/// Adds a second listed buffer holding `lines`, leaving the current one
/// unchanged. Stands in for `:enew` followed by `:buffer #`, which the Ex
/// executor does not serve yet.
fn add_buffer(editor: &mut Editor, lines: &[&str]) -> BufHandle {
    let owned = lines
        .iter()
        .map(|line| line.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let text = Buffer::from_lines(&owned, true).unwrap();
    editor.create_buffer_with(text, true).unwrap()
}

/// Runs `script` and returns the executor so its globals can be read.
fn run(editor: &mut Editor, script: &str) -> ExExecutor {
    let mut exec = ExExecutor::new();
    exec.execute_script(editor, "position.vim", script).unwrap();
    exec
}

/// The value of a global set by the script.
fn global(exec: &ExExecutor, name: &str) -> Typval {
    exec.scope()
        .get_scoped(ScopeKind::Global, name.as_bytes(), 0)
        .unwrap()
        .clone()
}

/// The numeric value of a global.
fn number(exec: &ExExecutor, name: &str) -> i64 {
    match global(exec, name) {
        Typval::Number(value) => value,
        other => panic!("expected a Number global, got {other:?}"),
    }
}

/// The numbers in a list-valued global.
fn numbers(exec: &ExExecutor, name: &str) -> Vec<i64> {
    match global(exec, name) {
        Typval::List(reference) => reference
            .borrow()
            .items
            .iter()
            .map(|item| match item {
                Typval::Number(value) => *value,
                other => panic!("expected a Number item, got {other:?}"),
            })
            .collect(),
        other => panic!("expected a List global, got {other:?}"),
    }
}

/// The Vim error code a script raises.
fn error_code(editor: &mut Editor, script: &str) -> String {
    let mut exec = ExExecutor::new();
    match exec.execute_script(editor, "position.vim", script) {
        Err(ExecError::Vim(exception)) => match exception.kind {
            VimExceptionKind::Error(code) => code,
            other => panic!("expected an error exception, got {other:?}"),
        },
        other => panic!("expected a Vim error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// getcurpos and the wanted column (move.c:update_curswant)
// ---------------------------------------------------------------------------

// funcs.c:getpos_both, move.c:update_curswant — `getcurpos()` answers
// `[0, lnum, col, coladd, curswant + 1]`, and the wanted column is refreshed
// from the cursor's virtual column while `w_set_curswant` is set. Oracle
// (nvim, 'ts'=8, line "the\tquick"): [0, 1, 5, 0, 9].
#[test]
fn getcurpos_refreshes_the_wanted_column_from_the_virtual_column() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, [\"the\tquick\", \"second line\"])\n\
         call cursor(1, 1)\n\
         call setpos('.', [0, 1, 5, 0])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 5, 0, 9]);
}

// funcs.c:set_position — a five-element list pins `w_curswant` to
// `curswant - 1` and clears `w_set_curswant`, so `getcurpos()` answers the
// wanted column the caller asked for. Oracle: [0, 1, 5, 0, 3].
#[test]
fn setpos_five_element_list_pins_the_wanted_column() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, [\"the\tquick\", \"second line\"])\n\
         call cursor(1, 1)\n\
         call setpos('.', [0, 1, 5, 0, 3])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 5, 0, 3]);
}

// funcs.c:set_position — the four-element `'.'` form never touches
// `w_set_curswant`; only a list carrying a wanted column clears it. Writing
// `false` there would freeze a stale column into `getcurpos()[4]`. Oracle
// (lines ['alpha', "\tabc", '日本語x']): [0, 2, 3, 0, 10].
#[test]
fn setpos_four_element_list_leaves_the_wanted_column_live() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['alpha', \"\tabc\", '日本語x'])\n\
         call cursor(2, 1)\n\
         call setpos('.', [0, 2, 3, 0])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 2, 3, 0, 10]);
    assert!(editor.window(window).unwrap().set_curswant);
}

// funcs.c:set_cursorpos — `cursor()` always writes `w_set_curswant`, and the
// list form clears it when the list carries a wanted column.
#[test]
fn cursor_list_form_with_a_wanted_column_clears_the_refresh_flag() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, [\"the\tquick\"])\n\
         call cursor([1, 5, 0, 3])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 5, 0, 3]);
    assert!(!editor.window(window).unwrap().set_curswant);
}

// funcs.c:set_cursorpos — the two-argument form leaves `w_set_curswant` set,
// so the next read recomputes the wanted column.
#[test]
fn cursor_line_column_form_leaves_the_refresh_flag_set() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, [\"the\tquick\"])\n\
         call cursor(1, 5)\n\
         let g:pos = getcurpos()",
    );
    // Byte column 4 is `q`, whose only cell is virtual column 8.
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 5, 0, 9]);
    assert!(editor.window(window).unwrap().set_curswant);
}

// plines.c:getvcol — in Normal mode without 'list' the cursor sits on a tab's
// last cell, so a cursor inside the tab of "the\tquick" wants virtual column
// 7 (zero-based) and `getcurpos()` answers 8.
#[test]
fn getcurpos_puts_the_cursor_on_the_last_cell_of_a_tab() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, \"the\tquick\")\n\
         call setpos('.', [0, 1, 4, 0])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 4, 0, 8]);
}

// funcs.c:getpos_both — `getcurpos(0)` names the current window, and
// `getcurpos()` with no argument answers the same list.
#[test]
fn getcurpos_zero_names_the_current_window() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\n\
         call cursor(1, 3)\n\
         let g:bare = getcurpos()\n\
         let g:zero = getcurpos(0)",
    );
    assert_eq!(numbers(&exec, "bare"), numbers(&exec, "zero"));
    assert_eq!(numbers(&exec, "zero"), vec![0, 1, 3, 0, 3]);
}

// funcs.c:getpos_both — an unknown window id answers all zeros rather than an
// error, and carries the fifth element `getcurpos()` always has.
#[test]
fn getcurpos_on_an_unknown_window_answers_zeros() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\n\
         let g:pos = getcurpos(9999)",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 0, 0, 0, 0]);
}

// move.c:update_curswant — upstream refreshes only the current window, so a
// background window answers `w_curswant` raw. A window that was told to want
// column 3 keeps answering 3 after the cursor moves elsewhere.
#[test]
fn getcurpos_on_a_background_window_reads_the_wanted_column_raw() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    let other = editor.split_horizontal(tab, window, buffer).unwrap();
    editor.set_current_window(window).unwrap();
    {
        let state = editor.window_mut(other).unwrap();
        state.cursor = Position { lnum: 1, col: 0 };
        state.curswant = 2;
        state.set_curswant = true;
    }
    let exec = run(
        &mut editor,
        &format!(
            "call setline(1, [\"the\tquick\"])\nlet g:pos = getcurpos({})",
            i64::from(other)
        ),
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 1, 0, 3]);
}

// ---------------------------------------------------------------------------
// setpos and setcharpos marks (mark.c:setmark_pos)
// ---------------------------------------------------------------------------

// mark.c:setmark_pos — a mark cannot be set in a buffer that does not exist:
// `buflist_findnr` fails first, so the call answers -1 and stores nothing.
// Oracle: setpos("'A", [9999, 1, 1, 0]) == -1, getpos("'A") == [0, 0, 0, 0].
#[test]
fn setpos_global_mark_in_a_nonexistent_buffer_fails_and_stores_nothing() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['aaaaa'])\n\
         let g:rc = setpos(\"'A\", [9999, 1, 1, 0])\n\
         let g:mark = getpos(\"'A\")",
    );
    assert_eq!(number(&exec, "rc"), -1);
    assert_eq!(numbers(&exec, "mark"), vec![0, 0, 0, 0]);
}

// mark.c:setmark_pos — the `buflist_findnr` guard precedes the lowercase
// branch too, so a buffer-local mark in a nonexistent buffer also fails.
#[test]
fn setpos_local_mark_in_a_nonexistent_buffer_fails_and_stores_nothing() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['aaaaa'])\n\
         let g:rc = setpos(\"'a\", [9999, 1, 1, 0])\n\
         let g:mark = getpos(\"'a\")",
    );
    assert_eq!(number(&exec, "rc"), -1);
    assert_eq!(numbers(&exec, "mark"), vec![0, 0, 0, 0]);
}

// mark.c:setmark_pos — a lowercase mark is written into `b_namedm` of the
// buffer `fnum` names, not the current one. Oracle: the mark reads
// [0, 1, 3, 0] in the named buffer and [0, 0, 0, 0] in the current one.
#[test]
fn setpos_lowercase_mark_lands_in_the_buffer_fnum_names() {
    let (mut editor, first, _window) = editor_with_window();
    let second = add_buffer(&mut editor, &["bbbbbbb"]);
    let exec = run(
        &mut editor,
        &format!(
            "call setline(1, ['aaaaa'])\n\
             let g:rc = setpos(\"'a\", [{}, 1, 3, 0])\n\
             let g:here = getpos(\"'a\")",
            i64::from(second)
        ),
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(numbers(&exec, "here"), vec![0, 0, 0, 0]);
    assert_eq!(editor.local_mark(first, 'a').unwrap(), None);
    assert_eq!(
        editor.local_mark(second, 'a').unwrap(),
        Some(Position { lnum: 1, col: 2 })
    );

    // Reading it from the buffer that owns it answers the stored position.
    editor
        .set_current_buffer(second, BufferRelease::KeepLoaded)
        .unwrap();
    let exec = run(&mut editor, "let g:there = getpos(\"'a\")");
    assert_eq!(numbers(&exec, "there"), vec![0, 1, 3, 0]);
}

// mark.c:setmark_pos — a global mark records the buffer it was set in, and
// `getpos` reports that buffer as the first element.
#[test]
fn setpos_global_mark_records_the_buffer_it_names() {
    let (mut editor, _first, _window) = editor_with_window();
    let second = add_buffer(&mut editor, &["bbbbbbb"]);
    let exec = run(
        &mut editor,
        &format!(
            "call setline(1, ['aaaaa'])\n\
             let g:rc = setpos(\"'A\", [{buffer}, 1, 4, 0])\n\
             let g:mark = getpos(\"'A\")",
            buffer = i64::from(second)
        ),
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(numbers(&exec, "mark"), vec![i64::from(second), 1, 4, 0]);
}

// eval.c:list2fpos — `fnum` 0 names the current buffer, so a global mark set
// with a leading zero reports the current buffer back.
#[test]
fn setpos_global_mark_with_zero_fnum_uses_the_current_buffer() {
    let (mut editor, first, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['aaaaa'])\n\
         call setpos(\"'A\", [0, 1, 2, 0])\n\
         let g:mark = getpos(\"'A\")",
    );
    assert_eq!(numbers(&exec, "mark"), vec![i64::from(first), 1, 2, 0]);
}

// mark.c:setmark_pos — the previous-context marks answer before the buffer is
// looked up, so a nonexistent `fnum` never fails them.
#[test]
fn setpos_previous_context_mark_ignores_a_nonexistent_fnum() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['aaaaa'])\n\
         let g:rc = setpos(\"''\", [9999, 1, 3, 0])\n\
         let g:mark = getpos(\"''\")",
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(numbers(&exec, "mark"), vec![0, 1, 3, 0]);
}

// funcs.c:set_position — a mark name ox-editor does not model answers the
// failure upstream answers for a name it cannot set.
#[test]
fn setpos_unmodelled_mark_name_fails() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['aaaaa'])\n\
         let g:rc = setpos(\"'<\", [0, 1, 1, 0])",
    );
    assert_eq!(number(&exec, "rc"), -1);
}

// funcs.c:set_position — a name that is neither `.` nor a two-character mark
// raises E474.
#[test]
fn setpos_unknown_name_raises_e474() {
    let (mut editor, _buffer, _window) = editor_with_window();
    assert_eq!(
        error_code(
            &mut editor,
            "call setline(1, ['aaaaa'])\ncall setpos('zz', [0, 1, 1, 0])"
        ),
        "E474"
    );
}

// eval.c:list2fpos — a character column is converted against the line of the
// buffer `fnum` names. Oracle (buffer 2 holding '日本語x'):
// setcharpos("'A", [2, 1, 3, 0]) leaves getpos("'A") == [2, 1, 7, 0], the
// byte column of the third character.
#[test]
fn setcharpos_converts_the_character_index_against_the_named_buffer() {
    let (mut editor, _first, _window) = editor_with_window();
    let second = add_buffer(&mut editor, &["日本語x"]);
    let exec = run(
        &mut editor,
        &format!(
            "call setline(1, ['abcdef'])\n\
             call cursor(1, 1)\n\
             let g:rc = setcharpos(\"'A\", [{buffer}, 1, 3, 0])\n\
             let g:mark = getpos(\"'A\")",
            buffer = i64::from(second)
        ),
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(numbers(&exec, "mark"), vec![i64::from(second), 1, 7, 0]);
}

// eval.c:list2fpos — reading the current buffer's line instead would land on
// byte column 3, because 'abcdef' is single-byte. The named buffer's
// multibyte line is what decides the column.
#[test]
fn setcharpos_does_not_use_the_current_buffer_line() {
    let (mut editor, _first, _window) = editor_with_window();
    let second = add_buffer(&mut editor, &["日本語x"]);
    let exec = run(
        &mut editor,
        &format!(
            "call setline(1, ['abcdef'])\n\
             call setcharpos(\"'A\", [{buffer}, 1, 3, 0])\n\
             let g:mark = getpos(\"'A\")",
            buffer = i64::from(second)
        ),
    );
    assert_ne!(numbers(&exec, "mark")[2], 3);
}

// eval.c:list2fpos — a character column against a nonexistent buffer fails
// the conversion, so `setcharpos` answers -1 without storing anything.
#[test]
fn setcharpos_in_a_nonexistent_buffer_fails() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\n\
         let g:rc = setcharpos(\"'A\", [9999, 1, 2, 0])\n\
         let g:mark = getpos(\"'A\")",
    );
    assert_eq!(number(&exec, "rc"), -1);
    assert_eq!(numbers(&exec, "mark"), vec![0, 0, 0, 0]);
}

// eval.c:list2fpos — a zero line number falls back to the cursor line for the
// character conversion only; the stored line stays zero, which `getpos`
// reports as no position.
#[test]
fn setcharpos_zero_line_converts_against_the_cursor_line() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef', '日本語x'])\n\
         call cursor(2, 1)\n\
         let g:rc = setcharpos(\"'A\", [0, 0, 3, 0])\n\
         let g:mark = getpos(\"'A\")",
    );
    assert_eq!(number(&exec, "rc"), 0);
    // Line 0 is stored as-is; the column came from line 2's third character.
    assert_eq!(numbers(&exec, "mark")[1], 0);
    assert_eq!(numbers(&exec, "mark")[2], 0);
}

// ---------------------------------------------------------------------------
// setpos on the cursor
// ---------------------------------------------------------------------------

// funcs.c:set_position — the `'.'` form writes the cursor, ignores `fnum`,
// and reports success.
#[test]
fn setpos_dot_moves_the_cursor_and_ignores_fnum() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef', 'ghijkl'])\n\
         let g:rc = setpos('.', [9999, 2, 3, 0])",
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 2, col: 2 }
    );
}

// funcs.c:set_position, cursor.c:check_cursor — a line past the end of the
// buffer is clamped to the last line, and a column past the end of the line
// to its last character.
#[test]
fn setpos_dot_clamps_past_the_end_of_the_buffer() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef', 'ghi'])\n\
         let g:rc = setpos('.', [0, 99, 99, 0])",
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 2, col: 2 }
    );
}

// cursor.c:check_cursor — an empty line puts the cursor on column zero.
#[test]
fn setpos_dot_on_an_empty_line_uses_column_zero() {
    let (mut editor, _buffer, window) = editor_with_window();
    run(
        &mut editor,
        "call setline(1, ['abcdef', ''])\ncall setpos('.', [0, 2, 5, 0])",
    );
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 2, col: 0 }
    );
}

// mark.c:mark_mb_adjustpos — a column landing inside a multibyte character is
// pulled back onto that character's first byte.
#[test]
fn setpos_dot_pulls_the_column_onto_a_head_byte() {
    let (mut editor, _buffer, window) = editor_with_window();
    run(
        &mut editor,
        "call setline(1, ['日本語x'])\ncall setpos('.', [0, 1, 5, 0])",
    );
    // Byte 4 is inside 本 (bytes 3..6), so the cursor lands on byte 3.
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 1, col: 3 }
    );
}

// funcs.c:set_position — the fourth element is `coladd`, reported back
// verbatim by `getcurpos`.
#[test]
fn setpos_dot_stores_the_virtual_offset() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\n\
         call setpos('.', [0, 1, 3, 4])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(numbers(&exec, "pos")[3], 4);
}

// eval.c:list2fpos — the list must hold three to five items when a buffer
// number is expected; anything else fails the conversion.
#[test]
fn setpos_rejects_lists_of_the_wrong_length() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\n\
         let g:short = setpos('.', [0, 1])\n\
         let g:long = setpos('.', [0, 1, 1, 0, 1, 1])",
    );
    assert_eq!(number(&exec, "short"), -1);
    assert_eq!(number(&exec, "long"), -1);
}

// eval.c:list2fpos — a negative buffer number, line or column fails the
// conversion.
#[test]
fn setpos_rejects_negative_components() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\n\
         let g:fnum = setpos('.', [-1, 1, 1, 0])\n\
         let g:lnum = setpos('.', [0, -1, 1, 0])\n\
         let g:col = setpos('.', [0, 1, -1, 0])",
    );
    assert_eq!(number(&exec, "fnum"), -1);
    assert_eq!(number(&exec, "lnum"), -1);
    assert_eq!(number(&exec, "col"), -1);
}

// eval.c:list2fpos — a non-list second argument fails the conversion.
#[test]
fn setpos_rejects_a_non_list_position() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\nlet g:rc = setpos('.', 1)",
    );
    assert_eq!(number(&exec, "rc"), -1);
}

// eval.c:list2fpos — a negative `off` is stored as zero.
#[test]
fn setpos_negative_offset_becomes_zero() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\n\
         call setpos('.', [0, 1, 2, -3])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(numbers(&exec, "pos")[3], 0);
}

// ---------------------------------------------------------------------------
// cursor and setcursorcharpos
// ---------------------------------------------------------------------------

// funcs.c:set_cursorpos — the two-argument form takes a one-based byte
// column.
#[test]
fn cursor_takes_a_one_based_byte_column() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef', 'ghijkl'])\nlet g:rc = cursor(2, 4)",
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 2, col: 3 }
    );
}

// funcs.c:set_cursorpos — a zero line keeps the cursor on its current line.
#[test]
fn cursor_zero_line_keeps_the_current_line() {
    let (mut editor, _buffer, window) = editor_with_window();
    run(
        &mut editor,
        "call setline(1, ['abcdef', 'ghijkl'])\ncall cursor(2, 1)\ncall cursor(0, 3)",
    );
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 2, col: 2 }
    );
}

// funcs.c:set_cursorpos — the third argument is the virtual offset.
#[test]
fn cursor_accepts_a_virtual_offset() {
    let (mut editor, _buffer, window) = editor_with_window();
    run(
        &mut editor,
        "call setline(1, ['abcdef'])\ncall cursor(1, 3, 2)",
    );
    assert_eq!(editor.window(window).unwrap().coladd, 2);
}

// funcs.c:set_cursorpos — `'$'` resolves through `tv_get_lnum` to the last
// line of the buffer.
#[test]
fn cursor_dollar_line_is_the_last_line() {
    let (mut editor, _buffer, window) = editor_with_window();
    run(
        &mut editor,
        "call setline(1, ['a', 'b', 'c'])\ncall cursor('$', 1)",
    );
    assert_eq!(editor.window(window).unwrap().cursor.lnum, 3);
}

// funcs.c:set_cursorpos — neither a list nor a number/string pair raises
// E474.
#[test]
fn cursor_with_one_scalar_argument_raises_e474() {
    let (mut editor, _buffer, _window) = editor_with_window();
    assert_eq!(
        error_code(&mut editor, "call setline(1, ['abcdef'])\ncall cursor(1)"),
        "E474"
    );
}

// funcs.c:set_cursorpos — a list that cannot be converted raises E474.
#[test]
fn cursor_with_an_unconvertible_list_raises_e474() {
    let (mut editor, _buffer, _window) = editor_with_window();
    assert_eq!(
        error_code(&mut editor, "call setline(1, ['abcdef'])\ncall cursor([1])"),
        "E474"
    );
}

// funcs.c:set_cursorpos — `setcursorcharpos` takes a character index and
// converts it against the cursor's buffer.
#[test]
fn setcursorcharpos_converts_a_character_index() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\nlet g:rc = setcursorcharpos(1, 3)",
    );
    assert_eq!(number(&exec, "rc"), 0);
    // The third character 語 starts at byte 6.
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 1, col: 6 }
    );
}

// funcs.c:set_cursorpos — the list form of `setcursorcharpos` converts the
// same way.
#[test]
fn setcursorcharpos_list_form_converts_a_character_index() {
    let (mut editor, _buffer, window) = editor_with_window();
    run(
        &mut editor,
        "call setline(1, ['abc', '日本語x'])\ncall setcursorcharpos([2, 2])",
    );
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 2, col: 3 }
    );
}

// funcs.c:set_cursorpos — a zero line in the two-argument character form
// converts against the cursor's line.
#[test]
fn setcursorcharpos_zero_line_converts_against_the_cursor_line() {
    let (mut editor, _buffer, window) = editor_with_window();
    run(
        &mut editor,
        "call setline(1, ['abc', '日本語x'])\n\
         call cursor(2, 1)\n\
         call setcursorcharpos(0, 3)",
    );
    assert_eq!(
        editor.window(window).unwrap().cursor,
        Position { lnum: 2, col: 6 }
    );
}

// ---------------------------------------------------------------------------
// getpos, getcharpos, getcursorcharpos
// ---------------------------------------------------------------------------

// funcs.c:getpos_both — `getpos('.')` answers the cursor without the fifth
// element the cursor forms carry.
#[test]
fn getpos_dot_answers_four_elements() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\ncall cursor(1, 3)\nlet g:pos = getpos('.')",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 3, 0]);
}

// eval.c:var2fpos — `'$'` without a line context answers the cursor line and
// the byte length of that line as the column.
#[test]
fn getpos_dollar_answers_the_last_line() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abc', 'defgh'])\nlet g:pos = getpos('$')",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 2, 1, 0]);
}

// eval.c:var2fpos — with Visual mode inactive, `'v'` answers the cursor.
#[test]
fn getpos_v_answers_the_cursor_when_visual_is_inactive() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\ncall cursor(1, 4)\nlet g:pos = getpos('v')",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 4, 0]);
}

// eval.c:var2fpos — an unset mark names no position, which `getpos` reports
// as all zeros.
#[test]
fn getpos_unset_mark_answers_zeros() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\nlet g:pos = getpos(\"'z\")",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 0, 0, 0]);
}

// eval.c:var2fpos — the list form is validated against the line it names: a
// column one past the end of the line is accepted, two past is not.
#[test]
fn getpos_list_form_accepts_one_column_past_the_line() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abc'])\n\
         let g:edge = getpos([1, 4, 0])\n\
         let g:past = getpos([1, 5, 0])",
    );
    assert_eq!(numbers(&exec, "edge"), vec![0, 1, 4, 0]);
    assert_eq!(numbers(&exec, "past"), vec![0, 0, 0, 0]);
}

// eval.c:var2fpos — a `'$'` column in the list form asks for the column one
// past the last byte of that line.
#[test]
fn getpos_list_form_dollar_column_is_one_past_the_line() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcde'])\nlet g:pos = getpos([1, '$', 0])",
    );
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 6, 0]);
}

// eval.c:var2fpos — a line outside the buffer names no position.
#[test]
fn getpos_list_form_rejects_a_line_outside_the_buffer() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abc'])\n\
         let g:zero = getpos([0, 1, 0])\n\
         let g:past = getpos([9, 1, 0])",
    );
    assert_eq!(numbers(&exec, "zero"), vec![0, 0, 0, 0]);
    assert_eq!(numbers(&exec, "past"), vec![0, 0, 0, 0]);
}

// eval.c:buf_byteidx_to_charidx — `getcharpos` reports the column as a
// character index.
#[test]
fn getcharpos_reports_a_character_column() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\n\
         call cursor(1, 7)\n\
         let g:byte = getpos('.')\n\
         let g:char = getcharpos('.')",
    );
    assert_eq!(numbers(&exec, "byte"), vec![0, 1, 7, 0]);
    assert_eq!(numbers(&exec, "char"), vec![0, 1, 3, 0]);
}

// funcs.c:getpos_both — `getcursorcharpos` is `getcurpos` with a character
// column, so it keeps the fifth element.
#[test]
fn getcursorcharpos_reports_a_character_column_and_the_wanted_column() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\ncall cursor(1, 7)\nlet g:pos = getcursorcharpos()",
    );
    let pos = numbers(&exec, "pos");
    assert_eq!(pos.len(), 5);
    assert_eq!(&pos[..4], &[0, 1, 3, 0]);
}

// eval.c:var2fpos — the character-column list form counts characters when
// validating the column against the line.
#[test]
fn getcharpos_list_form_counts_characters() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\n\
         let g:edge = getcharpos([1, 4, 0])\n\
         let g:past = getcharpos([1, 6, 0])",
    );
    assert_eq!(numbers(&exec, "edge"), vec![0, 1, 4, 0]);
    assert_eq!(numbers(&exec, "past"), vec![0, 0, 0, 0]);
}

// ---------------------------------------------------------------------------
// col and charcol
// ---------------------------------------------------------------------------

// funcs.c:get_col — `col('.')` is the cursor's one-based byte column.
#[test]
fn col_dot_is_the_cursor_byte_column() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\ncall cursor(1, 7)\nlet g:col = col('.')",
    );
    assert_eq!(number(&exec, "col"), 7);
}

// funcs.c:get_col — `charcol('.')` is the cursor's one-based character
// column.
#[test]
fn charcol_dot_is_the_cursor_character_column() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\ncall cursor(1, 7)\nlet g:col = charcol('.')",
    );
    assert_eq!(number(&exec, "col"), 3);
}

// eval.c:var2fpos — `'$'` in a column context answers one past the last byte
// of the cursor line.
#[test]
fn col_dollar_is_one_past_the_cursor_line() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcde'])\n\
         call cursor(1, 1)\n\
         let g:byte = col('$')\n\
         let g:char = charcol('$')",
    );
    assert_eq!(number(&exec, "byte"), 6);
    assert_eq!(number(&exec, "char"), 6);
}

// eval.c:var2fpos — `charcol('$')` counts characters, so a multibyte line
// answers fewer columns than `col('$')`.
#[test]
fn charcol_dollar_counts_characters() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\n\
         call cursor(1, 1)\n\
         let g:byte = col('$')\n\
         let g:char = charcol('$')",
    );
    assert_eq!(number(&exec, "byte"), 11);
    assert_eq!(number(&exec, "char"), 5);
}

// funcs.c:get_col — an unset mark names no position, which answers column 0.
#[test]
fn col_of_an_unset_mark_is_zero() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdef'])\nlet g:col = col(\"'z\")",
    );
    assert_eq!(number(&exec, "col"), 0);
}

// funcs.c:get_col — a mark in another buffer answers 0, because `get_col`
// only reports columns from the window's own buffer.
#[test]
fn col_of_a_mark_in_another_buffer_is_zero() {
    let (mut editor, _first, _window) = editor_with_window();
    let second = add_buffer(&mut editor, &["bbbbbbb"]);
    let exec = run(
        &mut editor,
        &format!(
            "call setline(1, ['aaaaa'])\n\
             call setpos(\"'A\", [{buffer}, 1, 3, 0])\n\
             let g:col = col(\"'A\")",
            buffer = i64::from(second)
        ),
    );
    assert_eq!(number(&exec, "col"), 0);
}

// funcs.c:get_col, typval.c:tv_check_for_number_arg — the window argument
// must be a Number.
#[test]
fn col_rejects_a_non_number_window_argument() {
    let (mut editor, _buffer, _window) = editor_with_window();
    assert_eq!(
        error_code(
            &mut editor,
            "call setline(1, ['abcdef'])\ncall col('.', 'x')"
        ),
        "E1210"
    );
}

// funcs.c:get_col — the first argument must be a String or a List.
#[test]
fn col_rejects_a_number_position() {
    let (mut editor, _buffer, _window) = editor_with_window();
    assert_eq!(
        error_code(&mut editor, "call setline(1, ['abcdef'])\ncall col(1)"),
        "E1222"
    );
}

// ---------------------------------------------------------------------------
// line
// ---------------------------------------------------------------------------

// funcs.c:f_line — `line('.')` is the cursor line and `line('$')` the last
// line of the buffer.
#[test]
fn line_dot_and_dollar() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['a', 'b', 'c'])\n\
         call cursor(2, 1)\n\
         let g:dot = line('.')\n\
         let g:last = line('$')",
    );
    assert_eq!(number(&exec, "dot"), 2);
    assert_eq!(number(&exec, "last"), 3);
}

// eval.c:var2fpos — `w0` is the window's top line and `w$` its last displayed
// line, both only in a line context.
#[test]
fn line_w0_and_wdollar_follow_the_window() {
    let (mut editor, buffer, window) = editor_with_window();
    let lines = (1..=40)
        .map(|index| format!("'line{index}'"))
        .collect::<Vec<_>>()
        .join(", ");
    run(&mut editor, &format!("call setline(1, [{lines}])"));
    assert_eq!(
        editor.buffer(buffer).unwrap().text().unwrap().line_count(),
        40
    );
    editor.set_window_topline(window, 5).unwrap();
    let exec = run(
        &mut editor,
        "let g:top = line('w0')\nlet g:bot = line('w$')",
    );
    assert_eq!(number(&exec, "top"), 5);
    // A 24-row window starting at line 5 shows through line 28.
    assert_eq!(number(&exec, "bot"), 28);
}

// eval.c:var2fpos — a mark in the current buffer answers its line.
#[test]
fn line_of_a_mark_is_its_stored_line() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['a', 'b', 'c'])\n\
         call setpos(\"'a\", [0, 3, 1, 0])\n\
         let g:lnum = line(\"'a\")",
    );
    assert_eq!(number(&exec, "lnum"), 3);
}

// funcs.c:f_line — an expression naming no position answers 0.
#[test]
fn line_of_an_unset_mark_is_zero() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['a'])\nlet g:lnum = line(\"'z\")",
    );
    assert_eq!(number(&exec, "lnum"), 0);
}

// ---------------------------------------------------------------------------
// virtcol
// ---------------------------------------------------------------------------

// plines.c:getvcol — `virtcol('.')` answers the last cell of the character
// under the cursor, and the list form answers its first and last cells.
#[test]
fn virtcol_expands_a_tab_into_its_cell_span() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, \"the\tquick\")\n\
         call setpos('.', [0, 1, 4, 0])\n\
         let g:end = virtcol('.')\n\
         let g:span = virtcol('.', v:true)",
    );
    assert_eq!(number(&exec, "end"), 8);
    assert_eq!(numbers(&exec, "span"), vec![4, 8]);
}

// plines.c:getvcol — a wide character occupies two cells.
#[test]
fn virtcol_counts_two_cells_for_a_wide_character() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\nlet g:span = virtcol([1, 1], v:true)",
    );
    assert_eq!(numbers(&exec, "span"), vec![1, 2]);
}

// funcs.c:f_virtcol — an expression naming no position answers 0, or a pair
// of zeros when a list result was asked for.
#[test]
fn virtcol_of_an_unset_mark_is_zero() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abc'])\n\
         let g:plain = virtcol(\"'z\")\n\
         let g:list = virtcol(\"'z\", v:true)",
    );
    assert_eq!(number(&exec, "plain"), 0);
    assert_eq!(numbers(&exec, "list"), vec![0, 0]);
}

// funcs.c:f_virtcol — the third argument names the window the column is
// measured in.
#[test]
fn virtcol_third_argument_names_a_window() {
    let (mut editor, _buffer, window) = editor_with_window();
    let exec = run(
        &mut editor,
        &format!(
            "call setline(1, \"the\tquick\")\nlet g:end = virtcol([1, 4], v:false, {})",
            i64::from(window)
        ),
    );
    assert_eq!(number(&exec, "end"), 8);
}

// funcs.c:f_virtcol — an unknown window answers 0.
#[test]
fn virtcol_in_an_unknown_window_is_zero() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abc'])\nlet g:end = virtcol([1, 1], v:false, 9999)",
    );
    assert_eq!(number(&exec, "end"), 0);
}

// ---------------------------------------------------------------------------
// Arity (eval.lua argument counts)
// ---------------------------------------------------------------------------

// The generated `eval.lua` table is the arity authority: too few arguments
// raise E119 and too many raise E118, before any body runs.
#[test]
fn position_builtins_enforce_their_eval_lua_arity() {
    for (script, code) in [
        ("call col()", "E119"),
        ("call col('.', 0, 0)", "E118"),
        ("call charcol()", "E119"),
        ("call cursor()", "E119"),
        ("call cursor(1, 1, 0, 0)", "E118"),
        ("call getcharpos()", "E119"),
        ("call getcurpos(0, 0)", "E118"),
        ("call getcursorcharpos(0, 0)", "E118"),
        ("call getpos()", "E119"),
        ("call getpos('.', '.')", "E118"),
        ("call line()", "E119"),
        ("call line('.', 0, 0)", "E118"),
        ("call setcharpos('.')", "E119"),
        ("call setcursorcharpos()", "E119"),
        ("call setpos('.')", "E119"),
        ("call setpos('.', [0, 1, 1, 0], 0)", "E118"),
        ("call virtcol()", "E119"),
        ("call virtcol('.', v:true, 0, 0)", "E118"),
    ] {
        let (mut editor, _buffer, _window) = editor_with_window();
        assert_eq!(
            error_code(&mut editor, script),
            code,
            "wrong arity error for `{script}`"
        );
    }
}

// funcs.c — `getcurpos` and `getcursorcharpos` take no required argument, so
// the bare call is valid.
#[test]
fn cursor_readers_accept_no_arguments() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abc'])\n\
         let g:curpos = getcurpos()\n\
         let g:charpos = getcursorcharpos()",
    );
    assert_eq!(numbers(&exec, "curpos").len(), 5);
    assert_eq!(numbers(&exec, "charpos").len(), 5);
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

// funcs.c — `setpos`/`getpos` and `setcharpos`/`getcharpos` round trip a
// multibyte position in their own column space.
#[test]
fn position_writers_and_readers_round_trip() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['日本語x'])\n\
         call setpos('.', [0, 1, 7, 0])\n\
         let g:bytes = getpos('.')\n\
         call setcharpos('.', [0, 1, 3, 0])\n\
         let g:chars = getcharpos('.')",
    );
    assert_eq!(numbers(&exec, "bytes"), vec![0, 1, 7, 0]);
    assert_eq!(numbers(&exec, "chars"), vec![0, 1, 3, 0]);
}

// funcs.c:set_cursorpos, funcs.c:getpos_both — `cursor()` and `getcurpos()`
// round trip through the wanted column a five-element list pins.
#[test]
fn cursor_and_getcurpos_round_trip_a_pinned_wanted_column() {
    let (mut editor, _buffer, _window) = editor_with_window();
    let exec = run(
        &mut editor,
        "call setline(1, ['abcdefgh'])\n\
         let g:rc = cursor([1, 3, 0, 7])\n\
         let g:pos = getcurpos()",
    );
    assert_eq!(number(&exec, "rc"), 0);
    assert_eq!(numbers(&exec, "pos"), vec![0, 1, 3, 0, 7]);
}
