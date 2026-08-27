//! Fold query builtins: `foldclosed`, `foldclosedend` and `foldlevel`.
//!
//! Upstream serves all three from `src/nvim/fold.c:3178-3212`, where
//! `foldclosed_both` resolves `{lnum}` with `tv_get_lnum` and answers the
//! closed fold `hasFoldingWin` (`fold.c:173-263`) finds, and `f_foldlevel`
//! answers `foldLevelWin` (`fold.c:1088-1107`). Both first bring the window's
//! folds up to date through `checkupdate` (`fold.c:1113-1122`) and both report
//! nothing at all when `hasAnyFolding` (`fold.h:23-25`) is false, which
//! `'foldenable'` decides.
//!
//! Named gap: `foldtext()` and `foldtextresult({lnum})` are absent. Both
//! render a fold through the `'foldtext'` option — `f_foldtext` reads
//! `v:foldstart`, `v:foldend` and `v:folddashes`, and `f_foldtextresult` calls
//! `get_foldtext`, which evaluates `'foldtext'` in the fold's context and then
//! strips `'foldmarker'` and `'commentstring'` in `foldtext_cleanup`
//! (`fold.c:3214-3301`, `fold.c:1681-1750`). This port has neither those three
//! `v:` variables nor the `'foldtext'` option, so neither builtin can be
//! answered without inventing a string, and they stay unimplemented.

use ox_eval::EvalError;
use ox_types::{BufHandle, Typval};

use crate::excmd_exec::{buffer_lines, EvalHost};
use crate::fold::{FoldMethod, Folds};
use crate::options::OptionValue;
use crate::script::FileIO;
use crate::Editor;

/// Routes one fold query builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    check_arity(name, args.len())?;
    let editor = &mut *host.editor;
    // `tv_get_lnum` runs before the range check in every one of the three, so
    // an argument with no numeric value raises its error whatever the folds
    // hold.
    let lnum = super::position::current_lnum_arg(editor, &args[0])?;
    let value = match name {
        "foldclosed" => closed_line(editor, lnum, false),
        "foldclosedend" => closed_line(editor, lnum, true),
        "foldlevel" => level(editor, lnum),
        _ => unreachable!("fold builtin route and dispatcher disagree"),
    };
    Ok(Typval::Number(value))
}

/// Enforces the `eval.lua` argument counts the way upstream's function table
/// does before a builtin body runs.
fn check_arity(name: &str, count: usize) -> ox_eval::Result<()> {
    let spec = ox_eval::builtin_spec(name).expect("fold builtins come from eval.lua");
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

/// `foldclosed_both` (`fold.c:3179-3191`): the first or last line of the closed
/// fold containing `lnum`, and `-1` when the line holds no closed fold or lies
/// outside the buffer.
fn closed_line(editor: &mut Editor, lnum: i64, end: bool) -> i64 {
    let Some((folds, line_count)) = synced_folds(editor) else {
        return -1;
    };
    if lnum < 1 || lnum > line_count as i64 {
        return -1;
    }
    let Some((first, last)) = folds.closed_rows_at(lnum as usize - 1) else {
        return -1;
    };
    if end {
        // `last = MIN(last, ml_line_count)` (`fold.c:250`).
        (last as i64 + 1).min(line_count as i64)
    } else {
        first as i64 + 1
    }
}

/// `f_foldlevel` (`fold.c:3206-3212`): the nesting level at `lnum`, and `0`
/// when no fold covers it or it lies outside the buffer.
fn level(editor: &mut Editor, lnum: i64) -> i64 {
    let Some((folds, line_count)) = synced_folds(editor) else {
        return 0;
    };
    if lnum < 1 || lnum > line_count as i64 {
        return 0;
    }
    folds.level_at_row(lnum as usize - 1) as i64
}

/// `checkupdate` (`fold.c:1113-1122`): brings the current buffer's folds up to
/// date under the current window's fold options and answers them with the
/// buffer's line count. `None` reproduces `hasAnyFolding` being false: no
/// window, no buffer, or `'foldenable'` off.
///
/// Named gaps, all of them upstream behaviour this port has no seam for:
/// `'foldmethod'` of `expr`, `syntax` or `diff` needs a host computation
/// [`Folds::refresh`] can only request, so those methods answer from whatever
/// a host last applied — nothing, by default. `'foldnestmax'` does not cap
/// computed levels and `'foldignore'` does not exclude lines, so an indent
/// fold deeper than `'foldnestmax'` or starting at an ignored line is reported
/// where upstream would not report it. `'foldlevel'` does not close computed
/// folds by level either; a computed fold starts closed, which is what
/// `'foldlevel'` at its default of zero produces.
fn synced_folds(editor: &mut Editor) -> Option<(&Folds, usize)> {
    let window = editor.current_window()?;
    let buffer = editor.current_buffer()?;
    if !matches!(
        editor.options().get_window(window, "foldenable"),
        Ok(OptionValue::Boolean(true))
    ) {
        return None;
    }
    let method = match editor.options().get_window(window, "foldmethod") {
        Ok(OptionValue::String(value)) => FoldMethod::from_option_value(value),
        _ => FoldMethod::Manual,
    };
    let shift_width = effective_shift_width(editor, buffer);
    let lines = buffer_lines(editor, buffer).ok()?;
    let line_count = lines.len();
    let state = editor.buffer_mut(buffer).ok()?;
    let changedtick = state.changedtick();
    state.folds.set_method(method);
    let _ = state.folds.set_shift_width(shift_width);
    // A host request or a computation error leaves the cached set answering,
    // which is the closest honest reading of folds this port cannot compute.
    let _ = state.folds.refresh(changedtick, &lines);
    Some((&state.folds, line_count))
}

/// `get_sw_value_col` (`indent.c:362-366`): `'shiftwidth'`, or `'tabstop'`
/// when `'shiftwidth'` is zero. Never zero, which [`Folds::set_shift_width`]
/// rejects.
fn effective_shift_width(editor: &Editor, buffer: BufHandle) -> usize {
    let read = |name| match editor.options().get_buffer(buffer, name) {
        Ok(OptionValue::Number(value)) => usize::try_from(*value).unwrap_or(0),
        _ => 0,
    };
    match read("shiftwidth") {
        0 => read("tabstop").max(1),
        width => width,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Every expectation below is the value the oracle at
    //! `.references/neovim/build/bin/nvim` (v0.13.0-dev-1390) answers for the
    //! same script, recorded next to each assertion.

    use ox_eval::ScopeKind;

    use super::*;
    use crate::excmd_exec::ExecError;
    use crate::{ExExecutor, Geometry, VimExceptionKind};

    /// An editor with one listed buffer shown in one 80x24 window.
    fn editor() -> Editor {
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        editor
    }

    /// Runs `script` against a six-line buffer and answers the numeric globals
    /// `names` in order.
    fn numbers(script: &str, names: &[&str]) -> Vec<i64> {
        let mut editor = editor();
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &mut editor,
            "fold.vim",
            &format!("call setline(1, ['a','b','c','d','e','f'])\n{script}"),
        )
        .unwrap();
        names
            .iter()
            .map(|name| {
                match exec
                    .scope()
                    .get_scoped(ScopeKind::Global, name.as_bytes(), 0)
                    .unwrap_or_else(|error| panic!("no g:{name}: {error:?}"))
                {
                    Typval::Number(value) => *value,
                    other => panic!("expected a Number in g:{name}, got {other:?}"),
                }
            })
            .collect()
    }

    /// The Vim error code a script raises.
    fn error_code(script: &str) -> String {
        let mut editor = editor();
        let mut exec = ExExecutor::new();
        match exec.execute_script(&mut editor, "fold.vim", script) {
            Err(ExecError::Vim(exception)) => match exception.kind {
                VimExceptionKind::Error(code) => code,
                other => panic!("expected an error exception, got {other:?}"),
            },
            other => panic!("expected a Vim error, got {other:?}"),
        }
    }

    // fold.c:3179-3191 — `:2,4fold` closes lines 2 to 4, so every line in it
    // answers the fold's first line and every line outside answers -1.
    // Oracle: fc1=-1 fc2=2 fc4=2 fc5=-1.
    #[test]
    fn foldclosed_answers_the_first_line_of_the_closed_fold() {
        let values = numbers(
            "2,4fold\n\
             let g:a = foldclosed(1)\n\
             let g:b = foldclosed(2)\n\
             let g:c = foldclosed(4)\n\
             let g:d = foldclosed(5)",
            &["a", "b", "c", "d"],
        );
        assert_eq!(values, vec![-1, 2, 2, -1]);
    }

    // fold.c:3179-3191 — the same fold starting on line 1, the first boundary
    // the buffer has. Oracle (`:1,2fold`): fc1=1 fce1=2 fl1=1.
    #[test]
    fn foldclosed_reports_a_fold_starting_on_the_first_line() {
        let values = numbers(
            "1,2fold\n\
             let g:a = foldclosed(1)\n\
             let g:b = foldclosedend(1)\n\
             let g:c = foldlevel(1)",
            &["a", "b", "c"],
        );
        assert_eq!(values, vec![1, 2, 1]);
    }

    // fold.c:3179-3191 — `tv_get_lnum` accepts the `getline()` address forms:
    // "." is the cursor line, "$" the last line, and a numeric string its
    // number. Oracle (cursor on line 3): dot=2 dollar=-1 str=2 fldot=1.
    #[test]
    fn foldclosed_resolves_the_address_forms() {
        let values = numbers(
            "2,4fold\n\
             call cursor(3, 1)\n\
             let g:a = foldclosed('.')\n\
             let g:b = foldclosed('$')\n\
             let g:c = foldclosed('4')\n\
             let g:d = foldlevel('.')",
            &["a", "b", "c", "d"],
        );
        assert_eq!(values, vec![2, -1, 2, 1]);
    }

    // fold.c:3182 — a line outside `[1, ml_line_count]` is answered without
    // looking at the folds at all. Oracle: fc0=-1 fc99=-1 fl0=0 fl99=0.
    #[test]
    fn fold_queries_reject_lines_outside_the_buffer() {
        let values = numbers(
            "2,4fold\n\
             let g:a = foldclosed(0)\n\
             let g:b = foldclosed(99)\n\
             let g:c = foldlevel(0)\n\
             let g:d = foldlevel(99)",
            &["a", "b", "c", "d"],
        );
        assert_eq!(values, vec![-1, -1, 0, 0]);
    }

    // fold.c:3200-3203 — `foldclosedend` answers the fold's last line from any
    // line in it, and -1 from outside. Oracle: fce2=4 fce4=4 fce1=-1.
    #[test]
    fn foldclosedend_answers_the_last_line_of_the_closed_fold() {
        let values = numbers(
            "2,4fold\n\
             let g:a = foldclosedend(2)\n\
             let g:b = foldclosedend(4)\n\
             let g:c = foldclosedend(1)",
            &["a", "b", "c"],
        );
        assert_eq!(values, vec![4, 4, -1]);
    }

    // fold.c:250 — `last` is capped at `ml_line_count`, the last-line
    // boundary. Oracle (`:4,6fold` in a six-line buffer): fc6=4 fce6=6 fce4=6.
    #[test]
    fn foldclosedend_stops_at_the_last_line_of_the_buffer() {
        let values = numbers(
            "4,6fold\n\
             let g:a = foldclosed(6)\n\
             let g:b = foldclosedend(6)\n\
             let g:c = foldclosedend(4)",
            &["a", "b", "c"],
        );
        assert_eq!(values, vec![4, 6, 6]);
    }

    // fold.c:3206-3212 — the nesting level, whatever the fold's state, and 0
    // outside every fold. Oracle (`:2,4fold`): fl1=0 fl2=1 fl4=1 fl5=0.
    #[test]
    fn foldlevel_answers_the_nesting_level() {
        let values = numbers(
            "2,4fold\n\
             let g:a = foldlevel(1)\n\
             let g:b = foldlevel(2)\n\
             let g:c = foldlevel(4)\n\
             let g:d = foldlevel(5)",
            &["a", "b", "c", "d"],
        );
        assert_eq!(values, vec![0, 1, 1, 0]);
    }

    // fold.c:211-238 — the search stops at the outermost closed fold, so a
    // closed inner fold is invisible until the outer one opens, while the level
    // counts both either way. Oracle (`:3,4fold` then `:2,5fold`):
    // fc3=2 fce3=5 fl3=2 fl2=1, and after `:2foldopen`
    // fc2=-1 fc3=3 fce3=4 fl3=2.
    #[test]
    fn nested_folds_report_the_outermost_closed_one() {
        let values = numbers(
            "3,4fold\n\
             2,5fold\n\
             let g:a = foldclosed(3)\n\
             let g:b = foldclosedend(3)\n\
             let g:c = foldlevel(3)\n\
             let g:d = foldlevel(2)\n\
             2foldopen\n\
             let g:e = foldclosed(2)\n\
             let g:f = foldclosed(3)\n\
             let g:g = foldclosedend(3)\n\
             let g:h = foldlevel(3)",
            &["a", "b", "c", "d", "e", "f", "g", "h"],
        );
        assert_eq!(values, vec![2, 5, 2, 1, -1, 3, 4, 2]);
    }

    // fold.h:23-25 — `hasAnyFolding` is false while `'foldenable'` is off, so
    // both queries answer as though the buffer had no folds. Oracle:
    // fc2=-1 fce2=-1 fl2=0, and the fold is still there once 'fen' is back.
    #[test]
    fn foldenable_off_hides_every_fold() {
        let values = numbers(
            "2,4fold\n\
             set nofoldenable\n\
             let g:a = foldclosed(2)\n\
             let g:b = foldclosedend(2)\n\
             let g:c = foldlevel(2)\n\
             set foldenable\n\
             let g:d = foldclosed(2)",
            &["a", "b", "c", "d"],
        );
        assert_eq!(values, vec![-1, -1, 0, 2]);
    }

    // fold.c:1113-1122 — `checkupdate` computes the folds an unattended
    // `'foldmethod'` implies, so an indent fold is observable without any
    // `:fold`. Oracle (`'foldmethod'`=indent, `'shiftwidth'`=2, lines
    // `a`, `  b`, `  c`, `d`, ...): fl2=1 fc2=2 fce2=3 fl1=0.
    #[test]
    fn indent_foldmethod_is_computed_before_the_query() {
        let mut editor = editor();
        let mut exec = ExExecutor::new();
        exec.execute_script(
            &mut editor,
            "fold.vim",
            "call setline(1, ['a','  b','  c','d','e','f'])\n\
             set foldmethod=indent\n\
             set shiftwidth=2\n\
             let g:a = foldlevel(2)\n\
             let g:b = foldclosed(2)\n\
             let g:c = foldclosedend(2)\n\
             let g:d = foldlevel(1)",
        )
        .unwrap();
        let values: Vec<i64> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| {
                match exec
                    .scope()
                    .get_scoped(ScopeKind::Global, name.as_bytes(), 0)
                    .unwrap()
                {
                    Typval::Number(value) => *value,
                    other => panic!("expected a Number, got {other:?}"),
                }
            })
            .collect();
        assert_eq!(values, vec![1, 2, 3, 0]);
    }

    // typval.c:tv_get_number_chk through `tv_get_lnum` — a container has no
    // numeric value and raises before the folds are consulted. Oracle:
    // E745 for a List, E728 for a Dictionary.
    #[test]
    fn fold_queries_reject_arguments_with_no_numeric_value() {
        assert_eq!(error_code("let g:a = foldclosed([])"), "E745");
        assert_eq!(error_code("let g:a = foldclosedend([])"), "E745");
        assert_eq!(error_code("let g:a = foldlevel({})"), "E728");
    }

    // eval.lua:3043-3095 — all three take exactly one argument. Oracle:
    // E119 with none and E118 with two.
    #[test]
    fn fold_queries_enforce_their_arity() {
        assert_eq!(error_code("let g:a = foldclosed()"), "E119");
        assert_eq!(error_code("let g:a = foldclosedend()"), "E119");
        assert_eq!(error_code("let g:a = foldlevel()"), "E119");
        assert_eq!(error_code("let g:a = foldclosed(1, 2)"), "E118");
        assert_eq!(error_code("let g:a = foldclosedend(1, 2)"), "E118");
        assert_eq!(error_code("let g:a = foldlevel(1, 2)"), "E118");
    }
}
