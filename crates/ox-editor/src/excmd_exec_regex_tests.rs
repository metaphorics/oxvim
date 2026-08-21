#![allow(clippy::unwrap_used)]

//! Regex-driven Ex execution tests.
//!
//! Upstream contracts:
//! - `src/nvim/ex_cmds.c` (`ex_global`, `ex_vglobal`, `ex_substitute`, `ex_delete`, `ex_yank`, `ex_put`)
//! - `src/nvim/regexp.c` (pattern compilation, `\(\)` captures, `\c`/case atoms, `&`)
//! - `test/old/testdir/test_global.vim`
//! - `test/old/testdir/test_substitute.vim`

use ox_text::{Buffer, Position};
use ox_types::{BufHandle, WinHandle};

use crate::layout::Geometry;
use crate::{Editor, ExExecutor};

fn position(lnum: usize, col: usize) -> Position {
    Position { lnum, col }
}

fn setup(text: &str) -> (Editor, BufHandle, WinHandle, ExExecutor) {
    let mut editor = Editor::new();
    let buffer = editor
        .create_buffer_with(Buffer::from_bytes(text.as_bytes()).unwrap(), true)
        .unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor
        .set_window_cursor(window, position(1, 0))
        .unwrap();
    let executor = ExExecutor::new();
    (editor, buffer, window, executor)
}

// ---------------------------------------------------------------------------
// :global / :vglobal selection and two-phase marking
// ---------------------------------------------------------------------------

#[test]
fn global_default_range_honors_current_line() {
    // `ex_global` default range is the current line (`ex_cmds.c:4946-4954`).
    let (mut editor, _buffer, window, mut executor) = setup("foo\nbar\nbaz");
    editor.set_window_cursor(window, position(2, 0)).unwrap();
    executor.execute_line(&mut editor, r"g/bar/d").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"foo\nbaz");
}

#[test]
fn global_whole_buffer_range_appends_to_all_matches() {
    // `%` range marks every matching line before the nested command runs.
    let (mut editor, _buffer, _window, mut executor) = setup("foo\nbar\nfoo");
    executor.execute_line(&mut editor, r"%g/foo/s/$/ END/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.line(1).unwrap(), b"foo END");
    assert_eq!(text.line(2).unwrap(), b"bar");
    assert_eq!(text.line(3).unwrap(), b"foo END");
}

#[test]
fn global_explicit_line_range_limits_matches() {
    // Explicit `2,3` restricts marking to that range (`test_global.vim`).
    let (mut editor, _buffer, _window, mut executor) = setup("one\nbar1\nbar2\nfoo");
    executor.execute_line(&mut editor, r"2,3g/bar/s/$/ X/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.line(1).unwrap(), b"one");
    assert_eq!(text.line(2).unwrap(), b"bar1 X");
    assert_eq!(text.line(3).unwrap(), b"bar2 X");
    assert_eq!(text.line(4).unwrap(), b"foo");
}

#[test]
fn vglobal_inverts_selection() {
    // `:v` inverts the match (`ex_cmds.c:4955-4961`).
    let (mut editor, _buffer, _window, mut executor) = setup("foo\nbar\nbaz");
    executor.execute_line(&mut editor, r"%v/foo/s/$/ X/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.line(1).unwrap(), b"foo");
    assert_eq!(text.line(2).unwrap(), b"bar X");
    assert_eq!(text.line(3).unwrap(), b"baz X");
}

#[test]
fn global_missing_pattern_reports_e148() {
    // No delimiter/pattern produces E148 (`ex_cmds.c:4962-4968`).
    let (mut editor, _buffer, _window, mut executor) = setup("foo\nbar");
    let error = executor.execute_line(&mut editor, "g").unwrap_err();
    assert!(error.to_string().contains("E148"));
}

#[test]
fn global_invalid_regex_reports_e54() {
    // Unbalanced `[` fails regex compilation (`regexp.c`).
    let (mut editor, _buffer, _window, mut executor) = setup("foo\nbar");
    let error = executor.execute_line(&mut editor, r"g/[/d").unwrap_err();
    assert!(error.to_string().contains("E54"));
}

#[test]
fn global_nested_delete_uses_marked_cursor() {
    // `:global` sets the current line for the nested command before it runs.
    let (mut editor, _buffer, _window, mut executor) = setup("foo\nbar\nbaz");
    executor.execute_line(&mut editor, r"%g/bar/d").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"foo\nbaz");
}

// ---------------------------------------------------------------------------
// :substitute ranges, flags, delimiters, escapes, captures and expressions
// ---------------------------------------------------------------------------

#[test]
fn substitute_default_range_only_current_line() {
    // Default range is the current line (`test_substitute.vim`).
    let (mut editor, _buffer, window, mut executor) = setup("one\nfoo\nthree");
    editor.set_window_cursor(window, position(2, 0)).unwrap();
    executor.execute_line(&mut editor, r"s/foo/bar/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.line(2).unwrap(), b"bar");
    assert_eq!(text.line(3).unwrap(), b"three");
}

#[test]
fn substitute_whole_buffer_range_replaces_all() {
    let (mut editor, _buffer, _window, mut executor) = setup("foo\nbar\nfoo");
    executor.execute_line(&mut editor, r"%s/foo/bar/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"bar\nbar\nbar");
}

#[test]
fn substitute_first_match_only_without_g() {
    // Without the `g` flag only the first match is replaced.
    let (mut editor, _buffer, _window, mut executor) = setup("foo foo");
    executor.execute_line(&mut editor, r"s/foo/bar/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"bar foo");
}

#[test]
fn substitute_global_flag_replaces_every_match() {
    // The `g` flag replaces every match on the line.
    let (mut editor, _buffer, _window, mut executor) = setup("foo foo");
    executor.execute_line(&mut editor, r"s/foo/bar/g").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"bar bar");
}

#[test]
fn substitute_no_match_errors_e486() {
    // Missing match reports E486 unless suppressed (`test_substitute.vim`).
    let (mut editor, _buffer, _window, mut executor) = setup("foo");
    let error = executor.execute_line(&mut editor, r"s/missing/x/").unwrap_err();
    assert!(error.to_string().contains("E486"));
    assert!(error.to_string().contains("missing"));
}

#[test]
fn substitute_e_flag_suppresses_no_match_error() {
    let (mut editor, _buffer, _window, mut executor) = setup("foo");
    let outcome = executor.execute_line(&mut editor, r"s/missing/x/e").unwrap();
    assert_eq!(outcome, crate::ExecOutcome::Completed);
}

#[test]
fn substitute_alternate_delimiter_hash() {
    // Any non-alphanumeric can delimit (`ex_cmds.c:4200-4212`).
    let (mut editor, _buffer, _window, mut executor) = setup("foo");
    executor.execute_line(&mut editor, r"s#foo#bar#").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"bar");
}

#[test]
fn substitute_escaped_slash_in_pattern() {
    // Escaped delimiter may appear in the pattern (`regexp.c`).
    let (mut editor, _buffer, _window, mut executor) = setup("a/b");
    executor.execute_line(&mut editor, r"s/a\/b/x/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"x");
}

#[test]
fn substitute_escaped_delimiter_in_replacement() {
    // Escaped delimiter may appear in the replacement.
    let (mut editor, _buffer, _window, mut executor) = setup("foo");
    executor.execute_line(&mut editor, r"s#foo#f\#o#").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"f#o");
}

#[test]
fn substitute_ampersand_inserts_whole_match() {
    // `&` expands to the matched text (`regexp.c:2200-2210`).
    let (mut editor, _buffer, _window, mut executor) = setup("abc");
    executor.execute_line(&mut editor, r"s/abc/& &/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"abc abc");
}

#[test]
fn substitute_backreference_reorders_captures() {
    // `\1`..`\9` refer to `\(\)` captures (`regexp.c`).
    let (mut editor, _buffer, _window, mut executor) = setup("ab");
    executor.execute_line(&mut editor, r"s/\(.\)\(.\)/\2\1/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"ba");
}

#[test]
fn substitute_expression_uses_submatch() {
    // `\=...` evaluates the replacement as an expression (`ex_cmds.c`).
    let (mut editor, _buffer, _window, mut executor) = setup("foo");
    executor
        .execute_line(&mut editor, r"s/foo/\=submatch(0).'-ok'/")
        .unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"foo-ok");
}

#[test]
fn substitute_case_sensitive_by_default() {
    // Patterns match case by default (`regexp.c`).
    let (mut editor, _buffer, _window, mut executor) = setup("Foo\nFoo");
    executor.execute_line(&mut editor, r"%s/foo/bar/e").unwrap();
    {
        let state = editor.current_buffer().unwrap();
        let text = editor.buffer(state).unwrap().text().unwrap();
        assert_eq!(text.line(1).unwrap(), b"Foo");
        assert_eq!(text.line(2).unwrap(), b"Foo");
    }
    executor.execute_line(&mut editor, r"%s/Foo/bar/").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"bar\nbar");
}

#[test]
fn substitute_i_flag_ignores_case() {
    // The `i` flag makes the pattern case-insensitive (`test_substitute.vim`).
    let (mut editor, _buffer, _window, mut executor) = setup("Foo\nFoo");
    executor.execute_line(&mut editor, r"%s/foo/bar/i").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"bar\nbar");
}

#[test]
fn substitute_c_flag_errors_without_interactive_ui() {
    // The `c` flag requires confirmation UI; without one the command errors
    // and must not silently replace.
    let (mut editor, _buffer, _window, mut executor) = setup("foo");
    let error = executor.execute_line(&mut editor, r"s/foo/bar/c").unwrap_err();
    assert!(error.to_string().contains("confirmation"));
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"foo");
}

// ---------------------------------------------------------------------------
// :delete, :yank and :put with registers
// ---------------------------------------------------------------------------

#[test]
fn ex_delete_default_current_line() {
    let (mut editor, _buffer, window, mut executor) = setup("one\ntwo\nthree");
    editor.set_window_cursor(window, position(2, 0)).unwrap();
    executor.execute_line(&mut editor, "delete").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"one\nthree");
}

#[test]
fn ex_delete_explicit_range_to_register() {
    let (mut editor, _buffer, _window, mut executor) = setup("one\ntwo\nthree\nfour");
    executor.execute_line(&mut editor, "2,3delete a").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"one\nfour");
    let register = editor.registers().get('a').unwrap().unwrap();
    assert_eq!(register.to_bytes(), b"two\nthree");
}

#[test]
fn ex_yank_explicit_range_to_register() {
    let (mut editor, _buffer, _window, mut executor) = setup("one\ntwo\nthree\nfour");
    executor.execute_line(&mut editor, "2,3yank a").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"one\ntwo\nthree\nfour");
    let register = editor.registers().get('a').unwrap().unwrap();
    assert_eq!(register.to_bytes(), b"two\nthree");
}

#[test]
fn ex_yank_default_current_line_to_unnamed() {
    let (mut editor, _buffer, window, mut executor) = setup("one\ntwo\nthree");
    editor.set_window_cursor(window, position(2, 0)).unwrap();
    executor.execute_line(&mut editor, "yank").unwrap();
    let register = editor.registers().get('"').unwrap().unwrap();
    assert_eq!(register.to_bytes(), b"two");
}

#[test]
fn ex_put_appends_register_below() {
    // `:put` inserts linewise text after the current line (`ex_cmds.c`).
    let (mut editor, _buffer, window, mut executor) = setup("one\ntwo\nthree");
    editor.set_window_cursor(window, position(2, 0)).unwrap();
    executor.execute_line(&mut editor, "yank a").unwrap();
    editor.set_window_cursor(window, position(1, 0)).unwrap();
    executor.execute_line(&mut editor, "put a").unwrap();
    let state = editor.current_buffer().unwrap();
    let text = editor.buffer(state).unwrap().text().unwrap();
    assert_eq!(text.line(1).unwrap(), b"one");
    assert_eq!(text.line(2).unwrap(), b"two");
    assert_eq!(text.line(3).unwrap(), b"two");
}
