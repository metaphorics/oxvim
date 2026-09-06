//! Ambient-environment builtins: working directory, system clock, standard
//! paths, shell quoting, display width, highlight table, and event-loop state.

use crate::excmd_exec::ExEditorAccess;
use crate::mode::Mode;
use crate::options::OptionValue;
use crate::script::{FileIO, StdPath};
use crate::visual::VisualKind;
use crate::{DirectoryScope, Editor};
use ox_eval::EvalError;
use ox_types::{OxStr, Typval};

use super::input_string_arg;
use crate::builtins::position::cell_width;
use crate::excmd_exec::{EvalHost, change_directory, typval_number, typval_to_text};

/// Routes one ambient-environment builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    match name {
        "chdir" => host
            .access
            .with_ex_editor(|editor| call_chdir_builtin(editor, args)),
        // `f_defer` through `add_defer` (`eval/userfunc.c` 3390-3484).
        "defer" => call_defer_builtin(host, args),
        "eventhandler" => Ok(Typval::Number(0)),
        // `f_api_info` (eval/funcs.c:450-454): the metadata Object
        // re-expressed as a Typval. Reading the canonical table is
        // infallible in practice; a decode failure is a broken build.
        "api_info" => Ok(crate::excmd_exec::object_to_typval(
            &ox_rpc::canonical_metadata().map_err(|err| {
                EvalError::new("E5009", 0, format!("api_info: {err}"))
            })?,
        )),
        "hlID" => host
            .access
            .with_ex_editor(|editor| call_hl_id_builtin(editor, args)),
        "highlight_exists" | "hlexists" => host
            .access
            .with_ex_editor(|editor| call_hlexists_builtin(editor, args)),
        "shellescape" => host
            .access
            .with_ex_editor(|editor| call_shellescape_builtin(editor, args)),
        "mode" => call_mode_builtin(host, args),
        "swapname" => {
            // `f_swapname` with this port's absent swap subsystem: every
            // buffer answers "no swap file".
            if args.len() > 1 {
                return Err(EvalError::new(
                    "E118",
                    0,
                    "Too many arguments for function: swapname",
                ));
            }
            Ok(Typval::String(OxStr::from("")))
        }
        "stdpath" => call_stdpath_builtin(args),
        "strdisplaywidth" => host
            .access
            .with_ex_editor(|editor| call_strdisplaywidth_builtin(editor, args)),
        "strftime" => call_strftime_builtin(args),
        _ => unreachable!("environment builtin route and dispatcher disagree"),
    }
}
/// `f_defer` through `add_defer` (`eval/userfunc.c` 3390-3484): register `{fn}`
/// — a Funcref or a function-name String — with its remaining arguments on
/// the innermost user-function frame, to be fired when that frame ends.
///
/// Registration is intentionally permissive: an unknown function is tolerated
/// ("it might be defined later", `userfunc.c` 3434); resolution happens at
/// fire time. Outside any function `can_add_defer` (3457-3464) gives
/// `E193: defer not inside a function`.
///
/// Dict partials are rejected with `E1300: Cannot use a partial with
/// dictionary for :defer`.
fn call_defer_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: defer",
        ));
    }
    if !host.runtime.can_add_defer() {
        return Err(EvalError::new("E193", 0, "defer not inside a function"));
    }
    // `userfunc.c:3398-3412`: a Partial's bound arguments come first, then
    // the arguments given to `defer()` itself.
    let (name, mut call_args) = match &args[0] {
        Typval::Funcref(function) | Typval::Partial(function) => {
            if function.dict.is_some() {
                return Err(EvalError::new(
                    "E1300",
                    0,
                    "Cannot use a partial with dictionary for :defer",
                ));
            }
            (
                function.name.to_string_lossy().into_owned(),
                function.args.clone(),
            )
        }
        value => {
            let name = input_string_arg(value)?;
            let text = name.to_string_lossy();
            if text.is_empty() {
                return Err(EvalError::new("E129", 0, "Function name required"));
            }
            (text.into_owned(), Vec::new())
        }
    };
    call_args.extend(args[1..].iter().cloned());
    host.runtime.push_deferred_call(name, call_args);
    Ok(Typval::Number(0))
}

/// `f_stdpath` (`eval/funcs.c:7011-7040`). The single-directory selectors
/// answer a String and `config_dirs`/`data_dirs` answer a List, per
/// `get_xdg_var_list`; an unrecognised selector is `E6100`
/// (`eval/funcs.c:7038`).
///
/// This is line 1 of every lazy.nvim bootstrap, and it resolves through the
/// same XDG helpers `'runtimepath'` is built from ([`crate::script::stdpath`]),
/// so the path a plugin manager installs into and the rtp entry it expects to
/// be found on are one answer, not two.
fn call_stdpath_builtin(args: &[Typval]) -> ox_eval::Result<Typval> {
    let what =
        input_string_arg(args.first().ok_or_else(|| {
            EvalError::new("E119", 0, "Not enough arguments for function: stdpath")
        })?)?;
    let what = what.to_string_lossy();
    let Some(selector) = StdPath::parse(&what) else {
        return Err(EvalError::new(
            "E6100",
            0,
            format!("\"{what}\" is not a valid stdpath"),
        ));
    };
    let mut dirs = crate::script::stdpath(selector);
    if selector.is_list() {
        return Ok(Typval::list(
            dirs.into_iter()
                .map(|dir| Typval::String(OxStr::from(dir.as_str())))
                .collect(),
        ));
    }
    Ok(Typval::String(OxStr::from(
        dirs.pop().unwrap_or_default().as_str(),
    )))
}

/// `f_shellescape` (`eval/funcs.c:6660-6667`) through
/// `vim_strsave_shellescape` (`strings.c:186-290`).
///
/// The whole string is single-quoted, `'` becomes `'\''`, and `!`/newline gain
/// a backslash when the shell is csh-like or the caller asked for special
/// handling -- two backslashes when both hold. A csh-like or fish-like shell is
/// decided by the tail of `'shell'` (`option.c:7095-7104`), so this reads the
/// editor's option rather than `$SHELL`.
fn call_shellescape_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let text = input_string_arg(args.first().ok_or_else(|| {
        EvalError::new("E119", 0, "Not enough arguments for function: shellescape")
    })?)?;
    let do_special = args.get(1).is_some_and(non_zero_arg);
    let shell = match editor.options().get_global("shell") {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    };
    let tail = shell.rsplit('/').next().unwrap_or(shell.as_str());
    let csh_like = tail.contains("csh");
    let fish_like = tail.contains("fish");

    let mut escaped = vec![b'\''];
    let mut rest = text.as_bytes();
    while let Some((byte, tail)) = rest.split_first() {
        match byte {
            b'\'' => escaped.extend_from_slice(b"'\\''"),
            b'\n' | b'!' if csh_like || do_special => {
                escaped.push(b'\\');
                if csh_like && do_special {
                    escaped.push(b'\\');
                }
                escaped.push(*byte);
            }
            b'\\' if fish_like => {
                escaped.push(b'\\');
                escaped.push(*byte);
            }
            _ => {
                if do_special && let Some(length) = cmdline_var_length(rest) {
                    escaped.push(b'\\');
                    escaped.extend_from_slice(&rest[..length]);
                    rest = &rest[length..];
                    continue;
                }
                escaped.push(*byte);
            }
        }
        rest = tail;
    }
    escaped.push(b'\'');
    Ok(Typval::String(OxStr(escaped)))
}

/// The cmdline special-file names `find_cmdline_var` recognises
/// (`ex_docmd.c:7491-7508`), longest-safe order: no entry is a prefix of
/// another, so first match wins as it does upstream.
const CMDLINE_VARS: [&[u8]; 15] = [
    b"%",
    b"#",
    b"<cword>",
    b"<cWORD>",
    b"<cexpr>",
    b"<cfile>",
    b"<sfile>",
    b"<slnum>",
    b"<stack>",
    b"<script>",
    b"<afile>",
    b"<abuf>",
    b"<amatch>",
    b"<sflnum>",
    b"<SID>",
];

fn cmdline_var_length(text: &[u8]) -> Option<usize> {
    CMDLINE_VARS
        .iter()
        .find(|name| text.starts_with(name))
        .map(|name| name.len())
}

/// `f_strdisplaywidth` (`strings.c:2775-2785`): `linetabsize_col(col, s) - col`,
/// so the answer depends on where on screen the text starts and on the
/// buffer's `'tabstop'` -- a tab is measured to the next stop, not as one
/// cell. That is what separates it from `strwidth`, which the typval-only
/// table already serves.
fn call_strdisplaywidth_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let text = input_string_arg(args.first().ok_or_else(|| {
        EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: strdisplaywidth",
        )
    })?)?;
    let start = args.get(1).and_then(typval_number).unwrap_or(0).max(0);
    let tabstop = match editor
        .current_buffer()
        .map(|buffer| editor.options().get_buffer(buffer, "tabstop"))
    {
        Some(Ok(OptionValue::Number(value))) if *value > 0 => usize::try_from(*value).unwrap_or(8),
        _ => 8,
    };
    let mut vcol = usize::try_from(start).unwrap_or(0);
    let begin = vcol;
    for character in String::from_utf8_lossy(text.as_bytes()).chars() {
        vcol += cell_width(character, vcol, tabstop);
    }
    Ok(Typval::Number(
        i64::try_from(vcol - begin).unwrap_or(i64::MAX),
    ))
}

/// `non_zero_arg` (`eval/funcs.c`): a non-zero Number, or a non-empty String
/// that is not `"0"`.
fn non_zero_arg(value: &Typval) -> bool {
    match value {
        Typval::Number(number) => *number != 0,
        Typval::Bool(flag) => *flag,
        Typval::Float(number) => *number != 0.0,
        Typval::String(text) => !text.as_bytes().is_empty() && text.as_bytes() != b"0",
        _ => false,
    }
}

fn call_chdir_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            "Invalid arguments for chdir",
        ));
    }
    let Typval::String(path) = &args[0] else {
        return Ok(Typval::String(OxStr::from("")));
    };
    let local_active = editor
        .current_window()
        .and_then(|window| editor.window_local_directory(window).ok().flatten())
        .is_some();
    let scope = match args.get(1) {
        None => {
            if local_active {
                DirectoryScope::Window
            } else {
                DirectoryScope::Global
            }
        }
        Some(value) => {
            let coerced;
            let scope = match value {
                Typval::String(scope) => scope,
                value => {
                    coerced = input_string_arg(value)?;
                    &coerced
                }
            };
            match scope.as_bytes() {
                b"global" => DirectoryScope::Global,
                b"window" | b"tabpage" | b"buffer" => DirectoryScope::Window,
                _ => {
                    return Err(EvalError::new(
                        "E475",
                        0,
                        format!(
                            "Invalid value for argument scope: {}",
                            scope.to_string_lossy()
                        ),
                    ));
                }
            }
        }
    };
    let previous = change_directory(editor, &path.to_string_lossy(), scope)?;
    Ok(Typval::String(OxStr::from(
        previous.to_string_lossy().as_ref(),
    )))
}

fn call_strftime_builtin(args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: strftime",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: strftime",
        ));
    }
    if typval_to_text(&args[0]) != "%H:%M:%S" {
        return Err(EvalError::not_implemented(OxStr::from("strftime format")));
    }
    let timestamp = args.get(1).and_then(typval_number).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            })
    });
    let seconds = timestamp.rem_euclid(86_400);
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    Ok(Typval::String(OxStr::from(
        format!("{hours:02}:{minutes:02}:{seconds:02}").as_str(),
    )))
}

fn call_hlexists_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            "hlexists() requires one argument",
        ));
    }
    let name = input_string_arg(&args[0])?;
    let name = name.to_string_lossy();
    Ok(Typval::Number(i64::from(
        editor
            .highlights()
            .keys()
            .any(|candidate| candidate.eq_ignore_ascii_case(&name)),
    )))
}

/// `f_mode` (`eval/funcs.c:4454`): the full mode string, truncated to its
/// major character when the optional argument is absent or zero. This port
/// models the same major/minor shapes `get_mode` builds: `n`/`no`, `i`, `R`,
/// `c`, `t`/`nt` for terminal buffers, and `v`/`V`/`CTRL-V`.
fn call_mode_builtin<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: mode",
        ));
    }
    let full = host
        .runtime
        .mode_machine
        .as_ref()
        .and_then(|machine| {
            let machine = machine.try_borrow().ok()?;
            let terminal = host.access.with_ex_editor(|editor| {
                editor
                    .current_buffer()
                    .is_some_and(|buffer| editor.is_terminal_buffer(buffer))
            });
            let name: &str = match machine.mode() {
                Mode::Normal(_) if terminal => "nt",
                Mode::Normal(_) => "n",
                Mode::Insert(_) if terminal => "t",
                Mode::Insert(_) => "i",
                Mode::Replace(_) => "R",
                Mode::Cmdline(_) => "c",
                Mode::OperatorPending(_) => "no",
                Mode::Visual(state) => match state.kind {
                    VisualKind::Character => "v",
                    VisualKind::Line => "V",
                    VisualKind::Block => "\u{16}",
                },
            };
            Some(name.to_owned())
        })
        .unwrap_or_else(|| "n".to_owned());
    let keep_minor = args
        .first()
        .is_some_and(|value| value.is_truthy() || typval_number(value).unwrap_or(0) != 0);
    let text = if keep_minor {
        full
    } else {
        full.chars()
            .next()
            .map_or_else(String::new, |c| c.to_string())
    };
    Ok(Typval::String(OxStr::from(text.as_str())))
}

#[cfg(test)]
mod tests {
    use ox_eval::Scope;
    use ox_types::Typval;

    use crate::{Editor, ExExecutor, ExecError, TestEditorAccess, VimExceptionKind};

    fn error_code(err: &ExecError) -> String {
        match err {
            ExecError::Vim(exception) => match &exception.kind {
                VimExceptionKind::Error(code) => code.clone(),
                VimExceptionKind::Throw => "Throw".to_owned(),
            },
            other => panic!("expected Vim error, got {other:?}"),
        }
    }

    fn global(scope: &Scope, name: &str) -> Option<Typval> {
        scope
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()
            .cloned()
    }

    fn global_number(scope: &Scope, name: &str) -> Option<i64> {
        match global(scope, name)? {
            Typval::Number(value) => Some(value),
            Typval::Bool(value) => Some(i64::from(value)),
            _ => None,
        }
    }

    // `can_add_defer` (`eval/userfunc.c` 3457-3464) gives E193 when no
    // function frame is active.
    #[test]
    fn defer_outside_a_function_reports_e193() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let err = exec
            .execute_script(&editor, "<test>", "defer('Absent')")
            .unwrap_err();
        assert_eq!(error_code(&err), "E193");
    }

    // `add_defer` (3469-3484) registers; `handle_defer_one` (3487-3524) fires
    // at frame end, so the deferred function's global is set only after the
    // caller returns.
    #[test]
    fn defer_fires_at_function_exit_and_sets_its_global() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let script = r"
            function! SetFlag()
                let g:deferred_ran = 1
            endfunction
            function! DeferUser()
                call defer('SetFlag')
                let g:before_exit = 1
            endfunction
            call DeferUser()
        ";
        exec.execute_script(&editor, "<test>", script).unwrap();
        assert_eq!(global_number(exec.scope(), "before_exit"), Some(1));
        assert_eq!(global_number(exec.scope(), "deferred_ran"), Some(1));
    }

    // `add_defer` stores the remaining arguments; `handle_defer_one` invokes
    // the callable with them. The Funcref form works as well as the name.
    #[test]
    fn defer_passes_arguments_and_funcref_form_fires() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        let script = r"
            function! StoreValue(value)
                let g:deferred_value = a:value
            endfunction
            function! Mark(name)
                call add(g:order, a:name)
            endfunction
            function! DeferWithArgs()
                let g:order = []
                call defer(function('StoreValue'), 7)
                call defer('Mark', 'first')
                call defer('Mark', 'second')
            endfunction
            call DeferWithArgs()
        ";
        exec.execute_script(&editor, "<test>", script).unwrap();
        assert_eq!(global_number(exec.scope(), "deferred_value"), Some(7));
        let order = match global(exec.scope(), "order") {
            Some(Typval::List(list)) => list
                .borrow()
                .items
                .iter()
                .map(|value| match value {
                    Typval::String(text) => text.to_string_lossy().into_owned(),
                    other => panic!("expected strings, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("expected a List, got {other:?}"),
        };
        assert_eq!(order, vec!["second", "first"]);
    }
}


/// `f_hlID` → `syn_name2id` (eval/funcs.c): the group's 1-based id, 0 when
/// absent. Upstream assigns ids in `hl_table` allocation order; the port's
/// name-keyed table yields the sorted position instead, which is stable and
/// positive but not order-identical to upstream.
fn call_hl_id_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        return Err(EvalError::new(
            if args.is_empty() { "E119" } else { "E118" },
            0,
            "hlID() requires one argument",
        ));
    }
    let name = input_string_arg(&args[0])?;
    let name = name.to_string_lossy();
    let id = editor
        .highlights()
        .keys()
        .position(|candidate| candidate.eq_ignore_ascii_case(&name))
        .map_or(0, |index| index as i64 + 1);
    Ok(Typval::Number(id))
}

#[cfg(test)]
mod api_and_highlight_tests {
    use ox_types::Typval;

    use crate::{Editor, ExExecutor, TestEditorAccess};

    fn run(editor: &TestEditorAccess, exec: &mut ExExecutor, script: &str) {
        exec.execute_script(editor, "<test>", script).unwrap();
    }

    fn global_number(exec: &ExExecutor, name: &str) -> Option<i64> {
        let value = exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?;
        match value {
            Typval::Number(n) => Some(*n),
            _ => None,
        }
    }

    fn global_keys(exec: &ExExecutor, name: &str) -> Option<Vec<String>> {
        let value = exec
            .scope()
            .get_scoped(ox_eval::ScopeKind::Global, name.as_bytes(), 0)
            .ok()?;
        match value {
            Typval::Dict(dict) => Some(
                dict.borrow()
                    .entries
                    .iter()
                    .map(|field| field.key.to_string_lossy().into_owned())
                    .collect(),
            ),
            _ => None,
        }
    }

    // f_api_info returns object_to_vim(api_metadata()) (funcs.c:450-454);
    // the metadata dict always carries these members.
    #[test]
    fn api_info_returns_metadata_dict() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        run(&editor, &mut exec, "let g:info = api_info()");
        let keys = global_keys(&exec, "info").expect("api_info() result is a dict");
        for member in ["version", "functions", "ui_events"] {
            assert!(
                keys.iter().any(|key| key == member),
                "api_info() missing '{member}' (has {:?})",
                keys
            );
        }
    }

    // f_hlID -> syn_name2id: a startup group answers a positive id, an
    // unknown name answers 0, and hlexists() agrees.
    #[test]
    fn hl_id_is_positive_for_known_group_and_zero_for_unknown() {
        let editor = TestEditorAccess::new(Editor::new());
        let mut exec = ExExecutor::new();
        run(
            &editor,
            &mut exec,
            "let g:known = hlID('NonText')\nlet g:unknown = hlID('NoSuchGroupEver')\nlet g:exists = hlexists('NonText')",
        );
        let known = global_number(&exec, "known").expect("hlID() number");
        assert!(known > 0, "hlID('NonText') = {known}, want > 0");
        assert_eq!(global_number(&exec, "unknown"), Some(0));
        assert_eq!(global_number(&exec, "exists"), Some(1));
    }
}
