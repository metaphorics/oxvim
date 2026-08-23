#![allow(clippy::unwrap_used)]

//! Behavioral tests for ExExecutor state-mutating commands.
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
use ox_types::{Object, OxStr, Typval};

use crate::excmd_exec::{ExecError, ExecOutcome};
use crate::register::RegisterContent;
use crate::{
    vim_variable_is_writable, Editor, ExExecutor, Geometry, LuaExec, LuaExecError, MessageKind,
    OptionValue, VimExceptionKind,
};
use ox_types::WinHandle;

static CWD_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

// ── let / const / unlet scoped variables ──────────────────────────────

// ex_docmd.c:ex_let, eval.c:set_var — `:let g:name = value` stores into
// the global scope and is readable after execution.
#[test]
fn let_global_variable_assigns_to_g_scope() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "let g:state_var = 42")
        .unwrap();
    let value = exec.scope().get_scoped(ScopeKind::Global, b"state_var", 0).unwrap();
    assert_eq!(*value, Typval::Number(42));
}

// eval.c:set_var, E46 — `:const` locks the variable; a subsequent `:let`
// on the same name raises E46 "Cannot change read-only variable".
#[test]
fn const_then_reassign_produces_e46() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "const g:IMMUTABLE = 1").unwrap();
    let err = exec
        .execute_line(&mut editor, "let g:IMMUTABLE = 2")
        .unwrap_err();
    match err {
        ExecError::Vim(exc) => {
            assert_eq!(exc.kind, VimExceptionKind::Error("E46".to_owned()));
            assert!(exc.message().contains("Cannot change read-only variable"));
            assert!(exc.message().contains("g:IMMUTABLE"));
        }
        other => panic!("expected Vim E46, got {other:?}"),
    }
}

// eval.c:unlet_var, E108 — `:unlet` removes the variable; `:unlet!`
// suppresses E108 for names that do not exist.
#[test]
fn unlet_removes_variable_and_bang_suppresses_e108() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "let g:tmp = 1").unwrap();
    exec.execute_line(&mut editor, "unlet g:tmp").unwrap();
    assert!(exec
        .scope()
        .get_scoped(ScopeKind::Global, b"tmp", 0)
        .is_err());
    // `unlet!` on a non-existent name must not error.
    let result = exec.execute_line(&mut editor, "unlet! g:no_such");
    assert!(result.is_ok());
}

// eval.c:set_var, `+=` compound assignment — reads the current value,
// applies the operator, and writes back the result.
#[test]
fn let_compound_addition_operator() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "let g:counter = 10").unwrap();
    exec.execute_line(&mut editor, "let g:counter += 5").unwrap();
    let value = exec.scope().get_scoped(ScopeKind::Global, b"counter", 0).unwrap();
    assert_eq!(*value, Typval::Number(15));
}

#[test]
fn let_compound_assignment_reads_then_writes() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
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
    let (mut editor, buffer, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"let b:bufvar = "hello""#)
        .unwrap();
    let vars = editor.buffer(buffer).unwrap().variables();
    let value = vars.get(&OxStr::from("bufvar")).unwrap();
    assert_eq!(value, &Object::String(OxStr::from("hello")));
}

// ── option / register / env targets ───────────────────────────────────

// ex_docmd.c:ex_let with `&opt` target — `:let &number = 1` (boolean,
// window-local) and `:let &tabstop = 4` (number, buffer-local) route
// through assign_option to the option store (option.c:set_option_value).
#[test]
fn let_assigns_options_through_ampersand() {
    let (mut editor, buffer, window) = editor_with_window();
    let mut exec = ExExecutor::new();
    // Boolean option via `:let &number = 1` → window-local store.
    exec.execute_line(&mut editor, "let &number = 1").unwrap();
    assert_eq!(
        editor.options().get_window(window, "number").unwrap(),
        &crate::options::OptionValue::Boolean(true)
    );
    // Number option via `:let &tabstop = 4` → buffer-local store.
    exec.execute_line(&mut editor, "let &tabstop = 4").unwrap();
    assert_eq!(
        editor.options().get_buffer(buffer, "tabstop").unwrap(),
        &crate::options::OptionValue::Number(4)
    );
}

// eval/vars.c ex_let_option — compound operators on option references.
// `..=` claims both dots (the legacy single-dot `.` form behaves the
// same), concatenates string options, and `+=`/`-=` do arithmetic on
// number options. Cross-kind use raises E734 before any state changes.
#[test]
fn let_option_compound_concatenates_dot_dot_equals() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "let &runtimepath = '/first'")
        .unwrap();
    exec.execute_line(&mut editor, "let &runtimepath ..= ',/second'")
        .unwrap();
    assert_eq!(
        editor.options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String("/first,/second".to_owned())
    );
    // Legacy single-dot form has identical behavior.
    exec.execute_line(&mut editor, "let &runtimepath .= ',/third'")
        .unwrap();
    assert_eq!(
        editor.options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String("/first,/second,/third".to_owned())
    );
}

// eval/vars.c ex_let_option — `+=` on a number option adds through the
// option layer.
#[test]
fn let_option_compound_adds_number_option() {
    let (mut editor, buffer, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "let &tabstop = 4").unwrap();
    exec.execute_line(&mut editor, "let &tabstop += 2").unwrap();
    assert_eq!(
        editor.options().get_buffer(buffer, "tabstop").unwrap(),
        &crate::options::OptionValue::Number(6)
    );
}

// eval/vars.c ex_let_option — a `.` operator on a number option and a
// `+` operator on a string option both raise E734 without writing.
#[test]
fn let_option_compound_rejects_wrong_kind_with_e734() {
    let (mut editor, _, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    let error = exec
        .execute_line(&mut editor, "let &tabstop ..= 'x'")
        .unwrap_err();
    match error {
        ExecError::Vim(exception) => {
            assert_eq!(exception.kind, VimExceptionKind::Error("E734".to_owned()));
        }
        other => panic!("expected Vim E734, got {other:?}"),
    }
    let error = exec
        .execute_line(&mut editor, "let &runtimepath += '/x'")
        .unwrap_err();
    match error {
        ExecError::Vim(exception) => {
            assert_eq!(exception.kind, VimExceptionKind::Error("E734".to_owned()));
        }
        other => panic!("expected Vim E734, got {other:?}"),
    }
    // The rejected writes left the stored values untouched.
    assert_eq!(
        editor.options().get_global("runtimepath").unwrap(),
        &crate::options::OptionValue::String(String::new())
    );
}

// eval/vars.c ex_let — `..=` also compounds plain variables with string
// concatenation.
#[test]
fn let_variable_compound_dot_dot_equals_concatenates() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"let g:parts = "a""#).unwrap();
    exec.execute_line(&mut editor, r#"let g:parts ..= "b""#).unwrap();
    assert_eq!(
        *exec.scope().get_scoped(ScopeKind::Global, b"parts", 0).unwrap(),
        Typval::String(OxStr::from("ab"))
    );
}

// ex_docmd.c:ex_let with `@r` target — `:let @a = "text"` stores
// characterwise content into the editor register (register.c:set_register).
#[test]
fn let_assigns_register_through_at() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"let @a = "text""#).unwrap();
    let content = editor.registers().get('a').unwrap().unwrap();
    assert_eq!(content.to_bytes(), b"text");
}

// ex_docmd.c:ex_let with `$VAR` target — `:let $VAR = "value"` stores
// into the scope's env map (eval.c:env_setvar).
#[test]
fn let_assigns_environment_variable_through_dollar() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"let $OXVIM_TEST_ENV = "value""#)
        .unwrap();
    let value = exec.scope().get_env(b"OXVIM_TEST_ENV");
    assert_eq!(value, Typval::String(OxStr::from("value")));
}

// ── set / setlocal / setglobal ─────────────────────────────────────────

// option.c:set_option_value — `:set number` sets boolean true; `:set
// nonumber` sets it false.  Both go through the window-local layer.
#[test]
fn set_boolean_toggle_on_and_off() {
    let (mut editor, _, window) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "set number").unwrap();
    assert_eq!(
        editor.options().get_window(window, "number").unwrap(),
        &crate::options::OptionValue::Boolean(true)
    );
    exec.execute_line(&mut editor, "set nonumber").unwrap();
    assert_eq!(
        editor.options().get_window(window, "number").unwrap(),
        &crate::options::OptionValue::Boolean(false)
    );
}

// option.c:set_option_value — `:set tabstop=4` writes a number value
// to the buffer-local option overlay.
#[test]
fn set_number_option_with_equals() {
    let (mut editor, buffer, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "set tabstop=4").unwrap();
    assert_eq!(
        editor.options().get_buffer(buffer, "tabstop").unwrap(),
        &crate::options::OptionValue::Number(4)
    );
}

// option.c:set_option_value — `:set background=light` writes a string
// to the global option store.
#[test]
fn set_global_string_option() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "set background=light").unwrap();
    assert_eq!(
        editor.options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("light".into())
    );
}

// option.c:do_set — both `&` and `&vim` restore the option's Vim default.
#[test]
fn set_ampersand_forms_restore_declared_default() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "set background=light").unwrap();
    exec.execute_line(&mut editor, "set background&vim").unwrap();
    assert_eq!(
        editor.options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("dark".into())
    );

    exec.execute_line(&mut editor, "set background=light").unwrap();
    exec.execute_line(&mut editor, "set background&").unwrap();
    assert_eq!(
        editor.options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("dark".into())
    );
}

// option.c:show_one — `:set number?` emits an Echo message with the
// current effective value and no message history.
#[test]
fn set_query_produces_echo_message() {
    let (mut editor, _, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "set number?").unwrap();
    let msgs = editor.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind, MessageKind::Echo);
    assert!(!msgs[0].history);
    // Default for `number` is false → "nonumber".
    assert_eq!(message_text(&msgs[0]), "nonumber");
}

#[test]
fn set_reset_restores_macro_backed_grepformat_default() {
    let (mut editor, _, _) = editor_with_window();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "set grepformat=custom").unwrap();
    exec.execute_line(&mut editor, "set grepformat&").unwrap();

    assert_eq!(
        editor.options().get_global("grepformat").unwrap(),
        &crate::OptionValue::String("%f:%l:%m,%f:%l%m,%f  %l%m".to_owned())
    );
}

// option.c:set_option_value, E518 — `:set` on an unknown option name
// raises E518 "Unknown option".
#[test]
fn set_unknown_option_produces_e518() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    let err = exec
        .execute_line(&mut editor, "set notanoption")
        .unwrap_err();
    match err {
        ExecError::Vim(exc) => {
            assert_eq!(exc.kind, VimExceptionKind::Error("E518".to_owned()));
            assert!(exc.message().contains("Unknown option"));
        }
        other => panic!("expected Vim E518, got {other:?}"),
    }
}

// option.c:set_option_value, setglobal / setlocal routing —
// `:setglobal background=dark` writes the global layer; `:setlocal number`
// writes the window-local layer.
#[test]
fn setlocal_and_setglobal_route_to_correct_layer() {
    let (mut editor, _, window) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "setglobal background=dark")
        .unwrap();
    assert_eq!(
        editor.options().get_global("background").unwrap(),
        &crate::options::OptionValue::String("dark".into())
    );
    exec.execute_line(&mut editor, "setlocal number").unwrap();
    assert_eq!(
        editor.options().get_window(window, "number").unwrap(),
        &crate::options::OptionValue::Boolean(true)
    );
}

#[test]
fn enew_selects_a_distinct_empty_buffer() {
    let (mut editor, original, _) = editor_with_window();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "enew").unwrap();

    let current = editor.current_buffer().unwrap();
    assert_ne!(current, original);
    assert_eq!(editor.buffer(current).unwrap().text().unwrap().line_count(), 1);
}

// ── echo / echomsg / echon ─────────────────────────────────────────────

#[test]
fn global_menu_cleanup_succeeds_for_empty_menu_state() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "aunmenu *").unwrap();
    exec.execute_line(&mut editor, "tlunmenu *").unwrap();
    assert!(matches!(
        exec.execute_line(&mut editor, "aunmenu File.Open"),
        Err(crate::ExecError::NotImplemented(command)) if command == "aunmenu"
    ));
}

// ex_docmd.c:ex_echo — `:echo "hello"` produces an Echo message without
// retaining literal string quotes and without message history.
#[test]
fn echo_produces_unsaved_unquoted_message() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"echo "hello""#).unwrap();
    let msgs = editor.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind, MessageKind::Echo);
    assert!(!msgs[0].history);
    assert_eq!(message_text(&msgs[0]), "hello");
}

// ex_docmd.c:ex_echo, echomsg — `:echomsg` produces an Echo message
// with history=true (enters message history).
#[test]
fn echomsg_produces_message_with_history() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"echomsg "hello""#).unwrap();
    let msgs = editor.messages();
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
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"echon "abc" "def""#)
        .unwrap();
    let msgs = editor.messages();
    assert_eq!(msgs.len(), 1);
    // echon separator is "" → "abcdef".
    assert_eq!(message_text(&msgs[0]), "abcdef");
}

// ── execute ────────────────────────────────────────────────────────────

#[test]
fn set_minus_equal_removes_complete_comma_list_item() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "set wildoptions-=pum").unwrap();
    assert_eq!(editor.options().get_global("wildoptions").unwrap(), &OptionValue::String("tagfile".to_owned()));
}

// ex_docmd.c:ex_execute — `:execute` evaluates expression arguments,
// joins the resulting strings, and runs the joined text as a command.
#[test]
fn execute_evaluates_and_runs_string_as_command() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, r#"execute "let g:execvar = 99""#)
        .unwrap();
    let value = exec.scope().get_scoped(ScopeKind::Global, b"execvar", 0).unwrap();
    assert_eq!(*value, Typval::Number(99));
}

#[test]
fn execute_keeps_spaced_operators_inside_each_expression() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
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
fn cd_changes_the_directory_observed_by_getcwd() {
    let _guard = CWD_GUARD.lock().unwrap_or_else(|poison| poison.into_inner());
    let original = std::env::current_dir().unwrap();
    let target = std::env::temp_dir().join(format!("ox-editor-cd-{}", std::process::id()));
    std::fs::create_dir_all(&target).unwrap();

    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    let result = exec.execute_line(&mut editor, &format!("cd {}", target.display()));
    let changed = std::env::current_dir();
    std::env::set_current_dir(&original).unwrap();
    std::fs::remove_dir(&target).unwrap();

    result.unwrap();
    assert_eq!(changed.unwrap(), target);
}

#[test]
fn cd_minus_toggles_and_returns_previous_directory() {
    let _guard = CWD_GUARD.lock().unwrap_or_else(|poison| poison.into_inner());
    let original = std::env::current_dir().unwrap();
    let target = std::env::temp_dir().join(format!("ox-editor-cd-{}", std::process::id()));
    std::fs::create_dir_all(&target).unwrap();

    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, &format!("cd {}", target.display())).unwrap();
    assert_eq!(std::env::current_dir().unwrap(), target);
    exec.execute_line(&mut editor, "let g:before = chdir('-')").unwrap();
    assert_eq!(std::env::current_dir().unwrap(), original);
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"before", 0),
        Ok(&Typval::String(OxStr::from(target.to_string_lossy().as_ref()))),
    );
    std::fs::remove_dir(&target).unwrap();
}

#[test]
fn buffer_identity_builtins_resolve_current_and_named_buffers() {
    let (mut editor, buffer, _) = editor_with_window();
    editor
        .buffer_mut(buffer)
        .unwrap()
        .set_name(OxStr::from("named.vim"));
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &mut editor,
        "<buffer-identity>",
        "let g:current_name = bufname()\nlet g:named_number = bufnr('named.vim')\nlet g:named_exists = bufexists('named.vim')\nlet g:missing_exists = bufexists('missing.vim')",
    )
    .unwrap();

    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"current_name", 0),
        Ok(&Typval::String(OxStr::from("named.vim"))),
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"named_number", 0),
        Ok(&Typval::Number(i64::from(buffer))),
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"named_exists", 0),
        Ok(&Typval::Number(1)),
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"missing_exists", 0),
        Ok(&Typval::Number(0)),
    );
}

#[test]
fn execute_builtin_captures_nested_command_output() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "let g:swap = execute('swapname')")
        .unwrap();

    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"swap", 0),
        Ok(&Typval::String(OxStr::from("\nNo swap file"))),
    );
    assert!(editor.messages().is_empty());
}

// ── normal ─────────────────────────────────────────────────────────────

// ex_docmd.c:ex_normal — `:normal` requires keys and completes after queuing them.
#[test]
fn normal_with_key_args_completes() {
    let (mut editor, _, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    let result = exec.execute_line(&mut editor, "normal gg");
    assert_eq!(result.unwrap(), ExecOutcome::Completed);
}

// ── marks / registers output ───────────────────────────────────────────

// ex_docmd.c:ex_mark, `:marks` — outputs a header line followed by one
// line per mark with the mark name, line, and column.
#[test]
fn marks_outputs_header_and_mark_lines() {
    let (mut editor, buffer, _) = editor_with_window();
    editor
        .set_local_mark(buffer, 'a', Position { lnum: 3, col: 2 })
        .unwrap();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "marks").unwrap();
    let msgs = editor.messages();
    assert!(msgs.len() >= 2);
    assert_eq!(message_text(&msgs[0]), "mark line  col file/text");
    // Mark 'a' at line 3, col 2 → " a     3    2".
    let mark_line = message_text(&msgs[1]);
    assert!(mark_line.starts_with(" a"));
    assert!(mark_line.contains("3"));
    assert!(mark_line.contains("2"));
}

// ex_docmd.c:ex_registers, `:registers` — outputs one line per non-empty
// register in the requested set, formatted as `"x   content`.
#[test]
fn registers_outputs_register_listing() {
    let mut editor = Editor::new();
    editor
        .registers_mut()
        .set('a', RegisterContent::characterwise(b"hello").unwrap())
        .unwrap();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "registers a").unwrap();
    let msgs = editor.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(message_text(&msgs[0]), "\"a   hello");
}

// ── highlight storage ──────────────────────────────────────────────────

// ex_docmd.c:ex_highlight — `:highlight Group key=value` stores the
// attribute map; `:highlight clear Group` removes it.
#[test]
fn highlight_stores_and_clears_group_attributes() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "highlight MyGroup guifg=red")
        .unwrap();
    let attrs = editor.highlights().get("MyGroup").unwrap();
    assert_eq!(attrs.get("guifg").unwrap(), "red");
    exec.execute_line(&mut editor, "highlight clear MyGroup")
        .unwrap();
    assert!(editor.highlights().get("MyGroup").is_none());
}

#[test]
fn highlight_default_and_link_forms_retain_definitions() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "highlight Existing guifg=red").unwrap();
    exec.execute_line(&mut editor, "highlight default Existing guifg=blue").unwrap();
    exec.execute_line(&mut editor, "highlight default NewGroup cterm=bold").unwrap();
    exec.execute_line(&mut editor, "highlight link Linked Existing").unwrap();
    exec.execute_line(&mut editor, "highlight default link DefaultLinked NewGroup").unwrap();

    assert_eq!(editor.highlights()["Existing"]["guifg"], "red");
    assert_eq!(editor.highlights()["NewGroup"]["cterm"], "bold");
    assert_eq!(editor.highlights()["Linked"]["link"], "Existing");
    assert_eq!(editor.highlights()["DefaultLinked"]["link"], "NewGroup");
}

// ── unsupported-command NotImplemented identity ────────────────────────

// ex_docmd.c:do_one_cmd dispatch — a builtin command not in the handler
// table returns NotImplemented(name) rather than a silent no-op.
#[test]
fn unimplemented_builtin_returns_not_implemented() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    let err = exec.execute_line(&mut editor, "redraw").unwrap_err();
    match err {
        ExecError::NotImplemented(name) => assert_eq!(name, "redraw"),
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
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    let script = "let g:line1 = 1\nlet g:nonexistent";
    let err = exec.execute_script(&mut editor, "test", script).unwrap_err();
    match err {
        ExecError::Vim(exc) => {
            assert_eq!(exc.kind, VimExceptionKind::Error("E121".to_owned()));
            assert!(exc.message().contains("Undefined variable: g:nonexistent"));
            // Throwpoint includes the script name and line 2.
            assert_eq!(exc.throwpoint, "script test[2]");
        }
        other => panic!("expected Vim E121, got {other:?}"),
    }
}

// ex_docmd.c:ex_call, E488 — `:call` with trailing text after the closing
// parenthesis raises E488 "Trailing characters".  From the command line
// the throwpoint is "command line" (no script frame).
#[test]
fn e488_from_call_trailing_characters() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    let err = exec
        .execute_line(&mut editor, "call Foo()trailing")
        .unwrap_err();
    match err {
        ExecError::Vim(exc) => {
            assert_eq!(exc.kind, VimExceptionKind::Error("E488".to_owned()));
            assert_eq!(exc.message(), "E488: Trailing characters");
            assert_eq!(exc.throwpoint, "command line");
        }
        other => panic!("expected Vim E488, got {other:?}"),
    }
}

// ex_call: `ends_excmd` accepts a `"` after the closing parenthesis, so a
// trailing comment is not "trailing characters" (userfunc.c:3615).
#[test]
fn call_allows_trailing_comment_after_closing_paren() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(&mut editor, "test", "function Store()\n  let g:stored = 1\nendfunction")
        .unwrap();
    exec.execute_line(&mut editor, "call Store()  \" comment here")
        .unwrap();
    exec.execute_line(&mut editor, "call Store()\"tight comment").unwrap();
    assert!(exec.execute_line(&mut editor, "call Store()trailing").is_err());
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
    fn execute_chunk(&mut self, editor: &mut Editor, code: &str, args: Vec<Object>) -> Result<Object, LuaExecError> {
        self.chunks.push((code.to_owned(), args.clone()));
        if code == "set-test-global" {
            editor.gvars_mut().insert(OxStr::from("lua_global"), Object::Integer(42));
        }
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

    fn execute_file(&mut self, _editor: &mut Editor, path: &Path) -> Result<(), LuaExecError> {
        self.files.push(path.to_path_buf());
        self.error.clone().map_or(Ok(()), Err)
    }

    fn eval_expression(
        &mut self,
        _editor: &mut Editor,
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
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone()).execute_line(&mut editor, "lua local x = 1 | 2").unwrap();
    assert_eq!(host.borrow().chunks[0], ("local x = 1 | 2".to_owned(), Vec::new()));
}

#[test]
fn sourced_lua_heredoc_preserves_body_and_resumes_after_marker() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host.clone());

    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "lua << END\n-- body comment\n END\n  trailing spaces  \nEND\nlet g:after_heredoc = 9",
        )
        .unwrap();

    assert_eq!(
        host.borrow().chunks[0],
        ("-- body comment\n END\n  trailing spaces  \n".to_owned(), Vec::new()),
    );
    assert_eq!(
        executor.scope().get_scoped(ScopeKind::Global, b"after_heredoc", 0).unwrap(),
        &Typval::Number(9),
    );
}

#[test]
fn sourced_lua_trim_uses_first_nonempty_body_indent() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host.clone());

    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "  :  lua << trim END\n\n      first\n    second\n  END",
        )
        .unwrap();

    assert_eq!(host.borrow().chunks[0], ("\nfirst\nsecond\n".to_owned(), Vec::new()));
}

#[test]
fn sourced_lua_heredoc_accepts_empty_body_and_default_dot_marker() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host.clone());

    executor.execute_script(&mut editor, "test.vim", "lua << END\nEND").unwrap();
    executor
        .execute_script(&mut editor, "test.vim", "lua << \" default marker\nreturn 1\n.")
        .unwrap();

    assert_eq!(host.borrow().chunks[0].0, "");
    assert_eq!(host.borrow().chunks[1].0, "return 1\n");
}

#[test]
fn let_heredoc_assigns_trimmed_lines_as_list() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "  let g:lines =<< trim END\n\n      alpha\n    beta\n      \" text, not an Ex comment\n  END",
        )
        .unwrap();

    let Typval::List(lines) = executor.scope().get_scoped(ScopeKind::Global, b"lines", 0).unwrap() else {
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
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &mut editor,
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
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    let error = executor
        .execute_script(&mut editor, "test.vim", "let g:lines =<< END\nmissing")
        .unwrap_err();
    assert!(error.to_string().contains("E990"));
    assert!(error.to_string().contains("END"));
}

#[test]
fn let_expression_containing_heredoc_text_does_not_consume_source_lines() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:literal = 'a=<<b'\nlet g:after_literal = 4",
        )
        .unwrap();

    assert_eq!(
        executor.scope().get_scoped(ScopeKind::Global, b"literal", 0).unwrap(),
        &Typval::String(OxStr::from("a=<<b")),
    );
    assert_eq!(
        executor.scope().get_scoped(ScopeKind::Global, b"after_literal", 0).unwrap(),
        &Typval::Number(4),
    );
}

#[test]
fn lua_global_mutation_is_visible_to_following_ex_command() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host);
    executor.execute_line(&mut editor, "lua set-test-global").unwrap();
    executor.execute_line(&mut editor, "unlet g:lua_global").unwrap();
    assert!(editor.gvars().get(&OxStr::from("lua_global")).is_none());
}

#[test]
fn luafile_executes_named_file() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone()).execute_line(&mut editor, "luafile runtime/colors/vim.lua").unwrap();
    assert_eq!(host.borrow().files, [PathBuf::from("runtime/colors/vim.lua")]);
}

#[test]
fn luado_transforms_every_line_with_line_number() {
    let (mut editor, buffer, _) = editor_with_window();
    editor.replace_buffer_lines(
        buffer,
        1,
        1,
        &[b"alpha".to_vec(), b"beta".to_vec()],
        ox_text::Position { lnum: 1, col: 0 },
        ox_text::Position { lnum: 1, col: 0 },
        0,
    ).unwrap();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone()).execute_line(&mut editor, "luado return line").unwrap();
    let lines = (1..=2).map(|lnum| editor.buffer(buffer).unwrap().text().unwrap().line(lnum).unwrap()).collect::<Vec<_>>();
    assert_eq!(lines, [b"alpha:1".to_vec(), b"beta:2".to_vec()]);
    assert_eq!(host.borrow().chunks.len(), 2);
}

#[test]
fn lua_runtime_error_is_catchable_vim_error() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua {
        error: Some(LuaExecError::Runtime("boom".to_owned())),
        ..FakeLua::default()
    }));
    let error = lua_executor(host).execute_line(&mut editor, "lua error('boom')").unwrap_err();
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
    let (mut editor, buffer, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "let g:error = 'screen too small'").unwrap();
    exec.execute_line(&mut editor, "$put =g:error").unwrap();

    let text = editor.buffer(buffer).unwrap().text().unwrap();
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
    for name in ["count", "count1", "dying", "register", "event", "servername"] {
        assert!(!vim_variable_is_writable(name.as_bytes()), "v:{name}");
    }
}

#[test]
fn luaeval_passes_expression_and_argument_to_host() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua {
        eval_result: Some(Typval::Number(42)),
        ..FakeLua::default()
    }));
    let mut executor = lua_executor(host.clone());
    executor
        .execute_line(&mut editor, "let g:answer = luaeval('_A[1] + _A[2]', [40, 2])")
        .unwrap();
    assert_eq!(
        executor.scope().get_scoped(ScopeKind::Global, b"answer", 0).unwrap(),
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
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    lua_executor(host.clone())
        .execute_line(&mut editor, "let g:solo = luaeval('pcall(require, \"ffi\")')")
        .unwrap();
    assert_eq!(host.borrow().evals[0].0, "pcall(require, \"ffi\")");
    assert!(host.borrow().evals[0].1.is_none());
}

#[test]
fn luaeval_without_host_stays_not_implemented() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    let error = executor.execute_line(&mut editor, "echo luaeval('1')").unwrap_err();
    assert_eq!(error.to_string(), "not implemented: luaeval");
}

#[test]
fn luaeval_rejects_wrong_argument_counts() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua::default()));
    let mut executor = lua_executor(host);
    let error = executor.execute_line(&mut editor, "echo luaeval()").unwrap_err();
    assert!(error.to_string().contains("E119"), "{}", error);
    let error = executor.execute_line(&mut editor, "echo luaeval('1', 2, 3)").unwrap_err();
    assert!(error.to_string().contains("E118"), "{}", error);
}

#[test]
fn luaeval_load_and_runtime_errors_use_upstream_codes() {
    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua {
        error: Some(LuaExecError::Load("[string \"luaeval()\"]:1: syntax".to_owned())),
        ..FakeLua::default()
    }));
    let error = lua_executor(host).execute_line(&mut editor, "echo luaeval('synta x')").unwrap_err();
    let ExecError::Vim(exception) = error else { panic!("expected Vim error") };
    assert_eq!(exception.kind, VimExceptionKind::Error("E5107".to_owned()));
    assert!(exception.message().contains("Lua: [string \"luaeval()\"]:1: syntax"));

    let mut editor = Editor::new();
    let host = Rc::new(RefCell::new(FakeLua {
        error: Some(LuaExecError::Runtime("boom".to_owned())),
        ..FakeLua::default()
    }));
    let error = lua_executor(host).execute_line(&mut editor, "echo luaeval('error(1)')").unwrap_err();
    let ExecError::Vim(exception) = error else { panic!("expected Vim error") };
    assert_eq!(exception.kind, VimExceptionKind::Error("E5108".to_owned()));
    assert!(exception.message().contains("Lua: boom"));
}

#[test]
fn let_writes_upstream_writable_vim_variables_only() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_line(&mut editor, "let v:testing = 1").unwrap();
    assert_eq!(editor.vvars().get(&OxStr::from("testing")), Some(&Object::Integer(1)));

    let error = exec.execute_line(&mut editor, "let v:count = 2").unwrap_err();
    let ExecError::Vim(exception) = error else { panic!("expected E46") };
    assert_eq!(exception.kind, VimExceptionKind::Error("E46".to_owned()));
}

#[test]
fn assertions_record_failures_in_writable_v_errors_and_messages() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &mut editor,
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

    for name in ["initial", "success", "true_ok", "false_ok", "notequal_ok", "match_ok", "notmatch_ok"] {
        assert_eq!(
            exec.scope().get_scoped(ScopeKind::Global, name.as_bytes(), 0),
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
    let Some(Object::Array(errors)) = editor.vvars().get(&OxStr::from("errors")) else {
        panic!("v:errors must remain an Array");
    };
    assert_eq!(errors.len(), 1);
    assert!(matches!(&errors[0], Object::String(text) if text.to_string_lossy().contains("comparison")));
    assert!(editor.messages().iter().any(|message| {
        message.kind == crate::MessageKind::Error
            && matches!(&message.content, Object::String(text) if text.to_string_lossy().contains("comparison"))
    }));

    exec.execute_line(&mut editor, "let v:errors = []").unwrap();
    assert_eq!(editor.vvars().get(&OxStr::from("errors")), Some(&Object::Array(Vec::new())));
}

#[test]
fn line_last_and_append_support_oldtest_result_logging() {
    let (mut editor, buffer, _) = editor_with_window();
    let mut exec = ExExecutor::new();

    exec.execute_script(
        &mut editor,
        "<oldtest-log>",
        "call setline(1, 'first')\ncall append(line('$'), ['second', 'third' . nr2char(10) . 'continued'])\nlet g:last = line('$')",
    )
    .unwrap();

    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"last", 0),
        Ok(&Typval::Number(3))
    );
    let text = editor.buffer(buffer).unwrap().text().unwrap();
    assert_eq!(text.to_bytes(), b"first\nsecond\nthird\0continued");
}

#[test]
fn assert_fails_executes_commands_and_consumes_expected_errors() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
        "assert_fails.vim",
        "let g:before = len(v:errors)\nlet g:ok = assert_fails('call Missing()', 'E117:')\nlet g:bad = assert_fails('let g:ran = 1', 'E121:', 'must fail')\nlet g:after = len(v:errors)",
    ).unwrap();
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"before", 0), Ok(&Typval::Number(0)));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"ok", 0), Ok(&Typval::Number(0)));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"bad", 0), Ok(&Typval::Number(1)));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"after", 0), Ok(&Typval::Number(1)));
    let Some(Object::Array(errors)) = editor.vvars().get(&OxStr::from("errors")) else { panic!("v:errors must remain an Array") };
    assert!(matches!(&errors[0], Object::String(text) if text.to_string_lossy().contains("must fail")));
}

#[test]
fn feedkeys_builtin_queues_input_consumed_by_getchar() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "call feedkeys('a', 'n')").unwrap();
    assert_eq!(editor.typeahead().as_bytes(), b"a");
    assert_eq!(editor.typeahead().front_flags().unwrap().remap, crate::Remap::No);
    exec.execute_line(&mut editor, "let g:fed = getchar()").unwrap();
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"fed", 0), Ok(&Typval::Number(97)));
    assert!(editor.typeahead().is_empty());
}

#[test]
fn getchar_special_keys_preserve_or_simplify_as_requested() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
        "getchar.vim",
        r#"call feedkeys("\<M-F2>", '')
let g:function_key = getchar(0)
call feedkeys("\<*C-I>", '')
let g:control_i = getchar(-1)
call feedkeys("\<*C-I>", '')
let g:raw_control_i = getchar(-1, #{simplify: v:false})
call feedkeys("\<Tab>", '')
let g:string_tab = getchar(-1, #{number: v:false})"#,
    ).unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"function_key", 0),
        Ok(&Typval::String(OxStr(vec![0x80, 0xfc, 4, 0x80, b'k', b'2'])))
    );
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"control_i", 0), Ok(&Typval::Number(9)));
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"raw_control_i", 0),
        Ok(&Typval::String(OxStr(vec![0x80, 0xfc, 2, b'I'])))
    );
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"string_tab", 0),
        Ok(&Typval::String(OxStr(vec![b'\t'])))
    );
}

#[test]
fn progpath_is_seeded_from_the_running_executable() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "let g:progpath = v:progpath").unwrap();
    let expected = std::env::current_exe().unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"progpath", 0),
        Ok(&Typval::String(OxStr(expected.to_string_lossy().into_owned().into_bytes())))
    );
}

#[test]
fn setbufvar_updates_variables_and_buffer_options() {
    let (mut editor, buffer, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
        "setbufvar.vim",
        "call setbufvar(bufnr('%'), 'answer', [42])\ncall setbufvar(bufnr('%'), '&tabstop', 3)\nlet g:answer = getbufvar(bufnr('%'), 'answer')\nlet g:tabstop = getbufvar(bufnr('%'), '&tabstop')",
    ).unwrap();
    assert_eq!(
        exec.scope().get_scoped(ScopeKind::Global, b"answer", 0),
        Ok(&Typval::list(vec![Typval::Number(42)]))
    );
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"tabstop", 0), Ok(&Typval::Number(3)));
    assert_eq!(editor.options().get_buffer(buffer, "tabstop").unwrap(), &OptionValue::Number(3));
}

#[test]
fn setbufvar_unknown_option_reports_e518() {
    let (mut editor, _, _) = editor_with_window();
    let mut exec = ExExecutor::new();
    let error = exec.execute_line(&mut editor, "call setbufvar(bufnr('%'), '&missing_option', 1)").unwrap_err();
    assert!(matches!(
        error,
        ExecError::Vim(ref exception)
            if exception.kind == VimExceptionKind::Error("E518".to_owned())
                && exception.message().contains("Unknown option")
    ));
}

#[test]
fn feedkeys_x_executes_through_mode_machine() {
    let (mut editor, _buffer, window) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_line(&mut editor, "call setline(1, 'abc')").unwrap();
    exec.execute_line(&mut editor, "call feedkeys('l', 'x')").unwrap();
    assert_eq!(editor.window(window).unwrap().cursor, Position { lnum: 1, col: 1 });
    assert!(editor.typeahead().is_empty());
}

#[test]
fn highlight_exists_reads_editor_highlight_table() {
    let mut editor = Editor::new();
    let mut exec = ExExecutor::new();
    exec.execute_script(&mut editor, "highlight.vim", "highlight Number guifg=#ffffff\nlet g:yes = hlexists('number')\nlet g:no = highlight_exists('missing')").unwrap();
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"yes", 0), Ok(&Typval::Number(1)));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"no", 0), Ok(&Typval::Number(0)));
}

#[test]
fn position_builtins_round_trip_and_expand_tabs() {
    let (mut editor, _buffer, window) = editor_with_window();
    let mut exec = ExExecutor::new();
    exec.execute_script(&mut editor, "position.vim", "call setline(1, \"the\tquick\")\ncall setpos('.', [0, 1, 4, 0])\nlet g:position = getcurpos()\nlet g:column = virtcol('.')\nlet g:span = virtcol('.', v:true)").unwrap();
    assert_eq!(editor.window(window).unwrap().cursor, Position { lnum: 1, col: 3 });
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"position", 0), Ok(&Typval::list(vec![Typval::Number(0), Typval::Number(1), Typval::Number(4), Typval::Number(0), Typval::Number(4)])));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"column", 0), Ok(&Typval::Number(8)));
    assert_eq!(exec.scope().get_scoped(ScopeKind::Global, b"span", 0), Ok(&Typval::list(vec![Typval::Number(4), Typval::Number(8)])));
}

#[test]
fn virtcol_counts_showbreak_on_wrapped_continuation_rows() {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    let tab = editor.create_tabpage(buffer, Geometry::new(0, 0, 10, 6).unwrap()).unwrap();
    let window = editor.tabpage(tab).unwrap().current_window();
    editor.options_mut().set_window(window, "showbreak", OptionValue::String("!!".to_owned())).unwrap();
    let mut exec = ExExecutor::new();
    exec.execute_script(
        &mut editor,
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
