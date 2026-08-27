#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Behavioral tests for Ex control-flow execution: if/elseif/else nesting,
//! while/for iteration, break/continue, inactive branches, try/catch regex
//! matching, bare catch, finally ordering, uncaught throw, exception
//! message/throwpoint shape, and malformed control-structure errors.
//!
//! Upstream citations:
//! - `src/nvim/ex_docmd.c`: `do_cmdline` error-abort semantics, `ex_try`/
//!   `ex_catch`/`ex_finally` exception objects, throwpoint, catch pattern
//!   matching; `:if`/`:elseif`/`:else`/`:endif`, `:while`/`:endwhile`,
//!   `:for`/`:endfor`, `:break`, `:continue` dispatch.
//! - `src/nvim/ex_eval.c`: `ex_eval_inner` exception propagation, finally
//!   ordering across throw/return/break/continue, uncaught throw abort.
//! - `test/old/testdir/test_trycatch.vim`: catch pattern matching, bare
//!   catch, finally ordering, exception message shape.

use ox_eval::ScopeKind;
use ox_types::Typval;

use crate::{Editor, ExExecutor, ExecError, VimExceptionKind};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a `g:` variable as a number, panicking if missing or non-numeric.
fn gnum(executor: &ExExecutor, name: &str) -> i64 {
    match executor.scope().get_scoped(ScopeKind::Global, name.as_bytes(), 0) {
        Ok(Typval::Number(n)) => *n,
        Ok(other) => panic!("g:{name} is {other:?}, expected Number"),
        Err(_) => panic!("g:{name} is undefined"),
    }
}

/// Read a `g:` variable as a string, panicking if missing or non-string.
fn gstr(executor: &ExExecutor, name: &str) -> String {
    match executor.scope().get_scoped(ScopeKind::Global, name.as_bytes(), 0) {
        Ok(Typval::String(s)) => s.to_string_lossy().into_owned(),
        Ok(other) => panic!("g:{name} is {other:?}, expected String"),
        Err(_) => panic!("g:{name} is undefined"),
    }
}

/// Assert that a `g:` variable is undefined (E121).
fn gundefined(executor: &ExExecutor, name: &str) {
    assert!(
        executor.scope().get_scoped(ScopeKind::Global, name.as_bytes(), 0).is_err(),
        "g:{name} should be undefined"
    );
}

/// Extract the Vim exception from an Err result, panicking if not Vim.
fn vim_error<T>(result: Result<T, ExecError>) -> crate::VimException {
    match result {
        Err(ExecError::Vim(exception)) => exception,
        Err(other) => panic!("expected ExecError::Vim, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

// ===========================================================================
// if / elseif / else nesting
// (ex_docmd.c: ex_if/ex_elseif/ex_else — branch selection, E171 missing endif)
// ===========================================================================

#[test]
fn if_true_branch_executes_and_sets_global() {
    // ex_docmd.c: `:if` evaluates condition; truthy branch body runs.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(&mut editor, "test.vim", "if 1\nlet g:x = 42\nendif")
        .unwrap();
    assert_eq!(gnum(&executor, "x"), 42);
}

#[test]
fn elseif_chain_picks_first_true_branch() {
    // ex_docmd.c: `:elseif` conditions are evaluated in order; the first
    // truthy one wins and later elseif bodies are skipped.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "if 0\nlet g:a = 1\nelseif 1\nlet g:b = 2\nelseif 1\nlet g:c = 3\nendif",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "b"), 2);
    gundefined(&executor, "a");
    gundefined(&executor, "c");
}

#[test]
fn else_runs_when_all_conditions_false() {
    // ex_docmd.c: `:else` is the fallback when no if/elseif condition matches.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "if 0\nlet g:a = 1\nelseif 0\nlet g:b = 1\nelse\nlet g:z = 99\nendif",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "z"), 99);
    gundefined(&executor, "a");
    gundefined(&executor, "b");
}

#[test]
fn nested_if_inside_if_evaluates_inner_branch() {
    // ex_docmd.c: nested `:if` blocks are independent; the inner condition
    // is only evaluated when the outer branch is active.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "if 1\nif 0\nlet g:inner_bad = 1\nelse\nlet g:inner_good = 1\nendif\nendif",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "inner_good"), 1);
    gundefined(&executor, "inner_bad");
}

// ===========================================================================
// while / for iteration
// (ex_docmd.c: ex_while/ex_for — loop condition/iterator evaluation)
// ===========================================================================

#[test]
fn while_loop_counts_iterations() {
    // ex_docmd.c: `:while` re-evaluates the condition before each iteration;
    // the body runs until the condition is falsy.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:i = 0\nwhile g:i < 3\nlet g:i += 1\nendwhile",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "i"), 3);
}

#[test]
fn for_loop_sums_list_elements() {
    // ex_docmd.c: `:for` iterates list elements, assigning each to the loop
    // variable before executing the body.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:sum = 0\nfor x in [1, 2, 3, 4]\nlet g:sum += x\nendfor",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "sum"), 10);
}

#[test]
fn list_targets_destructure_let_and_for_values() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:commented = 'ok' \" inline comment\nlet [x, y] = [3, 4]\nlet g:sum = x + y\nfor [left, right] in [[1, 2], [5, 8]]\nlet g:last = left + right\nendfor",
        )
        .unwrap();
    assert_eq!(gstr(&executor, "commented"), "ok");
    assert_eq!(gnum(&executor, "sum"), 7);
    assert_eq!(gnum(&executor, "last"), 13);
}

#[test]
fn for_loop_sets_last_element_as_string() {
    // ex_docmd.c: `:for` over a string list assigns each Typval::String to
    // the loop variable; the last value persists after the loop.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "for item in [\"alpha\", \"beta\", \"gamma\"]\nlet g:last = item\nendfor",
        )
        .unwrap();
    assert_eq!(gstr(&executor, "last"), "gamma");
}

// ===========================================================================
// break / continue
// (ex_docmd.c: ex_break/ex_continue — Flow::Break/Flow::Continue)
// ===========================================================================

#[test]
fn break_exits_while_loop_early() {
    // ex_docmd.c: `:break` terminates the innermost `:while`/`:for`.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:i = 0\nwhile 1\nif g:i >= 2\nbreak\nendif\nlet g:i += 1\nendwhile",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "i"), 2);
}

#[test]
fn continue_skips_rest_of_while_body() {
    // ex_docmd.c: `:continue` jumps to the next loop iteration, skipping the
    // remaining body statements.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:i = 0\nlet g:count = 0\nwhile g:i < 4\nlet g:i += 1\nif g:i == 2\ncontinue\nendif\nlet g:count += 1\nendwhile",
        )
        .unwrap();
    // i goes 1(count=1), 2(continue), 3(count=2), 4(count=3)
    assert_eq!(gnum(&executor, "count"), 3);
    assert_eq!(gnum(&executor, "i"), 4);
}

#[test]
fn break_exits_for_loop_early() {
    // ex_docmd.c: `:break` inside `:for` stops iteration immediately.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:sum = 0\nfor x in [1, 2, 3, 4, 5]\nif x == 3\nbreak\nendif\nlet g:sum += x\nendfor",
        )
        .unwrap();
    // Only x=1 and x=2 are summed before break at x=3.
    assert_eq!(gnum(&executor, "sum"), 3);
}

// ===========================================================================
// Inactive branches
// (ex_docmd.c: only the selected if/elseif branch body is executed;
//  inactive branch conditions are not evaluated)
// ===========================================================================

#[test]
fn inactive_if_branch_body_not_executed() {
    // ex_docmd.c: when `:if` condition is falsy and there is no `:else`,
    // the body is skipped entirely.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(&mut editor, "test.vim", "if 0\nlet g:bad = 1\nendif")
        .unwrap();
    gundefined(&executor, "bad");
}

#[test]
fn inactive_function_branch_does_not_resolve_unknown_command() {
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "function Run()\nif 0\nechoconsole 'unreachable'\nendif\nlet g:inside = 3\nendfunction\ncall Run()\nlet g:after = 4",
        )
        .unwrap();

    assert_eq!(gnum(&executor, "inside"), 3);
    assert_eq!(gnum(&executor, "after"), 4);
}

#[test]
fn selected_function_branch_resolves_unknown_command_to_e492() {
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let exception = vim_error(executor.execute_script(
        &mut editor,
        "test.vim",
        "function Run()\nif 1\nechoconsole 'reached'\nendif\nendfunction\ncall Run()",
    ));

    assert_eq!(exception.kind, VimExceptionKind::Error("E492".to_owned()));
}

#[test]
fn inactive_elseif_condition_not_evaluated() {
    // ex_docmd.c: once a truthy `:if`/`:elseif` branch is found, subsequent
    // `:elseif` conditions are NOT evaluated — referencing an undefined
    // variable in a skipped elseif must NOT raise E121.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "if 1\nlet g:ran = 1\nelseif g:undefined\nlet g:bad = 1\nendif",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "ran"), 1);
    gundefined(&executor, "bad");
}

// ===========================================================================
// try / catch regex matching
// (ex_docmd.c: ex_catch — catch pattern is a Vim regex compiled as-is; it may
//  match anywhere in the exception message. An authored `^` anchors it.
//  test_trycatch.vim: catch pattern cases)
// ===========================================================================

#[test]
fn catch_regex_matches_thrown_string() {
    // ex_docmd.c: `:catch /pattern/` matches when the pattern (searched
    // through the exception message) matches.
    // test_trycatch.vim: `:throw "MyError" | catch /MyError/`
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\nthrow \"MyError\"\ncatch /MyError/\nlet g:caught = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "caught"), 1);
}

#[test]
fn catch_unanchored_pattern_matches_suffix() {
    // test_trycatch.vim: `:throw "prefix-suffix"` is caught by `/suffix/`.
    // Patterns compile with search semantics; only an authored `^` anchors.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\nthrow \"prefix-suffix\"\ncatch /suffix/\nlet g:caught = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "caught"), 1);
}

#[test]
fn catch_anchored_pattern_requires_exact_start() {
    // An authored `^` anchors the catch pattern; `/^suffix$/` does not match
    // `prefix-suffix`.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_script(
        &mut editor,
        "test.vim",
        "try\nthrow \"prefix-suffix\"\ncatch /^suffix$/\nlet g:wrong = 1\nendtry",
    );
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Throw);
    assert_eq!(exception.message(), "prefix-suffix");
    gundefined(&executor, "wrong");
}

#[test]
fn catch_non_match_falls_through_to_next_catch() {
    // ex_docmd.c: multiple `:catch` blocks are tried in order; the first
    // matching pattern wins, non-matching ones are skipped.
    // test_trycatch.vim: sequential catch patterns
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\nthrow \"foo\"\ncatch /bar/\nlet g:wrong = 1\ncatch /foo/\nlet g:caught = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "caught"), 1);
    gundefined(&executor, "wrong");
}

#[test]
fn catch_matches_error_code_from_undefined_function_call() {
    // ex_docmd.c: an editor error (E117) produces an exception whose message
    // starts with the error code; `:catch /E117/` matches it.
    // test_trycatch.vim: catching error-code exceptions
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\ncall NoSuchFunction()\ncatch /E117/\nlet g:caught = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "caught"), 1);
}

// ===========================================================================
// Bare catch
// (ex_docmd.c: `:catch` with no pattern catches every exception;
//  test_trycatch.vim: bare catch cases)
// ===========================================================================

#[test]
fn bare_catch_catches_any_throw() {
    // ex_docmd.c: `:catch` without a pattern is an unconditional catch.
    // test_trycatch.vim: `:throw "x" | catch | ...`
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\nthrow \"anything\"\ncatch\nlet g:caught = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "caught"), 1);
}

#[test]
fn bare_catch_after_failed_pattern_catches_remaining() {
    // ex_docmd.c: a bare `:catch` after a non-matching pattern catch acts
    // as the fallback for any exception the earlier patterns missed.
    // test_trycatch.vim: pattern catch followed by bare catch
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\nthrow \"zzz\"\ncatch /aaa/\nlet g:wrong = 1\ncatch\nlet g:caught = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "caught"), 1);
    gundefined(&executor, "wrong");
}

// ===========================================================================
// finally ordering across throw / break
// (ex_eval.c: finally always executes; if finally completes normally the
//  pending flow propagates; test_trycatch.vim: finally ordering)
// ===========================================================================

#[test]
fn finally_runs_after_normal_try_completion() {
    // ex_eval.c: `:finally` executes even when the try body completes without
    // exception.
    // test_trycatch.vim: finally after normal try
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\nlet g:x = 1\nfinally\nlet g:final = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "x"), 1);
    assert_eq!(gnum(&executor, "final"), 1);
}

#[test]
fn finally_runs_after_catch_handles_throw() {
    // ex_eval.c: when a `:catch` handles the exception, `:finally` still
    // runs before control leaves the try block.
    // test_trycatch.vim: finally after catch
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "try\nthrow \"err\"\ncatch /err/\nlet g:caught = 1\nfinally\nlet g:final = 1\nendtry",
        )
        .unwrap();
    assert_eq!(gnum(&executor, "caught"), 1);
    assert_eq!(gnum(&executor, "final"), 1);
}

#[test]
fn finally_runs_and_uncaught_exception_propagates() {
    // ex_eval.c: when no `:catch` matches, `:finally` still runs, and the
    // exception propagates out of the try block as an error.
    // test_trycatch.vim: uncaught exception with finally
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_script(
        &mut editor,
        "test.vim",
        "try\nthrow \"boom\"\ncatch /nope/\nlet g:wrong = 1\nfinally\nlet g:final = 1\nendtry",
    );
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Throw);
    assert_eq!(exception.message(), "boom");
    assert_eq!(gnum(&executor, "final"), 1);
    gundefined(&executor, "wrong");
}

#[test]
fn finally_runs_after_break_exits_loop() {
    // ex_eval.c: `:break` inside `:try` is a pending flow; `:finally` runs
    // before the break propagates out to terminate the loop.
    // test_trycatch.vim: finally with break
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    executor
        .execute_script(
            &mut editor,
            "test.vim",
            "let g:final = 0\nlet g:ran = 0\nfor x in [1, 2, 3]\ntry\nif x == 2\nbreak\nendif\nlet g:ran = x\nfinally\nlet g:final = x\nendtry\nendfor",
        )
        .unwrap();
    // x=1: ran=1, final=1. x=2: break, final=2. Loop exits.
    assert_eq!(gnum(&executor, "ran"), 1);
    assert_eq!(gnum(&executor, "final"), 2);
}

// ===========================================================================
// Uncaught throw + exception message/throwpoint shape
// (ex_docmd.c: throwpoint format "function F[N]..script path[M]" or
//  "command line"; ex_eval.c: uncaught throw aborts execution)
// ===========================================================================

#[test]
fn uncaught_throw_returns_vim_error_with_message_and_throwpoint() {
    // ex_docmd.c: `:throw` creates a VimException with kind=Throw; uncaught,
    // it becomes ExecError::Vim.  Throwpoint for `:execute_line` is
    // "command line" (no source stack frame).
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_line(&mut editor, "throw \"oops\"");
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Throw);
    assert_eq!(exception.message(), "oops");
    assert_eq!(exception.throwpoint, "command line");
}

#[test]
fn script_throw_has_script_throwpoint_with_line_number() {
    // ex_docmd.c: throwpoint for a sourced script is "script name[line]".
    // The line number is the physical source line of the `:throw`.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_script(&mut editor, "my.vim", "let g:dummy = 0\nthrow \"err\"");
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Throw);
    assert_eq!(exception.message(), "err");
    assert_eq!(exception.throwpoint, "script my.vim[2]");
}

// ===========================================================================
// Malformed control structure errors
// (ex_docmd.c: E171 "Missing :endif", E600 "Missing :endtry";
//  ex_eval.c: E170 "Missing :endwhile")
// ===========================================================================

#[test]
fn missing_endif_produces_e171_error() {
    // ex_docmd.c: `:if` without a matching `:endif` raises E171. The oracle
    // reports `Vim:E171` and not `Vim(if):`, because `do_cmdline` notices the
    // missing closer after its loop, where no command is current.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_script(&mut editor, "test.vim", "if 1\necho \"hi\"");
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Error("E171".to_owned()));
    assert_eq!(exception.message(), "Vim:E171: Missing :endif");
}

#[test]
fn missing_endtry_produces_e600_error() {
    // ex_docmd.c: `:try` without a matching `:endtry` raises E600.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_script(&mut editor, "test.vim", "try\nthrow \"x\"");
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Error("E600".to_owned()));
    assert_eq!(exception.message(), "Vim:E600: Missing :endtry");
}

// ===========================================================================
// `v:exception`'s `Vim({cmdname}):` prefix and `append_command` suffix
// (ex_eval.c:383-401 get_exception_string, ex_docmd.c:2375-2384,2993-3019
//  append_command). Every string below was read from
// `.references/neovim/build/bin/nvim` first.
// ===========================================================================

/// An error escaping a builtin Ex command is prefixed with that command's
/// *canonical* name, so an abbreviation still reports the full name, and a
/// command implementation's own error carries no command line after it.
#[test]
fn an_error_is_prefixed_with_the_command_it_escaped_from() {
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();

    // Oracle: `Vim(echo):E121: Undefined variable: g:nope`.
    let exception = vim_error(executor.execute_line(&mut editor, "echo g:nope").map(|_| ()));
    assert_eq!(exception.message(), "Vim(echo):E121: Undefined variable: g:nope");

    // Oracle: `:unm` resolves to `unmap`, and the prefix names the canonical
    // command, not what was typed — `Vim(unmap):E31: No such mapping`.
    let exception = vim_error(executor.execute_line(&mut editor, "unm ,zzz").map(|_| ()));
    assert_eq!(exception.message(), "Vim(unmap):E31: No such mapping");
}

/// `:throw` is a user exception: its value is reported verbatim, with no
/// prefix at all (`get_exception_string`'s ET_USER branch).
#[test]
fn an_explicit_throw_keeps_its_value_unprefixed() {
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let exception = vim_error(executor.execute_line(&mut editor, "throw 'boom'").map(|_| ()));
    assert_eq!(exception.message(), "boom");
}

/// `append_command` echoes the command line verbatim after the message — but
/// only for the errors `do_one_cmd` raises while reading it. Leading
/// whitespace, quotes and bars all survive, because it copies bytes.
#[test]
fn a_command_line_error_echoes_the_line_it_could_not_read() {
    // Oracle: `Vim(print):E16: Invalid range:   99print` — the two spaces of
    // indent are part of the line and are echoed.
    let (mut editor, mut executor) = editor_with_buffer();
    executor
        .execute_script(&mut editor, "t.vim", "try\n  99print\ncatch\n  let g:e = v:exception\nendtry")
        .unwrap();
    assert_eq!(
        global_string(&executor, "e").as_deref(),
        Some("Vim(print):E16: Invalid range:   99print")
    );

    // Oracle: `Vim:E492: Not an editor command:   definitelynotacommand`. An
    // unresolvable name has no `cmdidx`, so upstream prefixes `Vim:`.
    let (mut editor, mut executor) = editor_with_buffer();
    executor
        .execute_script(&mut editor, "t.vim", "try\n  definitelynotacommand\ncatch\n  let g:e = v:exception\nendtry")
        .unwrap();
    assert_eq!(
        global_string(&executor, "e").as_deref(),
        Some("Vim:E492: Not an editor command:   definitelynotacommand")
    );

    // Oracle: `Vim(print):E16: Invalid range: 99print " q | b"` — a quote and
    // a bar inside the line are echoed as written, neither escaped nor used as
    // a separator.
    let (mut editor, mut executor) = editor_with_buffer();
    executor
        .execute_script(
            &mut editor,
            "t.vim",
            "try\nexecute '99print \" q | b\"'\ncatch\nlet g:e = v:exception\nendtry",
        )
        .unwrap();
    assert_eq!(
        global_string(&executor, "e").as_deref(),
        Some("Vim(print):E16: Invalid range: 99print \" q | b\"")
    );
}

/// An error a command *implementation* emits reaches `emsg` directly, so
/// `append_command` never runs on it. Oracle: `Vim(foldopen):E490: No fold
/// found` — the prefix is there and nothing follows the message.
#[test]
fn a_command_implementation_error_does_not_echo_the_line() {
    let (mut editor, mut executor) = editor_with_buffer();
    executor
        .execute_script(&mut editor, "t.vim", "try\n  foldopen\ncatch\n  let g:e = v:exception\nendtry")
        .unwrap();
    assert_eq!(global_string(&executor, "e").as_deref(), Some("Vim(foldopen):E490: No fold found"));
}

/// An editor with one listed buffer shown in one window, which the commands
/// above need in order to reach their own error rather than E749.
fn editor_with_buffer() -> (Editor, ExExecutor) {
    let mut editor = Editor::new();
    let buffer = editor.create_buffer(true).unwrap();
    editor
        .create_tabpage(buffer, crate::Geometry::new(0, 0, 80, 24).unwrap())
        .unwrap();
    (editor, ExExecutor::new())
}

/// Reads a global as a plain string.
fn global_string(executor: &ExExecutor, name: &str) -> Option<String> {
    executor
        .scope()
        .global
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .and_then(|(_, value)| match value {
            ox_types::Typval::String(text) => Some(text.to_string_lossy().into_owned()),
            _ => None,
        })
}

// ===========================================================================
// Trailing garbage after an expression argument
//
// Two rules meet here, and either one alone gives the wrong answer:
//
//   1. White space is what `skipwhite`/`del_trailing_spaces` call white space
//      (`strings.c:429-446`, `ascii_defs.h:84-87`): ASCII space and tab. Rust's
//      `str::trim` also removes CR, VT, FF, NL and every Unicode space, and
//      `u8::is_ascii_whitespace` also removes CR, NL and FF, so both silently
//      eat the bytes upstream keeps.
//   2. The remaining bytes have to reach `eval0`'s trailing check as
//      *remainder*, not as a lexing failure (`eval.c:1234-1252`,
//      `errors.h:123` `e_trailing_arg`).
//
// Every expectation below was read off `nvim` v0.13.0-dev-1390 one probe per
// process, the error taken from `v:exception`.
// ===========================================================================

/// Assert one Ex line raises `code`, and hand back the message.
fn line_error(line: &str, code: &str) -> String {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    let exception = vim_error(executor.execute_line(&mut editor, line));
    assert_eq!(exception.kind, VimExceptionKind::Error(code.to_owned()), "for {line:?}");
    exception.message()
}

/// The whole class in one assertion, and the reason it takes both rules: with
/// only the white-space rule the CR reaches an eager lexer and the answer is
/// E15; with only a tolerant lexer the CR never survives `str::trim` and there
/// is no error at all. Oracle: `Vim(let):E488: Trailing characters: <CR>`.
#[test]
fn trailing_carriage_return_after_a_let_expression_is_e488() {
    assert_eq!(line_error("let g:v = 4\r", "E488"), "Vim(let):E488: Trailing characters: \r");
}

/// The white-space rule on its own: a form feed is `is_ascii_whitespace` and a
/// vertical tab is Unicode white space, so an "ASCII white space" or a
/// `str::trim` spelling of the rule swallows one or both. Neither is
/// `skipwhite`. Oracle: both are `E488: Trailing characters:`.
#[test]
fn vertical_tab_and_form_feed_are_not_white_space_to_skipwhite() {
    assert_eq!(line_error("let g:v = 4\x0b", "E488"), "Vim(let):E488: Trailing characters: \x0b");
    assert_eq!(line_error("let g:v = 4\x0c", "E488"), "Vim(let):E488: Trailing characters: \x0c");
}

/// The tolerant-lexer rule on its own: `'ab` is not white space under any
/// spelling of rule 1, so only rule 2 decides. An eager lexer answers E115 for
/// the unterminated string it had no business reading.
/// Oracle: `Vim(let):E488: Trailing characters: 'ab`.
#[test]
fn an_unterminated_string_after_a_complete_expression_is_remainder() {
    assert_eq!(line_error("let g:v = 4 'ab", "E488"), "Vim(let):E488: Trailing characters: 'ab");
}

/// And the rule is still `skipwhite`, not "no trimming": space and tab around
/// the target, the operator and the expression are still white space, and a
/// compound operator still survives the split intact.
#[test]
fn space_and_tab_around_an_assignment_are_still_white_space() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor.execute_line(&mut editor, "let g:v\t=  4 \t").unwrap();
    assert_eq!(gnum(&executor, "v"), 4);
    executor.execute_line(&mut editor, "let g:v  +=\t3").unwrap();
    assert_eq!(gnum(&executor, "v"), 7);
}

/// One row per dispatch arm that hands an expression to `eval_text`, because
/// each arm trimmed its own argument and so each one has to be pinned: putting
/// `str::trim` back at any single call site fails exactly one of these.
#[test]
fn every_expression_command_rejects_a_trailing_carriage_return() {
    assert_eq!(line_error("const g:c = 4\r", "E488"), "Vim(const):E488: Trailing characters: \r");
    assert_eq!(line_error("eval 4\r", "E488"), "Vim(eval):E488: Trailing characters: \r");
    assert_eq!(line_error("throw 'a'\r", "E488"), "Vim(throw):E488: Trailing characters: \r");
    assert_eq!(line_error("call len('a')\r", "E488"), "Vim(call):E488: Trailing characters: \r");
    assert_eq!(line_error("unlet g:z\r", "E488"), "Vim(unlet):E488: Trailing characters: \r");
}

/// Two sites whose argument is only *tested* for emptiness or cut at a
/// comment, so the rows above leave them free: `:return`'s "did I get an
/// expression at all" check, and the `" comment` cut that runs before the
/// expression reaches `eval0`. Under `str::trim` a bare `:return<CR>` looks
/// like a plain `:return` and quietly returns 0, and `4<CR> "c"` loses the CR
/// with the comment.
#[test]
fn the_emptiness_test_and_the_comment_cut_see_the_carriage_return_too() {
    // Oracle: `Vim(return):E15: Invalid expression:` — an argument was given.
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    let exception = vim_error(executor.execute_script(
        &mut editor,
        "t.vim",
        "function! F()\nreturn \r\nendfunction\ncall F()",
    ));
    assert_eq!(exception.kind, VimExceptionKind::Error("E15".to_owned()));

    // A bare `:return` with nothing after it is still a bare `:return`.
    executor
        .execute_script(&mut editor, "t.vim", "function! G()\nreturn \nendfunction\nlet g:r = G()")
        .unwrap();
    assert_eq!(gnum(&executor, "r"), 0);

    // Oracle: `Vim(let):E488: Trailing characters: <CR> "c"`. This port names
    // only the CR, because the comment is cut before `eval0` sees it.
    assert_eq!(line_error("let g:v = 4\r \"c\"", "E488"), "Vim(let):E488: Trailing characters: \r");
    // ...and an ordinary trailing comment is still a comment.
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor.execute_line(&mut editor, "let g:v = 4 \"c\"").unwrap();
    assert_eq!(gnum(&executor, "v"), 4);
}

/// The block-opening commands read their condition from a different place
/// (`find_if`, the `:while` loop head, `split_for`), so they need their own
/// rows. Oracle: `Vim(if)`, `Vim(while)` and `Vim(for)` all raise E488.
#[test]
fn block_openers_reject_a_trailing_carriage_return_in_the_condition() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    let exception = vim_error(executor.execute_script(&mut editor, "t.vim", "if 1\r\nendif"));
    assert_eq!(exception.kind, VimExceptionKind::Error("E488".to_owned()));

    let exception = vim_error(executor.execute_script(&mut editor, "t.vim", "while 0\r\nendwhile"));
    assert_eq!(exception.kind, VimExceptionKind::Error("E488".to_owned()));

    let exception = vim_error(executor.execute_script(&mut editor, "t.vim", "for i in [1]\r\nendfor"));
    assert_eq!(exception.kind, VimExceptionKind::Error("E488".to_owned()));

    let exception = vim_error(executor.execute_script(
        &mut editor,
        "t.vim",
        "function! F()\nreturn 4\r\nendfunction\ncall F()",
    ));
    assert_eq!(exception.kind, VimExceptionKind::Error("E488".to_owned()));
}

/// `:echo`, `:echomsg` and `:execute` loop `eval1` until the line is spent, so
/// they do reach the refused byte and answer E15 rather than E488
/// (`eval.c:1846`). A blanket "refused byte means E488" would break this row.
/// Oracle: `Vim(echo):E15: Invalid expression: ...`.
#[test]
fn echo_family_reports_e15_not_e488_for_a_trailing_carriage_return() {
    line_error("echo 'z'\r", "E15");
    line_error("echomsg 'q'\r", "E15");
    line_error("execute 'let g:v = 5'\r", "E15");
}

/// A `-nargs=0` user command clears `EX_EXTRA`, so `do_one_cmd` rejects any
/// argument text at all before the body runs (`ex_docmd.c:4542`). The port
/// answered E471 "Argument required", which is the opposite complaint.
/// Oracle: `Vim:E488: Trailing characters: x`; `-nargs=1` with no argument
/// stays E471.
#[test]
fn a_nargs_zero_user_command_rejects_any_argument_with_e488() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor.execute_line(&mut editor, "command! -nargs=0 T73D let g:v = 4").unwrap();
    executor.execute_line(&mut editor, "command! -nargs=1 T73N let g:v = 4").unwrap();

    let exception = vim_error(executor.execute_line(&mut editor, "T73D x"));
    assert_eq!(exception.kind, VimExceptionKind::Error("E488".to_owned()));
    let exception = vim_error(executor.execute_line(&mut editor, "T73D\r"));
    assert_eq!(exception.kind, VimExceptionKind::Error("E488".to_owned()));
    let exception = vim_error(executor.execute_line(&mut editor, "T73N"));
    assert_eq!(exception.kind, VimExceptionKind::Error("E471".to_owned()));
}

/// The sourced-line reader stripped a trailing CR from every line, which hid
/// the whole class from any file-based probe. `get_one_sourceline`
/// (`runtime.c:2891-2905`) strips it only for an `EOL_DOS` file, and that
/// branch is inside `#ifdef USE_CRNL` — a Windows-only define. Oracle on this
/// platform: even a wholly CRLF script is `E488` on the first line.
#[test]
fn a_sourced_line_keeps_its_trailing_carriage_return() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    let exception = vim_error(executor.execute_script(&mut editor, "t.vim", "let g:v = 4\r\n"));
    assert_eq!(exception.kind, VimExceptionKind::Error("E488".to_owned()));

    // A script with no stray CR is untouched by the change.
    executor.execute_script(&mut editor, "t.vim", "let g:w = 7\n").unwrap();
    assert_eq!(gnum(&executor, "w"), 7);
}

/// `execute()` with a List runs each item as its own source line
/// (`execute_common` hands `do_cmdline` a `get_list_line` cookie,
/// `eval/funcs.c:1206-1216`). Stringifying the list instead made every
/// multi-line construct `E492: Not an editor command: ['if 1', ...]`, which is
/// also what stopped `:if`/`:while`/`:for` from being measurable at all.
#[test]
fn execute_runs_a_list_argument_line_by_line() {
    let mut editor = Editor::new();
    let mut executor = ExExecutor::new();
    executor
        .execute_line(&mut editor, "call execute(['if 1', 'let g:v = 9', 'endif'])")
        .unwrap();
    assert_eq!(gnum(&executor, "v"), 9);

    // The single-string form still runs as one command line.
    executor.execute_line(&mut editor, "call execute('let g:s = 3')").unwrap();
    assert_eq!(gnum(&executor, "s"), 3);
}
