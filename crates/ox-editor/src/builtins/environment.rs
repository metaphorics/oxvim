//! Ambient-environment builtins: working directory, system clock, standard
//! paths, shell quoting, display width, highlight table, and event-loop state.

use ox_eval::EvalError;
use ox_types::{OxStr, Typval};
use crate::options::OptionValue;
use crate::script::{FileIO, StdPath};
use crate::Editor;

use crate::builtins::position::cell_width;
use crate::excmd_exec::{EvalHost, ExRuntime, change_directory, typval_number, typval_to_text};
use super::input_string_arg;

/// Routes one ambient-environment builtin.
pub(crate) fn call<F: FileIO>(
    host: &mut EvalHost<'_, F>,
    name: &str,
    args: Vec<Typval>,
) -> ox_eval::Result<Typval> {
    match name {
        "chdir" => call_chdir_builtin(host.runtime, host.editor, args),
        // f_eventhandler: nothing in this host runs inside an event handler.
        "eventhandler" => Ok(Typval::Number(0)),
        "highlight_exists" | "hlexists" => call_hlexists_builtin(host.editor, args),
        "shellescape" => call_shellescape_builtin(host.editor, &args),
        "stdpath" => call_stdpath_builtin(&args),
        "strdisplaywidth" => call_strdisplaywidth_builtin(host.editor, &args),
        "strftime" => call_strftime_builtin(args),
        _ => unreachable!("environment builtin route and dispatcher disagree"),
    }
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
    let what = input_string_arg(args.first().ok_or_else(|| {
        EvalError::new("E119", 0, "Not enough arguments for function: stdpath")
    })?)?;
    let what = what.to_string_lossy();
    let Some(selector) = StdPath::parse(&what) else {
        return Err(EvalError::new("E6100", 0, format!("\"{what}\" is not a valid stdpath")));
    };
    let mut dirs = crate::script::stdpath(selector);
    if selector.is_list() {
        return Ok(Typval::list(
            dirs.into_iter().map(|dir| Typval::String(OxStr::from(dir.as_str()))).collect(),
        ));
    }
    Ok(Typval::String(OxStr::from(dirs.pop().unwrap_or_default().as_str())))
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
    b"%", b"#", b"<cword>", b"<cWORD>", b"<cexpr>", b"<cfile>", b"<sfile>", b"<slnum>",
    b"<stack>", b"<script>", b"<afile>", b"<abuf>", b"<amatch>", b"<sflnum>", b"<SID>",
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
        EvalError::new("E119", 0, "Not enough arguments for function: strdisplaywidth")
    })?)?;
    let start = args.get(1).and_then(typval_number).unwrap_or(0).max(0);
    let tabstop = match editor.current_buffer().map(|buffer| editor.options().get_buffer(buffer, "tabstop")) {
        Some(Ok(OptionValue::Number(value))) if *value > 0 => usize::try_from(*value).unwrap_or(8),
        _ => 8,
    };
    let mut vcol = usize::try_from(start).unwrap_or(0);
    let begin = vcol;
    for character in String::from_utf8_lossy(text.as_bytes()).chars() {
        vcol += cell_width(character, vcol, tabstop);
    }
    Ok(Typval::Number(i64::try_from(vcol - begin).unwrap_or(i64::MAX)))
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

fn call_chdir_builtin<F: FileIO>(runtime: &mut ExRuntime<F>, editor: &Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::new(if args.is_empty() { "E119" } else { "E118" }, 0, "Invalid arguments for chdir"));
    }
    let Typval::String(path) = &args[0] else { return Ok(Typval::String(OxStr::from(""))); };
    let local = match args.get(1) {
        None => runtime.local_dir.is_some(),
        Some(Typval::String(scope)) => match scope.as_bytes() {
            b"global" => false,
            b"window" | b"tabpage" | b"buffer" => true,
            _ => return Err(EvalError::new("E475", 0, format!("Invalid argument: scope {}", scope.to_string_lossy()))),
        },
        Some(other) => { input_string_arg(other)?; unreachable!("non-string conversion always errors above") }
    };
    let previous = change_directory(runtime, editor, &path.to_string_lossy(), local)?;
    Ok(Typval::String(OxStr::from(previous.to_string_lossy().as_ref())))
}

fn call_strftime_builtin(args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.is_empty() {
        return Err(EvalError::new("E119", 0, "Not enough arguments for function: strftime"));
    }
    if args.len() > 2 {
        return Err(EvalError::new("E118", 0, "Too many arguments for function: strftime"));
    }
    if typval_to_text(&args[0]) != "%H:%M:%S" {
        return Err(EvalError::not_implemented(OxStr::from("strftime format")));
    }
    let timestamp = args
        .get(1)
        .and_then(typval_number)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        });
    let seconds = timestamp.rem_euclid(86_400);
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    Ok(Typval::String(OxStr::from(format!("{hours:02}:{minutes:02}:{seconds:02}").as_str())))
}

fn call_hlexists_builtin(editor: &Editor, args: Vec<Typval>) -> ox_eval::Result<Typval> {
    if args.len() != 1 { return Err(EvalError::new(if args.is_empty() { "E119" } else { "E118" }, 0, "hlexists() requires one argument")); }
    let name = input_string_arg(&args[0])?;
    let name = name.to_string_lossy();
    Ok(Typval::Number(i64::from(editor.highlights().keys().any(|candidate| candidate.eq_ignore_ascii_case(&name)))))
}
