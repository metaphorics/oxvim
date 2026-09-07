#![allow(clippy::unwrap_used)]

//! Behavioral tests for `ExExecutor` state-mutating commands.
//!
//! Covers `:let`/`:const`/`:unlet` scoped variables, option/register/env
//! targets, `:set`/`:setlocal`/`:setglobal` booleans/numbers/strings/query/
//! reset/errors, `:echo`/`:echomsg`/`:echon` message history and output,
//! `:execute`, `:normal`, `:marks`/`:registers` output, `:highlight` storage,
//! unsupported-command `NotImplemented` identity, and exact E121/E488
//! line-numbered sample shapes.
//!
//! Upstream citations:
//! - `src/nvim/ex_docmd.c`: `do_cmdline` dispatch, `ex_let`, `ex_set`,
//!   `ex_echo`, `ex_execute`, `ex_normal`, `ex_mark`, `ex_registers`,
//!   `ex_highlight`, error-abort semantics.
//! - `src/nvim/eval.c`: `set_var`, `unlet_var`, E121 undefined variable,
//!   E46 read-only, E108 no such variable.
//! - `src/nvim/option.c`: `set_option_value`, `show_one`, E518 unknown
//!   option, E355 unknown option (internal), E474 wrong type.
//! - `test/old/testdir/test_let.vim`: `:let`/`:const`/`:unlet` semantics.
//! - `test/old/testdir/test_options.vim`: `:set`/`:setlocal`/`:setglobal`.

use ox_eval::ScopeKind;
use ox_text::Position;
use ox_types::{Object, OxStr, Typval, WinHandle};

use crate::TestEditorAccess;
use crate::excmd_exec::{ExecError, ExecOutcome};
use crate::register::RegisterContent;
use crate::{
    AutocmdKind, AutocmdOptions, DirectoryScope, Editor, Event, ExExecutor, Geometry,
    LineReplaceRequest, LuaExec, LuaExecError, MessageKind, OptionValue, VimExceptionKind,
    vim_variable_is_writable,
};

/// Build an editor with one listed buffer and a tabpage so window-local
/// and buffer-local options are accessible.
fn editor_with_window() -> (Editor, ox_types::BufHandle, WinHandle) {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let tab = editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    (editor, buffer, window)
}

/// Extract the text of a string-valued message.
fn message_text(msg: &crate::Message) -> String {
    match &msg.content {
        Object::String(s) => s.to_string_lossy().into_owned(),
        _ => panic!("expected string message content, got {:?}", msg.content),
    }
}

fn assert_vim_error(result: Result<ExecOutcome, ExecError>, code: &str) {
    let ExecError::Vim(exception) = result.unwrap_err() else {
        panic!("expected Vim error {code}")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error(code.to_owned()));
}

struct CwdFixture {
    original: std::path::PathBuf,
    directory: std::path::PathBuf,
}

impl CwdFixture {
    fn new(name: &str) -> Self {
        let original = std::env::current_dir().unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("ox-editor-{name}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        Self {
            original,
            directory,
        }
    }

    fn child(&self, name: &str) -> std::path::PathBuf {
        let directory = self.directory.join(name);
        std::fs::create_dir(&directory).unwrap();
        directory
    }
}

impl Drop for CwdFixture {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

// ── let / const / unlet scoped variables ──────────────────────────────

// ex_docmd.c:ex_let, eval.c:set_var — `:let g:name = value` stores into
// the global scope and is readable after execution.
#[test]
fn let_global_variable_assigns_to_g_scope() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:state_var = 42").unwrap();
    let value = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"state_var", 0)
        .unwrap();
    assert_eq!(*value, Typval::Number(42));
}

// eval.c:set_var, E46 — `:const` locks the variable; a subsequent `:let`
// on the same name raises E46 "Cannot change read-only variable".
#[test]
fn const_then_reassign_produces_e46() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "const g:IMMUTABLE = 1").unwrap();
    let err = exec
        .execute_line(&editor, "let g:IMMUTABLE = 2")
        .unwrap_err();
    match err {
        ExecError::Vim(vim_exc) => {
            assert_eq!(vim_exc.kind, VimExceptionKind::Error("E46".to_owned()));
            assert!(
                vim_exc
                    .message()
                    .contains("Cannot change read-only variable")
            );
            assert!(vim_exc.message().contains("g:IMMUTABLE"));
        }
        other => panic!("expected Vim E46, got {other:?}"),
    }
}

// eval.c:unlet_var, E108 — `:unlet` removes the variable; `:unlet!`
// suppresses E108 for names that do not exist.
#[test]
fn unlet_removes_variable_and_bang_suppresses_e108() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:tmp = 1").unwrap();
    exec.execute_line(&editor, "unlet g:tmp").unwrap();
    assert!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"tmp", 0)
            .is_err()
    );
    // `unlet!` on a non-existent name must not error.
    let result = exec.execute_line(&editor, "unlet! g:no_such");
    assert!(result.is_ok());
}

// eval/vars.c:ex_unletlock 1587-1600, ex_let_env 1323-1330 — a `$` target is
// measured with `get_env_len` before the unset or set runs. Before this guard
// `unlet $` reached `std::env::remove_var("")`, which panics and takes the
// whole editor with it (`test_unlet.vim:23`, oldtest rc 101).
//
// Each case fails exactly one part of the compound rule, so no part can be
// dropped without flipping one line:
//   `unlet $`      — empty name, E475 naming the remaining argument
//   `unlet $ tail` — empty name with a remainder, pinning that the message is
//                    the whole rest and not just the token
//   `unlet $A=B`   — non-empty name with garbage after it, E488, which the
//                    empty-name branch alone would answer E475
//   `unlet $HOME`  — a wholly valid name, which both error branches must let
//                    through
#[test]
fn unlet_env_target_measures_the_name_before_unsetting() {
    let editor = TestEditorAccess::new(Editor::new());
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _env = crate::test_guard::EnvGuard::new(&["OX_UNLET_KEEP"]);
    let mut exec = ExExecutor::new();

    for (line, code, message) in [
        ("unlet $", "E475", "Invalid argument: $"),
        ("unlet $ tail", "E475", "Invalid argument: $ tail"),
        ("unlet $OX_UNLET_A=B", "E488", "Trailing characters: =B"),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        let ExecError::Vim(exception) = error else {
            panic!("expected a Vim error for {line:?}")
        };
        assert_eq!(
            exception.kind,
            VimExceptionKind::Error(code.to_owned()),
            "{line:?}"
        );
        assert!(
            exception.message().contains(message),
            "{line:?}: {}",
            exception.message()
        );
    }

    // `unlet!` skips E108, not the name check: upstream reports E475 before it
    // ever consults `eap->forceit`.
    let error = exec.execute_line(&editor, "unlet! $").unwrap_err();
    let ExecError::Vim(exception) = error else {
        panic!("expected a Vim error")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E475".to_owned()));

    // A well-formed name still reaches the unset.
    exec.execute_line(&editor, "let $OX_UNLET_KEEP = 'v'")
        .unwrap();
    assert_eq!(std::env::var("OX_UNLET_KEEP").as_deref(), Ok("v"));
    exec.execute_line(&editor, "unlet $OX_UNLET_KEEP").unwrap();
    assert_eq!(std::env::var_os("OX_UNLET_KEEP"), None);
}

// eval/vars.c:ex_let_env 1323-1330 — `:let $` reports E475 naming the whole
// remaining argument, and only after the value expression has been evaluated,
// so a bad expression is still reported first.
#[test]
fn let_env_target_reports_e475_after_evaluating_the_value() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    for (line, message) in [
        ("let $=1", "Invalid argument: $=1"),
        ("let $ = 'x'", "Invalid argument: $ = 'x'"),
    ] {
        let error = exec.execute_line(&editor, line).unwrap_err();
        let ExecError::Vim(exception) = error else {
            panic!("expected a Vim error for {line:?}")
        };
        assert_eq!(
            exception.kind,
            VimExceptionKind::Error("E475".to_owned()),
            "{line:?}"
        );
        assert!(
            exception.message().contains(message),
            "{line:?}: {}",
            exception.message()
        );
    }

    // Ordering: the value is evaluated first, so this is E121 and not E475.
    let error = exec
        .execute_line(&editor, "let $ = g:no_such_variable")
        .unwrap_err();
    let ExecError::Vim(exception) = error else {
        panic!("expected a Vim error")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E121".to_owned()));
}

// eval.c:set_var, `+=` compound assignment — reads the current value,
// applies the operator, and writes back the result.
#[test]
fn let_compound_addition_operator() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:counter = 10").unwrap();
    exec.execute_line(&editor, "let g:counter += 5").unwrap();
    let value = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"counter", 0)
        .unwrap();
    assert_eq!(*value, Typval::Number(15));
}

#[test]
fn let_compound_assignment_reads_then_writes() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "<list-plus-equal>",
        "let g:items = [1, 2]\nlet g:alias = g:items\nlet g:items += [3, 4]",
    )
    .unwrap();
    let items = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"items", 0)
        .unwrap();
    let alias = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"alias", 0)
        .unwrap();

    let expected = Typval::list(vec![
        Typval::Number(1),
        Typval::Number(2),
        Typval::Number(3),
        Typval::Number(4),
    ]);
    assert_eq!(*items, expected);
    assert_eq!(*alias, expected);
}

// ex_docmd.c:ex_let, `b:` scope — `:let b:name = value` writes through
// sync_scope_into_editor into the buffer's API variable dict.
#[test]
fn let_buffer_scoped_variable_writes_to_editor() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"let b:bufvar = "hello""#)
        .unwrap();
    let ed = editor.editor();

    let vars = ed.buffer(buffer).unwrap().variables();
    let value = vars.get(&OxStr::from("bufvar")).unwrap();
    assert_eq!(value, &Object::String(OxStr::from("hello")));
}

// ── option / register / env targets ───────────────────────────────────

// ex_docmd.c:ex_let with `&opt` target — `:let &number = 1` (boolean,
// window-local) and `:let &tabstop = 4` (number, buffer-local) route
// through assign_option to the option store (option.c:set_option_value).
#[test]
fn let_assigns_options_through_ampersand() {
    let (editor, buffer, window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    // Boolean option via `:let &number = 1` → window-local store.
    exec.execute_line(&editor, "let &number = 1").unwrap();
    assert_eq!(
        editor
            .editor()
            .options()
            .get_window(window, "number")
            .unwrap(),
        &crate::options::OptionValue::Boolean(true)
    );
    // Number option via `:let &tabstop = 4` → buffer-local store.
    exec.execute_line(&editor, "let &tabstop = 4").unwrap();
    assert_eq!(
        editor
            .editor()
            .options()
            .get_buffer(buffer, "tabstop")
            .unwrap(),
        &crate::options::OptionValue::Number(4)
    );
}

// eval/vars.c ex_let_option — compound operators on option references.
// `..=` claims both dots (the legacy single-dot `.` form behaves the
// same), concatenates string options, and `+=`/`-=` do arithmetic on
// number options. Cross-kind use raises E734 before any state changes.
#[test]
fn let_option_compound_concatenates_dot_dot_equals() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let &runtimepath = '/first'")
        .unwrap();
    exec.execute_line(&editor, "let &runtimepath ..= ',/second'")
        .unwrap();
    assert_eq!(
        editor.editor().options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String("/first,/second".to_owned())
    );
    // Legacy single-dot form has identical behavior.
    exec.execute_line(&editor, "let &runtimepath .= ',/third'")
        .unwrap();
    assert_eq!(
        editor.editor().options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String("/first,/second,/third".to_owned())
    );
}

// eval/vars.c ex_let_option — `+=` on a number option adds through the
// option layer.
#[test]
fn let_option_compound_adds_number_option() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let &tabstop = 4").unwrap();
    exec.execute_line(&editor, "let &tabstop += 2").unwrap();
    assert_eq!(
        editor
            .editor()
            .options()
            .get_buffer(buffer, "tabstop")
            .unwrap(),
        &crate::options::OptionValue::Number(6)
    );
}

// eval/vars.c ex_let_option — a `.` operator on a number option and a
// `+` operator on a string option both raise E734 without writing.
#[test]
fn let_option_compound_rejects_wrong_kind_with_e734() {
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    let error = exec
        .execute_line(&editor, "let &tabstop ..= 'x'")
        .unwrap_err();
    match error {
        ExecError::Vim(exception) => {
            assert_eq!(exception.kind, VimExceptionKind::Error("E734".to_owned()));
        }
        other => panic!("expected Vim E734, got {other:?}"),
    }
    let error = exec
        .execute_line(&editor, "let &runtimepath += '/x'")
        .unwrap_err();
    match error {
        ExecError::Vim(exception) => {
            assert_eq!(exception.kind, VimExceptionKind::Error("E734".to_owned()));
        }
        other => panic!("expected Vim E734, got {other:?}"),
    }
    // The rejected writes left the stored values untouched.
    assert_eq!(
        editor.editor().options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String(String::new())
    );
}

// eval/vars.c ex_let — `..=` also compounds plain variables with string
// concatenation.
#[test]
fn let_variable_compound_dot_dot_equals_concatenates() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"let g:parts = "a""#).unwrap();
    exec.execute_line(&editor, r#"let g:parts ..= "b""#)
        .unwrap();
    assert_eq!(
        *exec
            .scope()
            .get_scoped(ScopeKind::Global, b"parts", 0)
            .unwrap(),
        Typval::String(OxStr::from("ab"))
    );
}

// ex_docmd.c:ex_let with `@r` target — `:let @a = "text"` stores
// characterwise content into the editor register (register.c:set_register).
#[test]
fn let_assigns_register_through_at() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"let @a = "text""#).unwrap();
    let ed = editor.editor();

    let content = ed.registers().get('a').unwrap().unwrap();
    assert_eq!(content.to_bytes(), b"text");
}

// ex_docmd.c:ex_let with `$VAR` target — `:let $VAR = "value"` stores
// into the scope's env map (eval.c:env_setvar).
#[test]
fn let_assigns_environment_variable_through_dollar() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"let $OXVIM_TEST_ENV = "value""#)
        .unwrap();
    let value = exec.scope().get_env(b"OXVIM_TEST_ENV");
    assert_eq!(value, Typval::String(OxStr::from("value")));
}

// ── set / setlocal / setglobal ─────────────────────────────────────────

// option.c:set_option_value — `:set number` sets boolean true; `:set
// nonumber` sets it false.  Both go through the window-local layer.
#[test]
fn set_boolean_toggle_on_and_off() {
    let (editor, _, window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "set number").unwrap();
    assert_eq!(
        editor
            .editor()
            .options()
            .get_window(window, "number")
            .unwrap(),
        &crate::options::OptionValue::Boolean(true)
    );
    exec.execute_line(&editor, "set nonumber").unwrap();
    assert_eq!(
        editor
            .editor()
            .options()
            .get_window(window, "number")
            .unwrap(),
        &crate::options::OptionValue::Boolean(false)
    );
}

#[test]
fn setlocal_modified_updates_buffer_state() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    editor.editor_mut().buffer_mut(buffer).unwrap().mark_saved();

    exec.execute_line(&editor, "setlocal modified").unwrap();

    assert!(
        editor
            .editor()
            .buffer(buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
    editor.editor_mut().sync_buffer_undo(buffer);
    editor
        .editor_mut()
        .replace_buffer_lines(LineReplaceRequest {
            buffer,
            start: 1,
            end: 1,
            lines: &[b"changed".to_vec()],
            cursor_before: Position { lnum: 1, col: 0 },
            cursor_after: Position { lnum: 1, col: 0 },
            timestamp: 0,
        })
        .unwrap();
    editor.editor_mut().sync_buffer_undo(buffer);
    assert!(
        editor
            .editor()
            .buffer(buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
}

// ex_docmd.c:6522-6551 with test/functional/api/buffer_spec.lua:490-503 —
// `new | wincmd w | setlocal modified`: the parser ends `wincmd` after its
// `w` key, `setlocal` runs as the next ordered instruction, and the option
// bridge marks the buffer of the window `wincmd w` switched to, leaving
// both windows' cursor and topline untouched.
#[test]
fn wincmd_w_chain_executes_setlocal_in_the_switched_window() {
    let (editor, buffer, original) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    editor.editor_mut().buffer_mut(buffer).unwrap().mark_saved();
    let original_cursor = editor.editor().window(original).unwrap().cursor;
    let original_topline = editor.editor().window(original).unwrap().topline;

    exec.execute_line(&editor, "new | wincmd w | setlocal modified")
        .unwrap();

    // `wincmd w` wrapped from the `:new` window back to the original one.
    assert_eq!(editor.editor().current_window(), Some(original));
    let tab = editor.editor().current_tabpage().unwrap();
    let windows = editor.editor().tabpage_windows(tab).unwrap();
    assert_eq!(windows.len(), 2);
    let new_window = *windows.iter().find(|window| **window != original).unwrap();
    let new_buffer = editor.editor().window(new_window).unwrap().buffer;
    assert_ne!(new_buffer, buffer);
    assert!(
        editor
            .editor()
            .buffer(buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
    assert!(
        !editor
            .editor()
            .buffer(new_buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
    // A flag-only change moves no window state.
    assert_eq!(
        editor.editor().window(original).unwrap().cursor,
        original_cursor
    );
    assert_eq!(
        editor.editor().window(original).unwrap().topline,
        original_topline
    );
    assert_eq!(
        editor.editor().window(new_window).unwrap().cursor,
        Position { lnum: 1, col: 0 }
    );
    assert_eq!(editor.editor().window(new_window).unwrap().topline, 1);
}

// check_nextcmd skips spaces and tabs before the bar (ex_docmd.c:4632) and
// skipwhite accepts trailing whitespace after the key (ex_docmd.c:6540), so
// whitespace never turns the tail into garbage nor hides the next command.
#[test]
fn wincmd_chain_survives_whitespace_around_the_separator() {
    for line in [
        "new | wincmd w | setlocal modified",
        "new | wincmd w\t|\tsetlocal modified",
        "new | wincmd w \t|\t setlocal modified",
    ] {
        let (editor, buffer, original) = editor_with_window();

        let editor = TestEditorAccess::new(editor);
        let mut exec = ExExecutor::new();
        editor.editor_mut().buffer_mut(buffer).unwrap().mark_saved();
        exec.execute_line(&editor, line).unwrap();
        assert_eq!(editor.editor().current_window(), Some(original), "{line:?}");
        assert!(
            editor
                .editor()
                .buffer(buffer)
                .unwrap()
                .flags
                .contains(crate::BufferFlags::MODIFIED),
            "{line:?}"
        );
    }
}

// ex_docmd.c:6541-6542 — text after the key form that is neither whitespace
// nor a comment is E474: the window command does not run and the rest of
// the line never executes.
#[test]
fn wincmd_garbage_tail_is_e474_and_stops_the_line() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    editor.editor_mut().buffer_mut(buffer).unwrap().mark_saved();

    assert_vim_error(exec.execute_line(&editor, "wincmd w garbage"), "E474");
    assert_vim_error(
        exec.execute_line(&editor, "wincmd w garbage | setlocal modified"),
        "E474",
    );
    assert!(
        !editor
            .editor()
            .buffer(buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
}

// ex_docmd.c:6541 — a `"` after the key form begins a comment: accepted,
// and a bar inside the comment never becomes a tail.
#[test]
fn wincmd_comment_tail_is_accepted_and_hides_the_bar() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    editor.editor_mut().buffer_mut(buffer).unwrap().mark_saved();

    exec.execute_line(&editor, r#"wincmd w " note | setlocal modified"#)
        .unwrap();
    assert!(
        !editor
            .editor()
            .buffer(buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
}

// ex_docmd.c:6527-6532 — the g/Ctrl-G forms consume a second key: `gT`
// keeps its second byte and the tail splits after it; the unimplemented
// g-form is E474 and stops the tail. The bare `wincmd g` is E474 too.
#[test]
fn wincmd_two_key_form_gates_the_tail_on_the_window_command() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    editor.editor_mut().buffer_mut(buffer).unwrap().mark_saved();

    assert_vim_error(exec.execute_line(&editor, "wincmd g"), "E474");
    assert_vim_error(
        exec.execute_line(&editor, "wincmd gT | setlocal modified"),
        "E474",
    );
    assert!(
        !editor
            .editor()
            .buffer(buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
}

// A literal `|` is the window-command key itself: only the second bar
// separates the next command, so the key is dispatched (and rejected as
// unimplemented, naming the key) instead of being read as trailing garbage.
#[test]
fn wincmd_pipe_key_consumes_the_first_bar_only() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    editor.editor_mut().buffer_mut(buffer).unwrap().mark_saved();

    let ExecError::Vim(exception) = exec
        .execute_line(&editor, "wincmd | | setlocal modified")
        .unwrap_err()
    else {
        panic!("expected E474 for the pipe key")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E474".to_owned()));
    assert!(
        exception.message().contains("Invalid argument: |"),
        "{}",
        exception.message()
    );
    assert!(
        !editor
            .editor()
            .buffer(buffer)
            .unwrap()
            .flags
            .contains(crate::BufferFlags::MODIFIED)
    );
}

// option.c:set_option_value — `:set tabstop=4` writes a number value
// to the buffer-local option overlay.
#[test]
fn set_number_option_with_equals() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "set tabstop=4").unwrap();
    assert_eq!(
        editor
            .editor()
            .options()
            .get_buffer(buffer, "tabstop")
            .unwrap(),
        &crate::options::OptionValue::Number(4)
    );
}

// option.c:set_option_value — `:set background=light` writes a string
// to the global option store.
#[test]
fn set_global_string_option() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "set background=light").unwrap();
    assert_eq!(
        editor.editor().options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("light".into())
    );
}

// option.c:do_set — both `&` and `&vim` restore the option's Vim default.
#[test]
fn set_ampersand_forms_restore_declared_default() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "set background=light").unwrap();
    exec.execute_line(&editor, "set background&vim").unwrap();
    assert_eq!(
        editor.editor().options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("dark".into())
    );

    exec.execute_line(&editor, "set background=light").unwrap();
    exec.execute_line(&editor, "set background&").unwrap();
    assert_eq!(
        editor.editor().options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("dark".into())
    );
}

// option.c:show_one — `:set number?` emits an Echo message with the
// current effective value and no message history.
#[test]
fn set_query_produces_echo_message() {
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "set number?").unwrap();
    let ed = editor.editor();

    let msgs = ed.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind, MessageKind::Echo);
    assert!(!msgs[0].history);
    // Default for `number` is false → "nonumber".
    assert_eq!(message_text(&msgs[0]), "nonumber");
}

#[test]
fn set_reset_restores_macro_backed_grepformat_default() {
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "set grepformat=custom").unwrap();
    exec.execute_line(&editor, "set grepformat&").unwrap();

    assert_eq!(
        editor.editor().options().get_global("grepformat").unwrap(),
        &crate::OptionValue::String("%f:%l:%m,%f:%l%m,%f  %l%m".to_owned())
    );
}

#[test]
fn set_reset_restores_macro_backed_cpo_default() {
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "set cpo=a").unwrap();
    exec.execute_line(&editor, "set cpo&").unwrap();

    assert_eq!(
        editor.editor().options().get_global("cpoptions").unwrap(),
        &crate::OptionValue::String("aABceFs_".to_owned())
    );
}

// option.c:set_option_value, E518 — `:set` on an unknown option name
// raises E518 "Unknown option".
#[test]
fn set_unknown_option_produces_e518() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    let err = exec.execute_line(&editor, "set notanoption").unwrap_err();
    match err {
        ExecError::Vim(vim_exc) => {
            assert_eq!(vim_exc.kind, VimExceptionKind::Error("E518".to_owned()));
            assert!(vim_exc.message().contains("Unknown option"));
        }
        other => panic!("expected Vim E518, got {other:?}"),
    }
}

// option.c:set_option_value, setglobal / setlocal routing —
// `:setglobal background=dark` writes the global layer; `:setlocal number`
// writes the window-local layer.
#[test]
fn setlocal_and_setglobal_route_to_correct_layer() {
    let (editor, _, window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "setglobal background=dark")
        .unwrap();
    assert_eq!(
        editor.editor().options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("dark".into())
    );
    exec.execute_line(&editor, "setlocal number").unwrap();
    assert_eq!(
        editor
            .editor()
            .options()
            .get_window(window, "number")
            .unwrap(),
        &crate::options::OptionValue::Boolean(true)
    );
}

#[test]
fn enew_selects_a_distinct_empty_buffer() {
    let (editor, original, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "enew").unwrap();

    let current = editor.editor().current_buffer().unwrap();
    assert_ne!(current, original);
    assert_eq!(
        editor
            .editor()
            .buffer(current)
            .unwrap()
            .text()
            .unwrap()
            .line_count(),
        1
    );
}

// ── echo / echomsg / echon ─────────────────────────────────────────────

#[test]
fn global_menu_cleanup_succeeds_for_empty_menu_state() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "aunmenu *").unwrap();
    exec.execute_line(&editor, "tlunmenu *").unwrap();
    assert!(matches!(
        exec.execute_line(&editor, "aunmenu File.Open"),
        Err(crate::ExecError::NotImplemented(command)) if command == "aunmenu"
    ));
}

// ex_docmd.c:ex_echo — `:echo "hello"` produces an Echo message without
// retaining literal string quotes and without message history.
#[test]
fn echo_produces_unsaved_unquoted_message() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"echo "hello""#).unwrap();
    let ed = editor.editor();

    let msgs = ed.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind, MessageKind::Echo);
    assert!(!msgs[0].history);
    assert_eq!(message_text(&msgs[0]), "hello");
}

// ex_docmd.c:ex_echo, echomsg — `:echomsg` produces an Echo message
// with history=true (enters message history).
#[test]
fn echomsg_produces_message_with_history() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"echomsg "hello""#).unwrap();
    let ed = editor.editor();

    let msgs = ed.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind, MessageKind::Echo);
    assert!(msgs[0].history);
    // `:echomsg` displays a String's contents without literal quotes.
    assert_eq!(message_text(&msgs[0]), "hello");
}

// ex_docmd.c:ex_echo, echon — `:echon` joins pieces with no separator
// (empty string), unlike `:echo` which uses a space.
#[test]
fn echon_joins_without_space_separator() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"echon "abc" "def""#).unwrap();
    let ed = editor.editor();

    let msgs = ed.messages();
    assert_eq!(msgs.len(), 1);
    // echon separator is "" → "abcdef".
    assert_eq!(message_text(&msgs[0]), "abcdef");
}

// ── execute ────────────────────────────────────────────────────────────

#[test]
fn set_minus_equal_removes_complete_comma_list_item() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "set wildoptions-=pum").unwrap();
    assert_eq!(
        editor.editor().options().get_global("wildoptions").unwrap(),
        &OptionValue::String("tagfile".to_owned())
    );
}

// ex_docmd.c:ex_execute — `:execute` evaluates expression arguments,
// joins the resulting strings, and runs the joined text as a command.
#[test]
fn execute_evaluates_and_runs_string_as_command() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, r#"execute "let g:execvar = 99""#)
        .unwrap();
    let value = exec
        .scope()
        .get_scoped(ScopeKind::Global, b"execvar", 0)
        .unwrap();
    assert_eq!(*value, Typval::Number(99));
}

#[test]
fn execute_keeps_spaced_operators_inside_each_expression() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "<execute-concat>",
        "function! Mark(value)\nlet g:marked = a:value\nendfunction\n\
         let g:name = 'Mark'\nexecute 'call ' . g:name . '(' 7 ')'",
    )
    .unwrap();

    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"marked", 0),
        Ok(&Typval::Number(7)),
    );
}

#[test]
fn lcd_without_a_current_window_is_atomic_e16() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-no-window");
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    assert_vim_error(
        exec.execute_line(&editor, &format!("lcd {}", fixture.directory.display())),
        "E16",
    );
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn window_local_directory_survives_buffer_replacement() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-buffer-replacement");
    let displaced = fixture.child("displaced");
    let (editor, original_buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, &format!("lcd {}", fixture.directory.display()))
        .unwrap();
    std::env::set_current_dir(displaced).unwrap();
    exec.execute_line(&editor, "enew").unwrap();
    assert_eq!(std::env::current_dir().unwrap(), fixture.directory);
    exec.execute_line(&editor, "bdelete").unwrap();

    assert_eq!(editor.editor().current_buffer(), Some(original_buffer));
    assert_eq!(std::env::current_dir().unwrap(), fixture.directory);
}

#[test]
fn switching_between_local_and_global_windows_restores_destination_directory() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-window-switch");
    let (editor, _, global_window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "new").unwrap();
    let local_window = editor.editor().current_window().unwrap();
    exec.execute_line(&editor, &format!("lcd {}", fixture.directory.display()))
        .unwrap();
    assert_eq!(std::env::current_dir().unwrap(), fixture.directory);

    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(global_window));
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);

    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(local_window));
    assert_eq!(std::env::current_dir().unwrap(), fixture.directory);
}

#[test]
fn windows_maintain_independent_lcd_minus_history() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-window-history");
    let first = fixture.child("first");
    let second = fixture.child("second");
    let third = fixture.child("third");
    let (editor, _, original_window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "new").unwrap();
    let other_window = editor.editor().current_window().unwrap();
    exec.execute_line(&editor, &format!("lcd {}", first.display()))
        .unwrap();
    exec.execute_line(&editor, &format!("lcd {}", second.display()))
        .unwrap();

    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(original_window));
    exec.execute_line(&editor, &format!("lcd {}", third.display()))
        .unwrap();

    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(other_window));
    exec.execute_line(&editor, "lcd -").unwrap();
    assert_eq!(std::env::current_dir().unwrap(), first);

    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(original_window));
    exec.execute_line(&editor, "lcd -").unwrap();
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn closing_current_local_window_restores_destination_directory() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-close-window");
    let (editor, _, destination) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "new").unwrap();
    exec.execute_line(&editor, &format!("lcd {}", fixture.directory.display()))
        .unwrap();
    exec.execute_line(&editor, "close").unwrap();

    assert_eq!(editor.editor().current_window(), Some(destination));
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn closing_current_local_tabpage_restores_destination_directory() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-close-tabpage");
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let destination = editor.editor().current_tabpage().unwrap();
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "tabnew").unwrap();
    exec.execute_line(&editor, &format!("lcd {}", fixture.directory.display()))
        .unwrap();

    // Close through the editor path, not the Ex command that masks the
    // missing reapply by calling set_current_tabpage afterward.
    let current = editor.editor().current_tabpage().unwrap();
    editor.editor_mut().close_tabpage(current).unwrap();

    assert_eq!(editor.editor().current_tabpage(), Some(destination));
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn chdir_coerces_numeric_scope_before_rejecting_it_with_e475() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("chdir-numeric-scope");
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    let error = exec
        .execute_line(
            &editor,
            &format!("let g:left = chdir('{}', 0)", fixture.directory.display()),
        )
        .unwrap_err();
    let ExecError::Vim(exception) = error else {
        panic!("expected a Vim error")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E475".to_owned()));
    assert_eq!(
        exception.message(),
        "Vim(let):E475: Invalid value for argument scope: 0"
    );
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn switching_to_removed_local_directory_keeps_prior_process_directory() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-removed-window");
    let removed = fixture.child("removed");
    let (editor, _, destination) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "new").unwrap();
    let local_window = editor.editor().current_window().unwrap();
    exec.execute_line(&editor, &format!("lcd {}", removed.display()))
        .unwrap();
    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(destination));
    std::fs::remove_dir(&removed).unwrap();
    let prior = std::env::current_dir().unwrap();

    // Upstream changes the window even when its saved directory no longer exists.
    exec.execute_line(&editor, "wincmd w").unwrap();

    assert_eq!(editor.editor().current_window(), Some(local_window));
    assert_eq!(std::env::current_dir().unwrap(), prior);
}

#[test]
fn fresh_executors_share_editor_global_cd_minus_history() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("shared-directory-state");
    let editor = TestEditorAccess::new(Editor::new());
    let mut first = ExExecutor::new();
    let mut second = ExExecutor::new();

    first
        .execute_line(&editor, &format!("cd {}", fixture.directory.display()))
        .unwrap();
    second.execute_line(&editor, "cd -").unwrap();

    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn lcd_does_not_disturb_global_cd_minus_history() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-global-history");
    let local = fixture.child("local");
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, &format!("cd {}", fixture.directory.display()))
        .unwrap();
    exec.execute_line(&editor, &format!("lcd {}", local.display()))
        .unwrap();
    exec.execute_line(&editor, "cd -").unwrap();

    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn split_inherits_source_window_local_history() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-split-history");
    let first = fixture.child("first");
    let second = fixture.child("second");
    let source_after_split = fixture.child("source-after-split");
    let (editor, _, source_window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, &format!("lcd {}", first.display()))
        .unwrap();
    exec.execute_line(&editor, &format!("lcd {}", second.display()))
        .unwrap();
    exec.execute_line(&editor, "new").unwrap();
    let split_window = editor.editor().current_window().unwrap();
    assert_eq!(std::env::current_dir().unwrap(), second);

    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(source_window));
    exec.execute_line(&editor, &format!("lcd {}", source_after_split.display()))
        .unwrap();

    exec.execute_line(&editor, "wincmd w").unwrap();
    assert_eq!(editor.editor().current_window(), Some(split_window));
    exec.execute_line(&editor, "lcd -").unwrap();
    assert_eq!(std::env::current_dir().unwrap(), first);
}

#[test]
fn tabnew_inherits_source_window_local_history() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("lcd-tabnew-history");
    let first = fixture.child("first");
    let second = fixture.child("second");
    let (editor, _, source_window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, &format!("lcd {}", first.display()))
        .unwrap();
    exec.execute_line(&editor, &format!("lcd {}", second.display()))
        .unwrap();
    exec.execute_line(&editor, "tabnew").unwrap();
    let new_tab = editor.editor().current_tabpage().unwrap();
    let new_window = editor.editor().current_window().unwrap();

    assert_eq!(
        editor.editor().window_local_directory(new_window).unwrap(),
        Some(second.clone())
    );
    assert_eq!(
        editor.editor().previous_directory(DirectoryScope::Window),
        Some(first.clone())
    );
    assert_eq!(
        editor
            .editor()
            .window_local_directory(source_window)
            .unwrap(),
        Some(second.clone())
    );

    // Displace the process cwd so a stale effective-directory cache cannot
    // make the reapply assertion pass accidentally.
    std::env::set_current_dir(&fixture.original).unwrap();
    editor.editor_mut().set_current_tabpage(new_tab).unwrap();
    assert_eq!(std::env::current_dir().unwrap(), second);
    assert_eq!(editor.editor().current_window(), Some(new_window));
}

#[test]
fn direct_directory_change_is_global_and_preserves_cd_minus() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("direct-cd");
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.change_directory(&editor, fixture.directory.to_str().unwrap())
        .unwrap();
    assert_eq!(std::env::current_dir().unwrap(), fixture.directory);

    exec.execute_line(&editor, "cd -").unwrap();
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
}

#[test]
fn direct_directory_change_reuses_cd_e344() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = CwdFixture::new("direct-cd-error");
    let missing = fixture.directory.join("missing");
    let missing = missing.to_str().unwrap();
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    let ExecError::Eval(error) = exec.change_directory(&editor, missing).unwrap_err() else {
        panic!("expected direct directory change to return an evaluation error")
    };
    assert_eq!(error.code, "E344");
    assert!(
        error
            .message
            .starts_with(&format!("Can't find directory {missing}:"))
    );
    assert_eq!(std::env::current_dir().unwrap(), fixture.original);
    assert_vim_error(exec.execute_line(&editor, &format!("cd {missing}")), "E344");
}

#[test]
fn cd_changes_the_directory_observed_by_getcwd() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original = std::env::current_dir().unwrap();
    let target = std::env::temp_dir().join(format!("ox-editor-cd-{}", std::process::id()));
    std::fs::create_dir_all(&target).unwrap();

    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    let result = exec.execute_line(&editor, &format!("cd {}", target.display()));
    let changed = std::env::current_dir();
    std::env::set_current_dir(&original).unwrap();
    std::fs::remove_dir(&target).unwrap();

    result.unwrap();
    assert_eq!(changed.unwrap(), target);
}

#[test]
fn cd_minus_toggles_and_returns_previous_directory() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original = std::env::current_dir().unwrap();
    let target = std::env::temp_dir().join(format!("ox-editor-cd-{}", std::process::id()));
    std::fs::create_dir_all(&target).unwrap();

    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, &format!("cd {}", target.display()))
        .unwrap();
    assert_eq!(std::env::current_dir().unwrap(), target);
    exec.execute_line(&editor, "let g:before = chdir('-')")
        .unwrap();
    assert_eq!(std::env::current_dir().unwrap(), original);
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"before", 0),
        Ok(&Typval::String(OxStr::from(
            target.to_string_lossy().as_ref()
        ))),
    );
    std::fs::remove_dir(&target).unwrap();
}

/// `:cd` back out of a working directory that has been deleted underneath the
/// process still works, and the directory it left behind is reported as empty.
///
/// `changedir_func` (`ex_docmd.c`:6308-6312) reads the old directory with
/// `os_dirname` purely to record it, and carries on when that fails. Refusing
/// the move instead strands the process: `runtest.vim` saves `getcwd()`, runs a
/// test that deletes its own directory, and restores with
/// `exe 'cd ' . save_cwd`. When that restore is refused, `FinishTesting`'s
/// write of the relative `test.log` dies with E212 and the whole file's results
/// go with it, which is what `test_alot.vim` and `test_expand.vim` did.
#[test]
fn cd_out_of_a_deleted_directory_still_moves() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original = std::env::current_dir().unwrap();
    let target = std::env::temp_dir().join(format!("ox-editor-cd-gone-{}", std::process::id()));
    std::fs::create_dir_all(&target).unwrap();

    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, &format!("cd {}", target.display()))
        .unwrap();
    // The process is now standing in a directory that no longer exists.
    std::fs::remove_dir(&target).unwrap();
    let result = exec.execute_line(&editor, &format!("cd {}", original.display()));
    let restored = std::env::current_dir();
    // Put the process back before asserting, whatever happened.
    std::env::set_current_dir(&original).unwrap();

    result.unwrap();
    assert_eq!(restored.unwrap(), original);

    // `chdir()` reports the directory it left, and an unreadable one is the
    // empty string rather than a refusal (`f_chdir`).
    std::fs::create_dir_all(&target).unwrap();
    exec.execute_line(&editor, &format!("cd {}", target.display()))
        .unwrap();
    std::fs::remove_dir(&target).unwrap();
    exec.execute_line(
        &editor,
        &format!("let g:left = chdir('{}')", original.display()),
    )
    .unwrap();
    std::env::set_current_dir(&original).unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"left", 0),
        Ok(&Typval::String(OxStr::from(""))),
    );
}

#[test]
fn buffer_identity_builtins_resolve_current_and_named_buffers() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    editor
        .editor_mut()
        .buffer_mut(buffer)
        .unwrap()
        .set_name(OxStr::from("named.vim"));
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &editor,
        "<buffer-identity>",
        "let g:current_name = bufname()\nlet g:named_number = bufnr('named.vim')\nlet g:named_exists = bufexists('named.vim')\nlet g:missing_exists = bufexists('missing.vim')",
    )
    .unwrap();

    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"current_name", 0),
        Ok(&Typval::String(OxStr::from("named.vim"))),
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"named_number", 0),
        Ok(&Typval::Number(i64::from(buffer))),
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"named_exists", 0),
        Ok(&Typval::Number(1)),
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"missing_exists", 0),
        Ok(&Typval::Number(0)),
    );
}

#[test]
fn execute_builtin_captures_nested_command_output() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "let g:swap = execute('swapname')")
        .unwrap();

    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"swap", 0),
        Ok(&Typval::String(OxStr::from("\nNo swap file"))),
    );
    assert!(editor.editor().messages().is_empty());
}

// ── normal ─────────────────────────────────────────────────────────────

// ex_docmd.c:ex_normal — `:normal` requires keys and completes after queuing them.
#[test]
fn normal_with_key_args_completes() {
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    let result = exec.execute_line(&editor, "normal gg");
    assert_eq!(result.unwrap(), ExecOutcome::Completed);
}

// ── marks / registers output ───────────────────────────────────────────

// ex_docmd.c:ex_mark, `:marks` — outputs a header line followed by one
// line per mark with the mark name, line, and column.
#[test]
fn marks_outputs_header_and_mark_lines() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    editor
        .editor_mut()
        .set_local_mark(buffer, 'a', Position { lnum: 3, col: 2 })
        .unwrap();
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "marks").unwrap();
    let ed = editor.editor();

    let msgs = ed.messages();
    assert!(msgs.len() >= 2);
    assert_eq!(message_text(&msgs[0]), "mark line  col file/text");
    // Mark 'a' at line 3, col 2 → " a     3    2".
    let mark_line = message_text(&msgs[1]);
    assert!(mark_line.starts_with(" a"));
    assert!(mark_line.contains('3'));
    assert!(mark_line.contains('2'));
}

#[test]
fn delmarks_removes_named_ranges_and_special_marks() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    for name in ['a', 'b', 'c', 'z', '"', '^', ':', '.', '[', ']'] {
        editor
            .editor_mut()
            .set_local_mark(buffer, name, Position { lnum: 1, col: 0 })
            .unwrap();
    }
    for name in ['0', '1', 'A', 'B'] {
        editor
            .editor_mut()
            .global_marks_mut()
            .set(
                name,
                crate::MarkLocation::in_buffer(buffer, Position { lnum: 1, col: 0 }),
            )
            .unwrap();
    }
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "delmarks b-z 0-1 A-B ^.[]:<>\\\"")
        .unwrap();

    assert!(editor.editor().local_mark(buffer, 'a').unwrap().is_some());
    for name in ['b', 'c', 'z', '"', '^', '.', '[', ']'] {
        assert_eq!(
            editor.editor().local_mark(buffer, name).unwrap(),
            None,
            "{name}"
        );
    }
    assert!(editor.editor().local_mark(buffer, ':').unwrap().is_some());
    for name in ['0', '1', 'A', 'B'] {
        assert_eq!(
            editor.editor().global_marks().get(name).unwrap(),
            None,
            "{name}"
        );
    }
}

#[test]
fn delmarks_keeps_prior_deletions_when_a_later_range_is_invalid() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    for name in ['a', 'b'] {
        editor
            .editor_mut()
            .set_local_mark(buffer, name, Position { lnum: 1, col: 0 })
            .unwrap();
    }
    let mut exec = ExExecutor::new();

    assert_vim_error(exec.execute_line(&editor, "delmarks a z-b"), "E475");

    assert_eq!(editor.editor().local_mark(buffer, 'a').unwrap(), None);
    assert!(editor.editor().local_mark(buffer, 'b').unwrap().is_some());
}

#[test]
fn delmarks_validates_bang_and_required_arguments() {
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    assert_vim_error(exec.execute_line(&editor, "delmarks"), "E471");
    assert_vim_error(exec.execute_line(&editor, "delmarks /"), "E475");
    assert_vim_error(exec.execute_line(&editor, "delmarks! x"), "E474");
}

#[test]
fn delmarks_bang_clears_local_marks_and_changelist_only() {
    let (editor, buffer, window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    for name in ['a', 'z', '"', '^', ':', '.', '[', ']'] {
        editor
            .editor_mut()
            .set_local_mark(buffer, name, Position { lnum: 1, col: 0 })
            .unwrap();
    }
    editor
        .editor_mut()
        .global_marks_mut()
        .set(
            'A',
            crate::MarkLocation::in_buffer(buffer, Position { lnum: 1, col: 0 }),
        )
        .unwrap();
    editor
        .editor_mut()
        .replace_buffer_lines(LineReplaceRequest {
            buffer,
            start: 1,
            end: 1,
            lines: &[b"changed".to_vec()],
            cursor_before: Position { lnum: 1, col: 0 },
            cursor_after: Position { lnum: 1, col: 0 },
            timestamp: 1,
        })
        .unwrap();
    assert!(!editor.editor().changelists().is_empty(buffer));
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "delmarks!").unwrap();

    for name in ['a', 'z', '"', '^', '.', '[', ']'] {
        assert_eq!(
            editor.editor().local_mark(buffer, name).unwrap(),
            None,
            "{name}"
        );
    }
    assert!(editor.editor().local_mark(buffer, ':').unwrap().is_some());
    assert!(editor.editor().global_marks().get('A').unwrap().is_some());
    assert!(editor.editor().changelists().is_empty(buffer));
    assert_eq!(editor.editor().window(window).unwrap().buffer, buffer);
}

// ex_docmd.c:ex_registers, `:registers` — outputs one line per non-empty
// register in the requested set, formatted as `"x   content`.
#[test]
fn registers_outputs_register_listing() {
    let editor = TestEditorAccess::new(Editor::new());
    editor
        .editor_mut()
        .registers_mut()
        .set('a', RegisterContent::characterwise(b"hello").unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "registers a").unwrap();
    let ed = editor.editor();

    let msgs = ed.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(message_text(&msgs[0]), "\"a   hello");
}

// ── highlight storage ──────────────────────────────────────────────────

// ex_docmd.c:ex_highlight — `:highlight Group key=value` stores the
// attribute map; `:highlight clear Group` removes it.
#[test]
fn highlight_stores_and_clears_group_attributes() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "highlight MyGroup guifg=red")
        .unwrap();
    {
        let ed = editor.editor();
        let attrs = ed.highlights().get("MyGroup").unwrap();
        assert_eq!(attrs.get("guifg").unwrap(), "red");
    }
    exec.execute_line(&editor, "highlight clear MyGroup")
        .unwrap();
    assert!(editor.editor().highlights().get("MyGroup").is_none());
}

#[test]
fn highlight_default_and_link_forms_retain_definitions() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "highlight Existing guifg=red")
        .unwrap();
    exec.execute_line(&editor, "highlight default Existing guifg=blue")
        .unwrap();
    exec.execute_line(&editor, "highlight default NewGroup cterm=bold")
        .unwrap();
    exec.execute_line(&editor, "highlight link Linked Existing")
        .unwrap();
    exec.execute_line(&editor, "highlight default link DefaultLinked NewGroup")
        .unwrap();

    assert_eq!(editor.editor().highlights()["Existing"]["guifg"], "red");
    assert_eq!(editor.editor().highlights()["NewGroup"]["cterm"], "bold");
    assert_eq!(editor.editor().highlights()["Linked"]["link"], "Existing");
    assert_eq!(
        editor.editor().highlights()["DefaultLinked"]["link"],
        "NewGroup"
    );
}

// ── unsupported-command NotImplemented identity ────────────────────────

// ex_docmd.c:do_one_cmd dispatch — a builtin command not in the handler
// table returns NotImplemented(name) rather than a silent no-op.
#[test]
fn unimplemented_builtin_returns_not_implemented() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    // ":sort" used to stand in here; it is dispatched now, so the probe
    // moved to ":move", which the handler table still does not carry.
    let err = exec.execute_line(&editor, "move").unwrap_err();
    match err {
        ExecError::NotImplemented(name) => assert_eq!(name, "move"),
        other => panic!("expected NotImplemented, got {other:?}"),
    }
}

// ── E121 / E488 line-numbered sample shapes ────────────────────────────

// eval.c:E121, ex_docmd.c:do_source throwpoint — `:let` without `=` on an
// undefined name raises E121 inside a sourced script; the throwpoint
// carries the script name and physical line number.
// Mirrors test_let.vim E121 samples and ex_docmd.c error formatting.
#[test]
fn e121_in_script_has_line_numbered_throwpoint() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    let script = "let g:line1 = 1\nlet g:nonexistent";
    let err = exec.execute_script(&editor, "test", script).unwrap_err();
    match err {
        ExecError::Vim(vim_exc) => {
            assert_eq!(vim_exc.kind, VimExceptionKind::Error("E121".to_owned()));
            assert!(
                vim_exc
                    .message()
                    .contains("Undefined variable: g:nonexistent")
            );
            // Throwpoint includes the script name and line 2.
            assert_eq!(vim_exc.throwpoint, "script test[2]");
        }
        other => panic!("expected Vim E121, got {other:?}"),
    }
}

// ex_docmd.c:ex_call, E488 — `:call` with trailing text after the closing
// parenthesis raises E488 "Trailing characters".  From the command line
// the throwpoint is "command line" (no script frame).
#[test]
fn e488_from_call_trailing_characters() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    let err = exec
        .execute_line(&editor, "call Foo()trailing")
        .unwrap_err();
    match err {
        ExecError::Vim(vim_exc) => {
            assert_eq!(vim_exc.kind, VimExceptionKind::Error("E488".to_owned()));
            // Oracle: `Vim(call):E488: Trailing characters: trailing`.
            // `ex_call` emits this itself, so `append_command` does not run on
            // it and the command line is *not* echoed after the remainder.
            assert_eq!(
                vim_exc.message(),
                "Vim(call):E488: Trailing characters: trailing"
            );
            assert_eq!(vim_exc.throwpoint, "command line");
        }
        other => panic!("expected Vim E488, got {other:?}"),
    }
}

// ex_call: `ends_excmd` accepts a `"` after the closing parenthesis, so a
// trailing comment is not "trailing characters" (userfunc.c:3615).
#[test]
fn call_allows_trailing_comment_after_closing_paren() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "test",
        "function Store()\n  let g:stored = 1\nendfunction",
    )
    .unwrap();
    exec.execute_line(&editor, "call Store()  \" comment here")
        .unwrap();
    exec.execute_line(&editor, "call Store()\"tight comment")
        .unwrap();
    assert!(exec.execute_line(&editor, "call Store()trailing").is_err());
}

// ── lua ────────────────────────────────────────────────────────────────

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Default)]
struct FakeLua {
    chunks: Vec<(String, Vec<Object>)>,
    files: Vec<PathBuf>,
    error: Option<LuaExecError>,
    evals: Vec<(String, Option<Typval>)>,
    eval_result: Option<Typval>,
}

impl LuaExec for FakeLua {
    fn execute_chunk(&mut self, code: &str, args: Vec<Object>) -> Result<Object, LuaExecError> {
        self.chunks.push((code.to_owned(), args.clone()));
        if let Some(error) = self.error.clone() {
            return Err(error);
        }
        match args.as_slice() {
            [Object::String(line), Object::Integer(lnum)] => Ok(Object::String(OxStr::from(
                format!("{}:{lnum}", line.to_string_lossy()).as_str(),
            ))),
            _ => Ok(Object::Nil),
        }
    }

    fn execute_file(&mut self, path: &Path) -> Result<(), LuaExecError> {
        self.files.push(path.to_path_buf());
        self.error.clone().map_or(Ok(()), Err)
    }

    fn eval_expression(
        &mut self,
        expression: &str,
        arg: Option<&Typval>,
    ) -> Result<Typval, LuaExecError> {
        self.evals.push((expression.to_owned(), arg.cloned()));
        if let Some(error) = self.error.clone() {
            return Err(error);
        }
        Ok(self.eval_result.clone().unwrap_or(Typval::Number(0)))
    }
}

fn lua_executor(host: Rc<RefCell<FakeLua>>) -> ExExecutor {
    let mut executor = ExExecutor::new();
    executor.set_lua_exec(host);
    executor
}

#[test]
fn lua_executes_exact_chunk() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone())
        .execute_line(&editor, "lua local x = 1 | 2")
        .unwrap();
    assert_eq!(
        host.borrow().chunks[0],
        ("local x = 1 | 2".to_owned(), Vec::new())
    );
}

#[test]
fn sourced_lua_heredoc_preserves_body_and_resumes_after_marker() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host.clone());

    executor
        .execute_script(
            &editor,
            "test.vim",
            "lua << END\n-- body comment\n END\n  trailing spaces  \nEND\nlet g:after_heredoc = 9",
        )
        .unwrap();

    assert_eq!(
        host.borrow().chunks[0],
        (
            "-- body comment\n END\n  trailing spaces  \n".to_owned(),
            Vec::new()
        ),
    );
    assert_eq!(
        executor
            .scope()
            .get_scoped(ScopeKind::Global, b"after_heredoc", 0)
            .unwrap(),
        &Typval::Number(9),
    );
}

#[test]
fn sourced_lua_trim_uses_first_nonempty_body_indent() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host.clone());

    executor
        .execute_script(
            &editor,
            "test.vim",
            "  :  lua << trim END\n\n      first\n    second\n  END",
        )
        .unwrap();

    assert_eq!(
        host.borrow().chunks[0],
        ("\nfirst\nsecond\n".to_owned(), Vec::new())
    );
}

#[test]
fn sourced_lua_heredoc_accepts_empty_body_and_default_dot_marker() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host.clone());

    executor
        .execute_script(&editor, "test.vim", "lua << END\nEND")
        .unwrap();
    executor
        .execute_script(&editor, "test.vim", "lua << \" default marker\nreturn 1\n.")
        .unwrap();

    assert_eq!(host.borrow().chunks[0].0, "");
    assert_eq!(host.borrow().chunks[1].0, "return 1\n");
}

#[test]
fn let_heredoc_assigns_trimmed_lines_as_list() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &editor,
            "test.vim",
            "  let g:lines =<< trim END\n\n      alpha\n    beta\n      \" text, not an Ex comment\n  END",
        )
        .unwrap();

    let Typval::List(lines) = executor
        .scope()
        .get_scoped(ScopeKind::Global, b"lines", 0)
        .unwrap()
    else {
        panic!("expected heredoc List");
    };
    let values = lines.borrow().items.clone();
    assert_eq!(
        values,
        vec![
            Typval::String(OxStr::from("")),
            Typval::String(OxStr::from("alpha")),
            Typval::String(OxStr::from("beta")),
            Typval::String(OxStr::from("\" text, not an Ex comment")),
        ],
    );
}

#[test]
fn let_heredoc_accepts_eval_before_trim() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &editor,
            "test.vim",
            "let g:lines =<< eval trim END\n  alpha\nEND",
        )
        .unwrap();

    assert_eq!(
        executor.scope().get_scoped(ScopeKind::Global, b"lines", 0),
        Ok(&Typval::list(vec![Typval::String(OxStr::from("alpha"))]))
    );
}

#[test]
fn let_heredoc_requires_end_marker() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    let error = executor
        .execute_script(&editor, "test.vim", "let g:lines =<< END\nmissing")
        .unwrap_err();
    assert!(error.to_string().contains("E990"));
    assert!(error.to_string().contains("END"));
}

#[test]
fn let_expression_containing_heredoc_text_does_not_consume_source_lines() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &editor,
            "test.vim",
            "let g:literal = 'a=<<b'\nlet g:after_literal = 4",
        )
        .unwrap();

    assert_eq!(
        executor
            .scope()
            .get_scoped(ScopeKind::Global, b"literal", 0)
            .unwrap(),
        &Typval::String(OxStr::from("a=<<b")),
    );
    assert_eq!(
        executor
            .scope()
            .get_scoped(ScopeKind::Global, b"after_literal", 0)
            .unwrap(),
        &Typval::Number(4),
    );
}

#[test]
#[ignore = "FakeLua mock can no longer mutate editor globals"]
fn lua_global_mutation_is_visible_to_following_ex_command() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host);
    executor
        .execute_line(&editor, "lua set-test-global")
        .unwrap();
    executor
        .execute_line(&editor, "unlet g:lua_global")
        .unwrap();
    assert!(
        editor
            .editor()
            .gvars()
            .get(&OxStr::from("lua_global"))
            .is_none()
    );
}

#[test]
fn luafile_executes_named_file() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone())
        .execute_line(&editor, "luafile runtime/colors/vim.lua")
        .unwrap();
    assert_eq!(
        host.borrow().files,
        [PathBuf::from("runtime/colors/vim.lua")]
    );
}

#[test]
fn luado_transforms_every_line_with_line_number() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    editor
        .editor_mut()
        .replace_buffer_lines(crate::LineReplaceRequest {
            buffer,
            start: 1,
            end: 1,
            lines: &[b"alpha".to_vec(), b"beta".to_vec()],
            cursor_before: ox_text::Position { lnum: 1, col: 0 },
            cursor_after: ox_text::Position { lnum: 1, col: 0 },
            timestamp: 0,
        })
        .unwrap();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone())
        .execute_line(&editor, "luado return line")
        .unwrap();
    let lines = (1..=2)
        .map(|lnum| {
            editor
                .editor()
                .buffer(buffer)
                .unwrap()
                .text()
                .unwrap()
                .line(lnum)
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(lines, [b"alpha:1".to_vec(), b"beta:2".to_vec()]);
    assert_eq!(host.borrow().chunks.len(), 2);
}

#[test]
fn lua_runtime_error_is_catchable_vim_error() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua {
        error: Some(LuaExecError::Runtime("boom".to_owned())),
        ..FakeLua::default()
    }));
    let error = lua_executor(host)
        .execute_line(&editor, "lua error('boom')")
        .unwrap_err();
    match error {
        ExecError::Vim(exception) => {
            assert_eq!(exception.kind, VimExceptionKind::Error("E5108".to_owned()));
            assert!(exception.message().contains("boom"));
        }
        other => panic!("expected Vim error, got {other:?}"),
    }
}

#[test]
fn put_expression_evaluates_and_inserts_expression_register() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:error = 'screen too small'")
        .unwrap();
    exec.execute_line(&editor, "$put =g:error").unwrap();

    let ed = editor.editor();

    let text = ed.buffer(buffer).unwrap().text().unwrap();
    assert_eq!(text.line(2).unwrap(), b"screen too small");
}

#[test]
fn writable_vim_variables_match_upstream_table() {
    for name in [
        "errmsg",
        "warningmsg",
        "statusmsg",
        "this_session",
        "fcs_choice",
        "scrollstart",
        "swapchoice",
        "char",
        "mouse_win",
        "mouse_winid",
        "mouse_lnum",
        "mouse_col",
        "searchforward",
        "hlsearch",
        "oldfiles",
        "completed_item",
        "errors",
        "testing",
    ] {
        assert!(vim_variable_is_writable(name.as_bytes()), "v:{name}");
    }
    for name in [
        "count",
        "count1",
        "dying",
        "register",
        "event",
        "servername",
    ] {
        assert!(!vim_variable_is_writable(name.as_bytes()), "v:{name}");
    }
}

#[test]
fn luaeval_passes_expression_and_argument_to_host() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua {
        eval_result: Some(Typval::Number(42)),
        ..FakeLua::default()
    }));
    let mut executor = lua_executor(host.clone());
    executor
        .execute_line(&editor, "let g:answer = luaeval('_A[1] + _A[2]', [40, 2])")
        .unwrap();
    assert_eq!(
        executor
            .scope()
            .get_scoped(ScopeKind::Global, b"answer", 0)
            .unwrap(),
        &Typval::Number(42),
    );
    let Typval::List(argument) = host.borrow().evals[0].1.clone().unwrap() else {
        panic!("expected list argument");
    };
    assert_eq!(
        argument.borrow().items,
        vec![Typval::Number(40), Typval::Number(2)],
    );
}

#[test]
fn luaeval_without_argument_passes_none_to_host() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone())
        .execute_line(&editor, "let g:solo = luaeval('pcall(require, \"ffi\")')")
        .unwrap();
    assert_eq!(host.borrow().evals[0].0, "pcall(require, \"ffi\")");
    assert!(host.borrow().evals[0].1.is_none());
}

#[test]
fn luaeval_without_host_stays_not_implemented() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut executor = ExExecutor::new();
    let error = executor
        .execute_line(&editor, "echo luaeval('1')")
        .unwrap_err();
    assert_eq!(error.to_string(), "not implemented: luaeval");
}

#[test]
fn luaeval_rejects_wrong_argument_counts() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host);
    let error = executor
        .execute_line(&editor, "echo luaeval()")
        .unwrap_err();
    assert!(error.to_string().contains("E119"), "{}", error);
    let error = executor
        .execute_line(&editor, "echo luaeval('1', 2, 3)")
        .unwrap_err();
    assert!(error.to_string().contains("E118"), "{}", error);
}

#[test]
fn luaeval_load_and_runtime_errors_use_upstream_codes() {
    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua {
        error: Some(LuaExecError::Load(
            "[string \"luaeval()\"]:1: syntax".to_owned(),
        )),
        ..FakeLua::default()
    }));
    let error = lua_executor(host)
        .execute_line(&editor, "echo luaeval('synta x')")
        .unwrap_err();
    let ExecError::Vim(exception) = error else {
        panic!("expected Vim error")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E5107".to_owned()));
    assert!(
        exception
            .message()
            .contains("Lua: [string \"luaeval()\"]:1: syntax")
    );

    let editor = TestEditorAccess::new(Editor::new());
    let host = Rc::new(RefCell::new(FakeLua {
        error: Some(LuaExecError::Runtime("boom".to_owned())),
        ..FakeLua::default()
    }));
    let error = lua_executor(host)
        .execute_line(&editor, "echo luaeval('error(1)')")
        .unwrap_err();
    let ExecError::Vim(exception) = error else {
        panic!("expected Vim error")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E5108".to_owned()));
    assert!(exception.message().contains("Lua: boom"));
}

#[test]
fn let_writes_upstream_writable_vim_variables_only() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "let v:testing = 1").unwrap();
    assert_eq!(
        editor.editor().vvars().get(&OxStr::from("testing")),
        Some(&Object::Integer(1))
    );

    let error = exec.execute_line(&editor, "let v:count = 2").unwrap_err();
    let ExecError::Vim(exception) = error else {
        panic!("expected E46")
    };
    assert_eq!(exception.kind, VimExceptionKind::Error("E46".to_owned()));
}

#[test]
fn assertions_record_failures_in_writable_v_errors_and_messages() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &editor,
        "assertions.vim",
        "let g:initial = len(v:errors)\n\
         let g:success = assert_equal([1, 2], [1, 2])\n\
         let g:failure = assert_equal('expected', 'actual', 'comparison')\n\
         let g:true_ok = assert_true(1)\n\
         let g:false_ok = assert_false(0)\n\
         let g:notequal_ok = assert_notequal(1, 2)\n\
         let g:match_ok = assert_match('^act', 'actual')\n\
         let g:notmatch_ok = assert_notmatch('missing', 'actual')\n\
         let g:after = len(v:errors)",
    )
    .unwrap();

    for name in [
        "initial",
        "success",
        "true_ok",
        "false_ok",
        "notequal_ok",
        "match_ok",
        "notmatch_ok",
    ] {
        assert_eq!(
            exec.scope()
                .get_scoped(ScopeKind::Global, name.as_bytes(), 0),
            Ok(&Typval::Number(0)),
            "g:{name}"
        );
    }
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"failure", 0),
        Ok(&Typval::Number(1))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"after", 0),
        Ok(&Typval::Number(1))
    );
    {
        let ed = editor.editor();

        let Some(Object::Array(errors)) = ed.vvars().get(&OxStr::from("errors")) else {
            panic!("v:errors must remain an Array");
        };
        assert_eq!(errors.len(), 1);
        assert!(
            matches!(&errors[0], Object::String(text) if text.to_string_lossy().contains("comparison"))
        );
        assert!(ed.messages().iter().any(|message| {
            message.kind == crate::MessageKind::Error
                && matches!(&message.content, Object::String(text) if text.to_string_lossy().contains("comparison"))
        }));
    }

    exec.execute_line(&editor, "let v:errors = []").unwrap();
    assert_eq!(
        editor.editor().vvars().get(&OxStr::from("errors")),
        Some(&Object::Array(Vec::new()))
    );
}

#[test]
fn dictionary_assertion_omits_equal_entries_and_sorts_keys() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_line(
        &editor,
        "call assert_equal(#{one: 1, two: 2}, #{two: 2, one: 3})",
    )
    .unwrap();

    let ed = editor.editor();
    let Some(Object::Array(errors)) = ed.vvars().get(&OxStr::from("errors")) else {
        panic!("v:errors must remain an Array");
    };
    assert!(
        matches!(
            errors.as_slice(),
            [Object::String(text)]
                if text.to_string_lossy().ends_with(
                    "Expected {'one': 1} but got {'one': 3} - 1 equal item omitted"
                )
        ),
        "{errors:?}"
    );
}

#[test]
fn line_last_and_append_support_oldtest_result_logging() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &editor,
        "<oldtest-log>",
        "call setline(1, 'first')\ncall append(line('$'), ['second', 'third' . nr2char(10) . 'continued'])\nlet g:last = line('$')",
    )
    .unwrap();

    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"last", 0),
        Ok(&Typval::Number(3))
    );
    let ed = editor.editor();

    let text = ed.buffer(buffer).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"first\nsecond\nthird\0continued");
}

#[test]
fn assert_fails_executes_commands_and_consumes_expected_errors() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "assert_fails.vim",
        "let g:before = len(v:errors)\nlet g:ok = assert_fails('call Missing()', 'E117:')\nlet g:bad = assert_fails('let g:ran = 1', 'E121:', 'must fail')\nlet g:after = len(v:errors)",
    ).unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"before", 0),
        Ok(&Typval::Number(0))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"ok", 0),
        Ok(&Typval::Number(0))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"bad", 0),
        Ok(&Typval::Number(1))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"after", 0),
        Ok(&Typval::Number(1))
    );
    let ed = editor.editor();

    let Some(Object::Array(errors)) = ed.vvars().get(&OxStr::from("errors")) else {
        panic!("v:errors must remain an Array")
    };
    assert!(
        matches!(&errors[0], Object::String(text) if text.to_string_lossy().contains("must fail"))
    );
}

#[test]
fn assert_fails_without_an_expected_error_only_requires_the_command_to_fail() {
    // f_assert_fails takes 1 to 5 arguments: with no {error} the assertion is
    // satisfied by any failure, and only a command that ran cleanly is
    // reported. test_assert.vim:368 (`assert_fails('throw "error"')`) crashed
    // the process while the second argument was read unconditionally.
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "assert_fails_one_arg.vim",
        "let g:threw = assert_fails('throw \"error\"')\nlet g:missing = assert_fails('call Missing()')\nlet g:clean = assert_fails('let g:ran = 1')\nlet g:after = len(v:errors)",
    ).unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"threw", 0),
        Ok(&Typval::Number(0))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"missing", 0),
        Ok(&Typval::Number(0))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"clean", 0),
        Ok(&Typval::Number(1))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"after", 0),
        Ok(&Typval::Number(1))
    );
    let ed = editor.editor();

    let Some(Object::Array(errors)) = ed.vvars().get(&OxStr::from("errors")) else {
        panic!("v:errors must remain an Array")
    };
    assert!(
        matches!(&errors[0], Object::String(text) if text.to_string_lossy().contains("command did not fail: let g:ran = 1"))
    );
}

#[test]
fn feedkeys_builtin_queues_input_consumed_by_getchar() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "call feedkeys('a', 'n')")
        .unwrap();
    assert_eq!(editor.editor().typeahead().as_bytes(), b"a");
    assert_eq!(
        editor.editor().typeahead().front_flags().unwrap().remap,
        crate::Remap::No
    );
    exec.execute_line(&editor, "let g:fed = getchar()").unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"fed", 0),
        Ok(&Typval::Number(97))
    );
    assert!(editor.editor().typeahead().is_empty());
}

#[test]
fn getchar_special_keys_preserve_or_simplify_as_requested() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "getchar.vim",
        r#"call feedkeys("\<M-F2>", '')
let g:function_key = getchar(0)
call feedkeys("\<*C-I>", '')
let g:control_i = getchar(-1)
call feedkeys("\<*C-I>", '')
let g:raw_control_i = getchar(-1, #{simplify: v:false})
call feedkeys("\<Tab>", '')
let g:string_tab = getchar(-1, #{number: v:false})"#,
    )
    .unwrap();
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"function_key", 0),
        Ok(&Typval::String(OxStr(vec![
            0x80, 0xfc, 8, 0x80, b'k', b'2'
        ])))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"control_i", 0),
        Ok(&Typval::Number(9))
    );
    assert_eq!(
        exec.scope()
            .get_scoped(ScopeKind::Global, b"raw_control_i", 0),
        Ok(&Typval::String(OxStr(vec![0x80, 0xfc, 4, b'I'])))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"string_tab", 0),
        Ok(&Typval::String(OxStr(vec![b'\t'])))
    );
}

#[test]
fn progpath_is_seeded_from_the_running_executable() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:progpath = v:progpath")
        .unwrap();
    let expected = std::env::current_exe().unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"progpath", 0),
        Ok(&Typval::String(OxStr(
            expected.to_string_lossy().into_owned().into_bytes()
        )))
    );
}

#[test]
fn setbufvar_updates_variables_and_buffer_options() {
    let (editor, buffer, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "setbufvar.vim",
        "call setbufvar(bufnr('%'), 'answer', [42])\ncall setbufvar(bufnr('%'), '&tabstop', 3)\nlet g:answer = getbufvar(bufnr('%'), 'answer')\nlet g:tabstop = getbufvar(bufnr('%'), '&tabstop')",
    ).unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"answer", 0),
        Ok(&Typval::list(vec![Typval::Number(42)]))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"tabstop", 0),
        Ok(&Typval::Number(3))
    );
    assert_eq!(
        editor
            .editor()
            .options()
            .get_buffer(buffer, "tabstop")
            .unwrap(),
        &OptionValue::Number(3)
    );
}

#[test]
fn setbufvar_unknown_option_reports_e518() {
    let (editor, _, _) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    let error = exec
        .execute_line(&editor, "call setbufvar(bufnr('%'), '&missing_option', 1)")
        .unwrap_err();
    assert!(matches!(
        error,
        ExecError::Vim(ref exception)
            if exception.kind == VimExceptionKind::Error("E518".to_owned())
                && exception.message().contains("Unknown option")
    ));
}

#[test]
fn matchstrlist_misplaced_lookaround_reports_e866() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    let error = exec
        .execute_line(&editor, r"call matchstrlist(['abc'], '\@=')")
        .unwrap_err();
    assert!(matches!(
        error,
        ExecError::Vim(ref exception)
            if exception.kind == VimExceptionKind::Error("E866".to_owned())
                && exception.message().contains("Misplaced @")
    ));
}

#[test]
fn feedkeys_x_executes_through_mode_machine() {
    let (editor, _buffer, window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "call setline(1, 'abc')")
        .unwrap();
    exec.execute_line(&editor, "call feedkeys('l', 'x')")
        .unwrap();
    assert_eq!(
        editor.editor().window(window).unwrap().cursor,
        Position { lnum: 1, col: 1 }
    );
    assert!(editor.editor().typeahead().is_empty());
}

#[test]
fn feedkeys_execute_runs_command_line_input_without_e121() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "feedkeys_input.vim",
        r#"call feedkeys(":let c = input('Q:')\<CR>B\<CR>", 'xt')
let g:captured = c"#,
    )
    .unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"captured", 0),
        Ok(&Typval::String(OxStr::from("B")))
    );
}

#[test]
fn highlight_exists_reads_editor_highlight_table() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(&editor, "highlight.vim", "highlight Number guifg=#ffffff\nlet g:yes = hlexists('number')\nlet g:no = highlight_exists('missing')").unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"yes", 0),
        Ok(&Typval::Number(1))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"no", 0),
        Ok(&Typval::Number(0))
    );
}

#[test]
fn position_builtins_round_trip_and_expand_tabs() {
    let (editor, _buffer, window) = editor_with_window();

    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_script(&editor, "position.vim", "call setline(1, \"the\tquick\")\ncall setpos('.', [0, 1, 4, 0])\nlet g:position = getcurpos()\nlet g:column = virtcol('.')\nlet g:span = virtcol('.', v:true)").unwrap();
    assert_eq!(
        editor.editor().window(window).unwrap().cursor,
        Position { lnum: 1, col: 3 }
    );
    // move.c:update_curswant — the wanted column is the cursor's virtual
    // column, and plines.c:getvcol puts the Normal-mode cursor on a tab's
    // last cell: 'ts'=8 spans the tab over virtual columns 4-8, so
    // w_curswant is 7 and getcurpos() answers 8.
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"position", 0),
        Ok(&Typval::list(vec![
            Typval::Number(0),
            Typval::Number(1),
            Typval::Number(4),
            Typval::Number(0),
            Typval::Number(8)
        ]))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"column", 0),
        Ok(&Typval::Number(8))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"span", 0),
        Ok(&Typval::list(vec![Typval::Number(4), Typval::Number(8)]))
    );
}

#[test]
fn virtcol_counts_showbreak_on_wrapped_continuation_rows() {
    let editor = TestEditorAccess::new(Editor::new());
    let buffer = editor.editor_mut().create_buffer(true).unwrap();
    let tab = editor
        .editor_mut()
        .create_tabpage(buffer, Geometry::new(0, 0, 10, 6).unwrap())
        .unwrap();
    let window = editor.editor().tabpage(tab).unwrap().current_window();
    editor
        .editor_mut()
        .options_mut()
        .set_window(window, "showbreak", OptionValue::String("!!".to_owned()))
        .unwrap();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "virtcol_showbreak.vim",
        &format!(
            "call setline(1, 'aaaaaaaaaaaa')\nlet g:first = virtcol([1, 10], v:true, {})\nlet g:wrapped = virtcol([1, 11], v:true, {})",
            i64::from(window),
            i64::from(window),
        ),
    ).unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"first", 0),
        Ok(&Typval::list(vec![Typval::Number(10), Typval::Number(10)]))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"wrapped", 0),
        Ok(&Typval::list(vec![Typval::Number(13), Typval::Number(13)]))
    );
}

/// `f_stdpath` (`eval/funcs.c:7011-7040`) through `get_xdg_home` and
/// `stdpaths_get_xdg_var` (`os/stdpaths.c:151-225`).
///
/// Oracle, `nvim --headless -u <lua>` with every `XDG_*` pointed at a scratch
/// directory: `cache`/`config`/`data`/`state` are that directory plus
/// `/nvim`, `log` is the state directory plus `/nvim/logs`, `run` is
/// `$XDG_RUNTIME_DIR` with *no* `nvim` component, and `config_dirs` is a List.
/// An unknown selector is `E6100` and no argument is `E119`.
///
/// `$XDG_*` is read from the process environment, so this test sets it for the
/// duration and puts it back; `--test-threads=1` is how the suite runs.
#[test]
fn stdpath_resolves_every_selector_from_the_xdg_environment() {
    let _guard = crate::PROCESS_STATE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let names = [
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_DIRS",
    ];
    let _env = crate::test_guard::EnvGuard::new(&names);
    let root = std::env::temp_dir().join(format!("oxvim-t78-stdpath-{}", std::process::id()));
    for name in names {
        let suffix = name
            .strip_prefix("XDG_")
            .unwrap_or(name)
            .to_ascii_lowercase();
        let _set = ox_sys::set_env(name, root.join(suffix).as_os_str());
    }

    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "stdpath.vim",
        "let g:cache = stdpath('cache')\nlet g:config = stdpath('config')\nlet g:data = stdpath('data')\nlet g:state = stdpath('state')\nlet g:log = stdpath('log')\nlet g:run = stdpath('run')\nlet g:dirs = stdpath('config_dirs')",
    )
    .unwrap();
    let expect = |name: &str, tail: &str| {
        Typval::String(OxStr::from(
            root.join(name).join(tail).to_string_lossy().as_ref(),
        ))
    };
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"cache", 0),
        Ok(&expect("cache_home", "nvim"))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"config", 0),
        Ok(&expect("config_home", "nvim"))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"data", 0),
        Ok(&expect("data_home", "nvim"))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"state", 0),
        Ok(&expect("state_home", "nvim"))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"log", 0),
        Ok(&expect("state_home", "nvim/logs"))
    );
    // `run` is the raw variable: `f_stdpath` calls `stdpaths_get_xdg_var`
    // rather than `get_xdg_home` for it (`eval/funcs.c:7032`).
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"run", 0),
        Ok(&Typval::String(OxStr::from(
            root.join("runtime_dir").to_string_lossy().as_ref()
        ))),
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"dirs", 0),
        Ok(&Typval::list(vec![expect("config_dirs", "nvim")])),
    );

    let mut exec = ExExecutor::new();
    let bogus = exec
        .execute_line(&editor, "let g:x = stdpath('nope')")
        .unwrap_err();
    assert!(bogus.to_string().contains("E6100"), "{bogus}");
    let missing = exec
        .execute_line(&editor, "let g:x = stdpath()")
        .unwrap_err();
    assert!(missing.to_string().contains("E119"), "{missing}");
}

/// `f_shellescape` (`eval/funcs.c:6660-6667`) through
/// `vim_strsave_shellescape` (`strings.c:186-290`), and `f_strdisplaywidth`
/// (`strings.c:2775-2785`).
///
/// Oracle: `shellescape("a b'c!d%e#f")` is `'a b'\''c!d%e#f'`,
/// `shellescape('a!b', 1)` is `'a\!b'`, `shellescape('x%y#z', 1)` is
/// `'x\%y\#z'`, `shellescape('a<cword>b', 1)` is `'a\<cword>b'`;
/// `strdisplaywidth("a\tb")` is 9 and `strdisplaywidth("a\tb", 3)` is 6.
/// The two `strdisplaywidth` rows differ only in the starting column, which is
/// the whole reason it is not `strwidth`: the tab is measured to the next
/// `'tabstop'`, so the answer moves with the column.
#[test]
fn shellescape_and_strdisplaywidth_follow_the_shell_and_the_tabstop() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "escape.vim",
        "let g:plain = shellescape(\"a b'c!d%e#f\")\nlet g:special = shellescape('a!b', 1)\nlet g:vars = shellescape('x%y#z', 1)\nlet g:cword = shellescape('a<cword>b', 1)\nlet g:width = strdisplaywidth(\"a\\tb\")\nlet g:width_at_3 = strdisplaywidth(\"a\\tb\", 3)",
    )
    .unwrap();
    let global = |name: &[u8]| exec.scope().get_scoped(ScopeKind::Global, name, 0).cloned();
    assert_eq!(
        global(b"plain"),
        Ok(Typval::String(OxStr::from("'a b'\\''c!d%e#f'")))
    );
    assert_eq!(
        global(b"special"),
        Ok(Typval::String(OxStr::from("'a\\!b'")))
    );
    // `%`, `#` and `<cword>` are `find_cmdline_var` names
    // (`ex_docmd.c:7491-7508`), escaped only when the caller asked.
    assert_eq!(
        global(b"vars"),
        Ok(Typval::String(OxStr::from("'x\\%y\\#z'")))
    );
    assert_eq!(
        global(b"cword"),
        Ok(Typval::String(OxStr::from("'a\\<cword>b'")))
    );
    assert_eq!(global(b"width"), Ok(Typval::Number(9)));
    assert_eq!(global(b"width_at_3"), Ok(Typval::Number(6)));

    // A csh-like 'shell' escapes `!` with no second argument at all, which is
    // why this reads the option rather than $SHELL (`option.c:7095-7098`).
    editor
        .editor_mut()
        .options_mut()
        .set_global("shell", OptionValue::String("/bin/tcsh".to_owned()))
        .unwrap();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &editor,
        "csh.vim",
        "let g:csh = shellescape('a!b')\nlet g:both = shellescape('a!b', 1)",
    )
    .unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"csh", 0),
        Ok(&Typval::String(OxStr::from("'a\\!b'"))),
    );
    // csh plus do_special is two backslashes: one for Vim, one for the shell.
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"both", 0),
        Ok(&Typval::String(OxStr::from("'a\\\\!b'"))),
    );
}

// ── :set filetype → FileType autocmd dispatch ─────────────────────────
//
// option.c:4150-4156 — did_set_option fires do_filetype_autocmd after each
// committed 'filetype' write, before the next `:set` argument. autocmd.c:
// 2516-2539 — do_filetype_autocmd suppresses same-value recursion through
// ft_recursive and passes `force || ft_recursive == 1` as the nested flag;
// apply_autocmds (1465-1468) lets a non-forced event through only a
// ++nested handler while autocommands are busy.

/// One named buffer with a tabpage (so the buffer-local option is settable)
/// and a fresh executor.
fn filetype_fixture(name: &str) -> (Editor, ox_types::BufHandle, ExExecutor) {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .buffer_mut(buffer)
        .unwrap()
        .set_name(OxStr::from(name));
    editor
        .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    (editor, buffer, ExExecutor::new())
}

fn global_scope(exec: &ExExecutor, name: &[u8]) -> Option<Typval> {
    exec.scope()
        .get_scoped(ScopeKind::Global, name, 0)
        .cloned()
        .ok()
}

// The event binds `<amatch>` to the committed value, `<afile>` to the target
// buffer's name, and `<abuf>` to the target buffer, for `filetype`, the `ft`
// alias, and `:setlocal` alike.
#[test]
fn set_filetype_fires_with_committed_value_and_buffer_context() {
    let (editor, buffer, mut exec) = filetype_fixture("notes.py");

    let editor = TestEditorAccess::new(editor);
    exec.execute_line(
        &editor,
        "autocmd FileType * let g:amatch = expand('<amatch>')",
    )
    .unwrap();
    exec.execute_line(
        &editor,
        "autocmd FileType * let g:afile = expand('<afile>')",
    )
    .unwrap();
    exec.execute_line(&editor, "autocmd FileType * let g:abuf = expand('<abuf>')")
        .unwrap();

    exec.execute_line(&editor, "set filetype=python").unwrap();
    assert_eq!(
        global_scope(&exec, b"amatch"),
        Some(Typval::String(OxStr::from("python")))
    );
    assert_eq!(
        global_scope(&exec, b"afile"),
        Some(Typval::String(OxStr::from("notes.py")))
    );
    assert_eq!(
        global_scope(&exec, b"abuf"),
        Some(Typval::String(OxStr::from(
            i64::from(buffer).to_string().as_str()
        ))),
    );
    assert_eq!(
        editor
            .editor()
            .options()
            .get_buffer(buffer, "filetype")
            .unwrap(),
        &OptionValue::String("python".to_owned()),
    );

    exec.execute_line(&editor, "set ft=rust").unwrap();
    assert_eq!(
        global_scope(&exec, b"amatch"),
        Some(Typval::String(OxStr::from("rust")))
    );

    exec.execute_line(&editor, "setlocal filetype=lua").unwrap();
    assert_eq!(
        global_scope(&exec, b"amatch"),
        Some(Typval::String(OxStr::from("lua")))
    );
}

// `:setglobal` on a `noglob` option is accepted and has no effect: it does not
// fire FileType and new buffers do not inherit the value.
#[test]
fn setglobal_filetype_succeeds_without_firing() {
    let (editor, _buffer, mut exec) = filetype_fixture("notes.py");

    let editor = TestEditorAccess::new(editor);
    exec.execute_line(&editor, "autocmd FileType rust let g:hit = 1")
        .unwrap();
    exec.execute_line(&editor, "setglobal filetype=rust")
        .unwrap();
    assert_eq!(global_scope(&exec, b"hit"), None);
}

// Two top-level writes of the same value each fire; `++once` consumes on the
// first (autocmd.c apply_jumplist 1988-2011 acknowledges one-shot handlers
// as each action starts).
#[test]
fn top_level_same_value_writes_fire_twice_and_once_consumes_one() {
    let (editor, _buffer, mut exec) = filetype_fixture("notes.py");

    let editor = TestEditorAccess::new(editor);
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "*",
            &AutocmdKind::ExString("let g:hits = exists('g:hits') ? g:hits + 1 : 1".to_owned()),
            &AutocmdOptions::default(),
        )
        .unwrap();
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "*",
            &AutocmdKind::ExString("let g:onces = exists('g:onces') ? g:onces + 1 : 1".to_owned()),
            &AutocmdOptions {
                once: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();

    exec.execute_line(&editor, "set ft=x").unwrap();
    exec.execute_line(&editor, "set ft=x").unwrap();
    assert_eq!(global_scope(&exec, b"hits"), Some(Typval::Number(2)));
    assert_eq!(global_scope(&exec, b"onces"), Some(Typval::Number(1)));
}

// A ++nested handler re-assigning the same value must not recurse: the
// ft_recursive guard suppresses it even though the handler allows nesting.
#[test]
fn same_value_filetype_inside_a_nested_handler_does_not_recurse() {
    let (editor, _buffer, mut exec) = filetype_fixture("notes.py");

    let editor = TestEditorAccess::new(editor);
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "*",
            &AutocmdKind::ExString("let g:hits = exists('g:hits') ? g:hits + 1 : 1".to_owned()),
            &AutocmdOptions::default(),
        )
        .unwrap();
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "python",
            &AutocmdKind::ExString("set filetype=python".to_owned()),
            &AutocmdOptions {
                nested: true,
                ..AutocmdOptions::default()
            },
        )
        .unwrap();

    exec.execute_line(&editor, "set ft=python").unwrap();
    assert_eq!(global_scope(&exec, b"hits"), Some(Typval::Number(1)));
}

// A changed assignment recurses exactly once even through a non-nested
// outer handler (the value change forces it), and the outer `<amatch>` is
// restored for handlers sequenced after the nested one.
#[test]
fn changed_filetype_recursion_fires_once_and_restores_the_outer_match() {
    let (editor, _buffer, mut exec) = filetype_fixture("notes.py");

    let editor = TestEditorAccess::new(editor);
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "python",
            &AutocmdKind::ExString("set filetype=lua".to_owned()),
            &AutocmdOptions::default(),
        )
        .unwrap();
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "python",
            &AutocmdKind::ExString("let g:outer_match = expand('<amatch>')".to_owned()),
            &AutocmdOptions::default(),
        )
        .unwrap();
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "lua",
            &AutocmdKind::ExString("let g:inner_match = expand('<amatch>')".to_owned()),
            &AutocmdOptions::default(),
        )
        .unwrap();

    exec.execute_line(&editor, "set ft=python").unwrap();
    assert_eq!(
        global_scope(&exec, b"inner_match"),
        Some(Typval::String(OxStr::from("lua")))
    );
    assert_eq!(
        global_scope(&exec, b"outer_match"),
        Some(Typval::String(OxStr::from("python")))
    );
}

// Query and display arguments commit nothing and stay silent; writes that
// fail before committing (no current buffer) are silent too.
#[test]
fn query_display_and_failed_writes_never_fire_filetype() {
    let (editor, _buffer, mut exec) = filetype_fixture("notes.py");

    let editor = TestEditorAccess::new(editor);
    editor
        .editor_mut()
        .autocmds_mut()
        .register_legacy(
            &[Event::FileType],
            "*",
            &AutocmdKind::ExString("let g:hits = 1".to_owned()),
            &AutocmdOptions::default(),
        )
        .unwrap();

    exec.execute_line(&editor, "set ft?").unwrap();
    exec.execute_line(&editor, "set ft").unwrap();
    assert_eq!(global_scope(&exec, b"hits"), None);

    let bare = TestEditorAccess::new(Editor::new());
    let mut bare_exec = ExExecutor::new();
    bare_exec
        .execute_line(&bare, "autocmd FileType * let g:bare = 1")
        .unwrap();
    assert!(bare_exec.execute_line(&bare, "setlocal ft=x").is_err());
    assert!(bare_exec.execute_line(&bare, "set ft=x").is_err());
    assert_eq!(global_scope(&bare_exec, b"bare"), None);
}

// A handler failure keeps the first assignment committed (option.c fires the
// event after the write) and abandons later arguments of the same `:set`.
#[test]
fn failing_handler_keeps_the_assignment_and_abandons_later_arguments() {
    let (editor, buffer, mut exec) = filetype_fixture("notes.py");

    let editor = TestEditorAccess::new(editor);
    exec.execute_line(&editor, "autocmd FileType python call NoSuchFunction()")
        .unwrap();

    let error = exec
        .execute_line(&editor, "set ft=python ft=lua")
        .unwrap_err();
    match error {
        ExecError::Vim(exception) => {
            assert_eq!(exception.kind, VimExceptionKind::Error("E117".to_owned()));
        }
        other => panic!("expected Vim E117, got {other:?}"),
    }
    assert_eq!(
        editor
            .editor()
            .options()
            .get_buffer(buffer, "filetype")
            .unwrap(),
        &OptionValue::String("python".to_owned()),
    );
}

// ── #15d differential bidirectional scope sync ─────────────────────────

// buffer.c:buf_set_changedtick + `b:changedtick`, and the #15d dirty-flag
// gate. A scope-side write must reach the editor through the gate: if
// `Scope::set_scoped` ever drops its `mark_dirty`, the write side skips
// `scope_to_dict` and the script's assignment silently never lands.
#[test]
fn scope_write_propagates_to_editor_through_dirty_gate() {
    let (mut editor, _, _) = editor_with_window();
    let mut scope = ox_eval::Scope::new();

    // Establish a clean baseline: read, then write back with nothing changed.
    crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();
    crate::excmd_exec::sync_scope_into_editor(&mut editor, &scope).unwrap();
    assert!(!scope.synced.is_dirty(ScopeKind::Global));

    scope
        .set_scoped(ScopeKind::Global, b"test_var", 0, Typval::Number(42))
        .unwrap();
    assert!(
        scope.synced.is_dirty(ScopeKind::Global),
        "set_scoped must mark g: dirty"
    );

    crate::excmd_exec::sync_scope_into_editor(&mut editor, &scope).unwrap();

    let value = editor
        .gvars()
        .0
        .iter()
        .find(|(key, _)| key.as_bytes() == b"test_var")
        .map(|(_, value)| value.clone());
    assert_eq!(value, Some(Object::Integer(42)));
    assert!(
        !scope.synced.is_dirty(ScopeKind::Global),
        "a landed write must clear the dirty flag"
    );
}

// buffer.c:buf_freeall / the `b:` mirror. Two buffers that have each had one
// variable write report the *same* `variables_version`, so a version-only
// gate cannot notice the switch and would keep serving buffer 1's cached map
// after the window moved to buffer 2. Identity tracking is what makes the
// switch visible.
#[test]
fn buffer_switch_invalidates_scope_buffer_cache() {
    let (mut editor, first, _) = editor_with_window();
    let second = editor.create_buffer(true).unwrap();
    editor
        .buffer_mut(first)
        .unwrap()
        .variables_mut()
        .0
        .push((OxStr::from("first_var"), Object::Integer(1)));
    // Give the second buffer the same number of variable writes (no entries),
    // so both maps carry the same version stamp.
    editor.buffer_mut(second).unwrap().variables_mut();

    // The version gate alone cannot tell the buffers apart.
    assert_eq!(
        editor.buffer_variables_version(first).unwrap(),
        editor.buffer_variables_version(second).unwrap()
    );

    editor
        .set_current_buffer(first, crate::BufferRelease::KeepLoaded)
        .unwrap();
    let mut scope = ox_eval::Scope::new();
    crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();
    assert!(
        scope
            .buffer
            .iter()
            .any(|(key, _)| key.as_bytes() == b"first_var"),
        "the current buffer's variables must be mirrored"
    );

    editor
        .set_current_buffer(second, crate::BufferRelease::KeepLoaded)
        .unwrap();
    crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();
    assert!(
        !scope
            .buffer
            .iter()
            .any(|(key, _)| key.as_bytes() == b"first_var"),
        "switching buffers must invalidate the cached b: map"
    );
}

// buffer.c:changedtick + eval/vars.c:b:changedtick. A text edit advances the
// live counter without touching the variable dict, so the version gate stays
// closed and the fast path must refresh the materialized tick in place.
#[test]
fn changedtick_updates_in_place_without_version_bump() {
    let (mut editor, buffer, _) = editor_with_window();
    let mut scope = ox_eval::Scope::new();
    crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();

    let tick = |scope: &ox_eval::Scope| -> i64 {
        scope
            .buffer
            .iter()
            .find(|(key, _)| key.as_bytes() == b"changedtick")
            .map(|(_, value)| match value {
                Typval::Number(tick) => *tick,
                other => panic!("b:changedtick must be a Number, got {other:?}"),
            })
            .expect("b:changedtick must be materialized")
    };
    let initial = tick(&scope);
    let version_before = editor.buffer_variables_version(buffer).unwrap();

    editor
        .replace_buffer_lines(LineReplaceRequest {
            buffer,
            start: 1,
            end: 1,
            lines: &[b"edited".to_vec()],
            cursor_before: Position { lnum: 1, col: 0 },
            cursor_after: Position { lnum: 1, col: 6 },
            timestamp: 1,
        })
        .unwrap();

    // The edit moved text, not variables: the gate that rebuilds `b:` stays
    // closed, which is what makes the in-place refresh load-bearing.
    assert_eq!(
        editor.buffer_variables_version(buffer).unwrap(),
        version_before,
        "a text edit must not bump the variable-map version"
    );
    crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();
    assert_eq!(
        scope.synced.buffer_version(),
        version_before,
        "the sync must have taken the fast path"
    );

    let updated = tick(&scope);
    assert!(
        updated > initial,
        "b:changedtick must advance on a text edit ({initial} -> {updated})"
    );
    assert_eq!(
        updated,
        i64::try_from(editor.buffer(buffer).unwrap().script_changedtick()).unwrap()
    );
}

// eval/vars.c:set_var_list_loop / the shared-container alias. A `List` or
// `Dict` value is an `Rc<RefCell<..>>`: the map entry and every clone name the
// same container, so `add(g:items, 3)` mutates the value without assigning to
// `g:`. Under the #15d dirty gate nothing at the assignment site notices, so
// the read that hands out the alias is what marks the map. Without it the new
// element never reaches the editor.
#[test]
fn in_place_container_mutation_reaches_editor() {
    let (editor, _, _) = editor_with_window();
    let editor = TestEditorAccess::new(editor);
    let mut exec = ExExecutor::new();
    exec.execute_line(&editor, "let g:items = [1, 2]").unwrap();
    // Drain the pending write so the next sync starts from a clean map.
    exec.execute_line(&editor, "echo g:items").unwrap();

    exec.execute_line(&editor, "call add(g:items, 3)").unwrap();

    let ed = editor.editor();
    let stored = ed
        .gvars()
        .0
        .iter()
        .find(|(key, _)| key.as_bytes() == b"items")
        .expect("g:items must exist in the editor")
        .1
        .clone();
    assert_eq!(
        stored,
        Object::Array(vec![
            Object::Integer(1),
            Object::Integer(2),
            Object::Integer(3)
        ]),
        "an in-place container mutation must reach the editor"
    );
}

#[test]
fn scope_sync_property_interleaved_mutations_mirror_editor() {
    // Complete-by-construction guard for the differential sync: a seeded
    // pseudo-random walk interleaves editor-side writes, script-side writes
    // (plain, compound, unlet), and buffer switches, syncing after every
    // step. Each synced kind's scope map must mirror the editor exactly.
    // Per-path tests pass with a missed dirty-mark on a path they do not
    // name; this walk is exhaustive over whatever paths it drives.
    let (mut editor, first, _) = editor_with_window();
    let second = editor.create_buffer(true).unwrap();
    editor
        .create_tabpage(second, Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    let mut scope = ox_eval::Scope::new();
    crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();

    let mut live: Vec<String> = Vec::new();
    let mut removed_names: Vec<String> = Vec::new();
    let mut rng: u64 = 0x5EED_CAFE_F00D_D00D;
    let mut next = move || {
        rng = rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (rng >> 33) % 100
    };

    for step in 0..200 {
        match next() {
            0..=24 => {
                // Editor-side global write: version bump must invalidate the
                // cached g: mirror on the next read sync.
                let name = format!("v{step}");
                live.push(name.clone());
                editor.gvars_mut().0.push((
                    OxStr::from(name.as_bytes()),
                    Object::Integer(i64::from(step)),
                ));
            }
            25..=49 => {
                // Script-side global write through the mutating API.
                let name = format!("s{step}");
                scope
                    .set_scoped(ScopeKind::Global, name.as_bytes(), 0, Typval::Number(7))
                    .unwrap();
            }
            50..=64 => {
                // Unlet-style removal through the consolidated route. The
                // victim comes from the live-name ledger, so every removal
                // after the first create bites a real entry.
                if let Some(victim) = live.pop() {
                    let removed = scope.remove_pair(ScopeKind::Global, victim.as_bytes());
                    assert!(removed, "victim {victim} must still be live at step {step}");
                    removed_names.push(victim);
                }
            }
            65..=79 => {
                // Buffer switch: identity must invalidate the b: cache.
                let current = editor.current_buffer().unwrap();
                let target = if current == first { second } else { first };
                editor
                    .set_current_buffer(target, crate::BufferRelease::KeepLoaded)
                    .unwrap();
            }
            _ => {
                // Editor-side buffer variable write.
                let buffer = editor.current_buffer().unwrap();
                editor.buffer_mut(buffer).unwrap().variables_mut().0.push((
                    OxStr::from(format!("b{step}").as_bytes()),
                    Object::Boolean(true),
                ));
            }
        }
        crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();
        crate::excmd_exec::sync_scope_into_editor(&mut editor, &scope).unwrap();
        crate::excmd_exec::sync_editor_into_scope(&editor, &mut scope).unwrap();

        // Mirror invariant: a freshly synced scope materializes the same
        // maps as the walked scope for every synced kind.
        let mut fresh = ox_eval::Scope::new();
        crate::excmd_exec::sync_editor_into_scope(&editor, &mut fresh).unwrap();
        let normalize = |mut map: Vec<(OxStr, Typval)>| {
            map.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            map
        };
        assert_eq!(
            normalize(scope.global.clone()),
            normalize(fresh.global.clone()),
            "g: mirror diverged at step {step}"
        );
        assert_eq!(
            normalize(scope.buffer.clone()),
            normalize(fresh.buffer.clone()),
            "b: mirror diverged at step {step}"
        );
    }
}

#[test]
fn recursive_containers_have_finite_display_text() {
    let dictionary = Typval::dict(Vec::new());
    let Typval::Dict(reference) = &dictionary else {
        panic!("Typval::dict must construct a dictionary");
    };
    reference
        .borrow_mut()
        .entries
        .push(ox_types::DictEntry::new(
            OxStr::from("self"),
            dictionary.clone(),
        ));

    assert_eq!(
        crate::excmd_exec::typval_to_text(&dictionary),
        "{'self': {...}}"
    );

    let reversed = Typval::dict(vec![
        (OxStr::from("two"), Typval::Number(2)),
        (OxStr::from("one"), Typval::Number(1)),
    ]);
    assert_eq!(
        crate::excmd_exec::typval_to_text(&reversed),
        "{'two': 2, 'one': 1}"
    );
    assert_eq!(
        crate::excmd_exec::typval_to_text(&Typval::Float(5.0)),
        "5.0"
    );
}

#[test]
fn stopinsert_requests_a_host_mode_transition() {
    let editor = TestEditorAccess::new(Editor::new());
    let mut exec = ExExecutor::new();

    exec.execute_line(&editor, "stopinsert").unwrap();

    assert_eq!(
        exec.take_pending_edit_mode(),
        Some(crate::PendingEditMode::StopInsert)
    );
}
