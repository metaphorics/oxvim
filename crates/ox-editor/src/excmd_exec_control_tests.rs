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
    // ex_docmd.c: `:if` without a matching `:endif` raises E171.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_script(&mut editor, "test.vim", "if 1\necho \"hi\"");
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Error("E171".to_owned()));
    assert_eq!(exception.message(), "E171: Missing :endif");
}

#[test]
fn missing_endtry_produces_e600_error() {
    // ex_docmd.c: `:try` without a matching `:endtry` raises E600.
    let mut executor = ExExecutor::new();
    let mut editor = Editor::new();
    let result = executor.execute_script(&mut editor, "test.vim", "try\nthrow \"x\"");
    let exception = vim_error(result.map(|_| ()));
    assert_eq!(exception.kind, VimExceptionKind::Error("E600".to_owned()));
    assert_eq!(exception.message(), "E600: Missing :endtry");
}
