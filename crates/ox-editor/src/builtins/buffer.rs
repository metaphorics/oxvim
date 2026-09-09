//! Buffer-state builtins: buffer-local variables, buffer identity, and the
//! line seams `getline`/`setline`/`append`/`line` reach through the current
//! buffer (upstream `eval/buffer.c`).

use crate::excmd_exec::ExEditorAccess;
use crate::script::FileIO;
use ox_eval::BufferHost;
use ox_eval::EvalError;
use ox_eval::Scope;
use ox_eval::call_buffer_builtin;
use ox_eval::scope::OptionScope as EvalOptionScope;
use ox_eval::scope::{ScopeKind, scope_var_entry};
use ox_text::{Buffer, Position};
use ox_types::{BufHandle, OxStr, Special, Typval};

use super::position::number_value;
use crate::options::{OptionScope, OptionValue};
use crate::{Editor, LineReplaceRequest};

use crate::autocmd::Event;
use crate::excmd_exec::{
    CurrentBuffer, EvalHost, Flow, fire_buffer_lifecycle, flow_to_eval_error, object_to_typval,
    option_to_typval, path_from_ox_str, resolve_buffer_argument, typval_number, typval_to_object,
    typval_to_option, typval_to_text,
};

/// Routes one buffer-state builtin.
pub(crate) fn call<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    name: &str,
    args: &[Typval],
    scope: &mut Scope,
) -> ox_eval::Result<Typval> {
    match name {
        // `getline`/`setline` reach the current buffer through
        // `ox_eval::BufferHost`; the typval-only dispatcher has no buffer.
        "getline" => host.access.with_ex_editor(|editor| {
            let mut seam = CurrentBuffer(editor);
            call_buffer_builtin(&mut seam, name, args)
        }),
        "setline" => {
            let result = host.access.with_ex_editor(|editor| {
                let mut seam = CurrentBuffer(editor);
                call_buffer_builtin(&mut seam, name, args)
            })?;
            let prompt = host.access.with_ex_editor(|editor| {
                editor.current_buffer().and_then(|buffer| {
                    let state = editor.buffer(buffer).ok()?;
                    let line = state.text().ok()?.line(state.prompt_start()).ok()?;
                    (!state.prompt().is_empty() && line.is_empty()).then(|| state.prompt().to_vec())
                })
            });
            if let Some(prompt) = prompt {
                host.access.with_ex_editor(|editor| {
                    call_prompt_setprompt_builtin(
                        editor,
                        &[Typval::Number(0), Typval::String(OxStr(prompt))],
                    )
                })?;
            }
            Ok(result)
        }
        "append" => host
            .access
            .with_ex_editor(|editor| call_append_builtin(editor, args)),
        "appendbufline" => host.access.with_ex_editor(|editor| {
            call_bufline_mutation_builtin(editor, BufferLineMutation::Append, name, args)
        }),
        "deletebufline" => host
            .access
            .with_ex_editor(|editor| call_deletebufline_builtin(editor, args)),
        "changenr" => host
            .access
            .with_ex_editor(|editor| call_changenr_builtin(editor, args)),
        "undotree" => host
            .access
            .with_ex_editor(|editor| call_undotree_builtin(editor, args)),
        "bufadd" => host
            .access
            .with_ex_editor(|editor| call_bufadd_builtin(editor, args)),
        "bufexists" => host
            .access
            .with_ex_editor(|editor| call_bufexists_builtin(editor, args)),
        "bufload" => call_bufload_with_events(host, scope, args),
        "getbufline" => host
            .access
            .with_ex_editor(|editor| call_getbufline_builtin(editor, args)),
        "getbufinfo" => host
            .access
            .with_ex_editor(|editor| call_getbufinfo_builtin(editor, args)),
        "getchangelist" => host
            .access
            .with_ex_editor(|editor| call_getchangelist_builtin(editor, args)),
        "bufname" | "bufnr" => host
            .access
            .with_ex_editor(|editor| call_buffer_identity_builtin(editor, name, args)),
        "getbufvar" => host
            .access
            .with_ex_editor(|editor| call_getbufvar_builtin(editor, scope, args)),
        "last_buffer_nr" => host
            .access
            .with_ex_editor(|editor| call_last_buffer_nr_builtin(editor, args)),
        "setbufvar" => host
            .access
            .with_ex_editor(|editor| call_setbufvar_builtin(editor, scope, args)),
        "setbufline" => host.access.with_ex_editor(|editor| {
            call_bufline_mutation_builtin(editor, BufferLineMutation::Set, name, args)
        }),
        "prompt_getprompt" => host
            .access
            .with_ex_editor(|editor| call_prompt_getprompt_builtin(editor, args)),
        "prompt_setprompt" => host
            .access
            .with_ex_editor(|editor| call_prompt_setprompt_builtin(editor, args)),
        _ => unreachable!("buffer builtin route and dispatcher disagree"),
    }
}

fn call_prompt_getprompt_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: prompt_getprompt",
        ));
    }
    let Some(buffer) = resolve_buffer_argument(editor, args.first()) else {
        return Ok(Typval::String(OxStr::from("")));
    };
    let is_prompt = editor
        .options()
        .get_buffer(buffer, "buftype")
        .is_ok_and(|value| matches!(value, OptionValue::String(value) if value == "prompt"));
    if !is_prompt {
        return Ok(Typval::String(OxStr::from("")));
    }
    match editor.buffer(buffer) {
        Ok(state) => Ok(Typval::String(OxStr::from(state.effective_prompt()))),
        Err(_) => Ok(Typval::String(OxStr::from(""))),
    }
}

fn call_prompt_setprompt_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: prompt_setprompt",
        ));
    }
    let buffer = resolve_buffer_argument(editor, args.first())
        .or_else(|| editor.current_buffer())
        .ok_or_else(|| EvalError::new("E86", 0, "Buffer does not exist"))?;
    let prompt = typval_to_text(&args[1]);
    let stored = prompt.into_bytes();
    let new_prompt: &[u8] = if stored.is_empty() { b"% " } else { &stored };
    // `f_prompt_setprompt` (`eval/buffer.c:963-1014`) rewrites stored prompt
    // for every valid buffer, but touches resident text only for a loaded
    // prompt buffer. When the special `':'` mark or stored prompt state
    // records a previous complete prefix, that entire prefix is replaced —
    // not a suffix-matched slice of the current stored prompt.
    let visible_prompt = matches!(
        editor.options().get_buffer(buffer, "buftype"),
        Ok(OptionValue::String(value)) if value == "prompt"
    ) && editor
        .buffer(buffer)
        .map_err(|error| EvalError::new("E86", 0, error.to_string()))?
        .residency
        .is_loaded();
    if visible_prompt {
        let (lnum, line, old_len) = {
            let state = editor
                .buffer(buffer)
                .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
            let text = state
                .text()
                .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
            let lnum = state.prompt_start();
            let mut line = text
                .line(lnum)
                .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
            let old_prompt = state.effective_prompt();
            let prompt_mark = state
                .marks
                .get(':')
                .map_err(|error| EvalError::new("E86", 0, error.to_string()))?
                .filter(|mark| mark.lnum == lnum);
            let old_len = match prompt_mark {
                Some(mark)
                    if mark.col >= old_prompt.len()
                        && mark.col <= line.len()
                        && line.get(mark.col - old_prompt.len()..mark.col) == Some(old_prompt) =>
                {
                    Some(mark.col)
                }
                // A stale mark away from the prompt start replaces the whole
                // visible line; with no mark, a stored prefix swap stops at
                // the prefix.
                Some(_) => None,
                None => line.starts_with(old_prompt).then_some(old_prompt.len()),
            }
            .unwrap_or(line.len());
            line.splice(0..old_len, new_prompt.iter().copied());
            (lnum, line, old_len)
        };
        editor
            .replace_prompt_line(buffer, lnum, line, old_len, new_prompt.len())
            .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
        editor
            .buffer_mut(buffer)
            .map_err(|error| EvalError::new("E86", 0, error.to_string()))?
            .marks
            .set(
                ':',
                Position {
                    lnum,
                    col: new_prompt.len(),
                },
            )
            .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
    }
    editor
        .buffer_mut(buffer)
        .map_err(|error| EvalError::new("E86", 0, error.to_string()))?
        .set_prompt_text(stored);
    Ok(Typval::Number(0))
}

/// `bufexists()`: buffer number 0 never exists, every other resolvable
/// argument does (`f_bufexists` → `buflist_find_nr`).
fn call_bufexists_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        let (code, message) = if args.is_empty() {
            ("E119", "Not enough arguments for function: bufexists")
        } else {
            ("E118", "Too many arguments for function: bufexists")
        };
        return Err(EvalError::new(code, 0, message));
    }
    let exists = !matches!(args.first(), Some(Typval::Number(0)))
        && resolve_buffer_argument(editor, args.first()).is_some();
    Ok(Typval::Number(i64::from(exists)))
}

/// `bufname()` and `bufnr()`: `bufnr("$")` answers the highest buffer number
/// ever used, an unresolvable argument answers -1 (`f_bufnr`, `f_bufname`).
fn call_buffer_identity_builtin(
    editor: &mut Editor,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let max_args = if name == "bufnr" { 2 } else { 1 };
    if args.len() > max_args {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    let buffer = resolve_buffer_argument(editor, args.first());
    if name == "bufnr" {
        if args
            .first()
            .is_some_and(|value| typval_to_text(value) == "$")
        {
            return Ok(Typval::Number(editor.last_buffer_nr()));
        }
        if let Some(buffer) = buffer {
            return Ok(Typval::Number(i64::from(buffer)));
        }
        let create = args
            .get(1)
            .map(number_value)
            .transpose()?
            .is_some_and(|value| value != 0);
        if !create {
            return Ok(Typval::Number(-1));
        }
        let buffer_name = args.first().map_or_else(
            || OxStr::from(""),
            |value| OxStr(typval_to_text(value).into_bytes()),
        );
        let handle = add_unloaded_buffer(editor, &buffer_name)?;
        return Ok(Typval::Number(i64::from(handle)));
    }
    let name = buffer
        .and_then(|handle| editor.buffer(handle).ok())
        .map_or_else(|| OxStr::from(""), |state| state.name().clone());

    Ok(Typval::String(name))
}

fn call_bufadd_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    let [value] = args else {
        let (code, message) = if args.is_empty() {
            ("E119", "Not enough arguments for function: bufadd")
        } else {
            ("E118", "Too many arguments for function: bufadd")
        };
        return Err(EvalError::new(code, 0, message));
    };
    let name = bufadd_name(value)?;
    if !name.as_bytes().is_empty()
        && let Some(handle) = editor.buffers().into_iter().find(|handle| {
            editor
                .buffer(*handle)
                .is_ok_and(|state| state.name() == &name)
        })
    {
        return Ok(Typval::Number(i64::from(handle)));
    }
    Ok(Typval::Number(
        add_unloaded_buffer(editor, &name).map_or(0, i64::from),
    ))
}

fn bufadd_name(value: &Typval) -> ox_eval::Result<OxStr> {
    match value {
        Typval::String(value) => Ok(value.clone()),
        Typval::Number(value) => Ok(OxStr(value.to_string().into_bytes())),
        Typval::Bool(value) => Ok(OxStr::from(if *value { "v:true" } else { "v:false" })),
        Typval::Special(Special::Null) => Ok(OxStr::from("v:null")),
        Typval::Float(value) => Ok(ox_eval::float_as_string(*value)),
        Typval::Channel(value) | Typval::Job(value) => Ok(OxStr(value.to_string().into_bytes())),
        Typval::List(_) => Err(EvalError::new("E730", 0, "Using a List as a String")),
        Typval::Dict(_) => Err(EvalError::new("E731", 0, "Using a Dictionary as a String")),
        Typval::Blob(_) => Err(EvalError::new("E976", 0, "Using a Blob as a String")),
        Typval::Funcref(_) | Typval::Partial(_) => {
            Err(EvalError::new("E729", 0, "Using a Funcref as a String"))
        }
    }
}

fn add_unloaded_buffer(editor: &mut Editor, name: &OxStr) -> ox_eval::Result<BufHandle> {
    let handle = editor
        .create_buffer(false)
        .map_err(|error| EvalError::new("E948", 0, error.to_string()))?;
    let state = editor
        .buffer_mut(handle)
        .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
    if !name.as_bytes().is_empty() {
        state.set_name(name.clone());
    }
    state
        .unload()
        .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
    Ok(handle)
}

/// `bufload({buf})`: ensure an unloaded buffer is loaded (`f_bufload`,
/// `eval/buffer.c:484-495` → `buf_ensure_loaded` → `open_buffer`).
///
/// For ordinary buffers the buffer name is read through the `FileIO` seam;
/// for `nofile`/`quickfix`/`prompt`/`terminal` special types it loads an
/// empty buffer instead (`bt_nofileread`, `buffer.c:4071-4077`).  The current
/// buffer and window are never changed.
///
/// A file-backed load fires `BufReadPre` before the read and `BufReadPost`
/// after it, the way `open_buffer`'s `readfile` does; a name whose file
/// does not exist fires `BufNewFile` instead. Resolution and the text
/// install each hold one short borrow; the events fire between borrows
/// because listeners reenter the editor.
fn call_bufload_with_events<F: FileIO, E: ExEditorAccess>(
    host: &mut EvalHost<'_, F, E>,
    scope: &mut Scope,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() != 1 {
        let (code, message) = if args.is_empty() {
            ("E119", "Not enough arguments for function: bufload")
        } else {
            ("E118", "Too many arguments for function: bufload")
        };
        return Err(EvalError::new(code, 0, message));
    }
    let (buffer, name, buftype, loaded) = host.access.with_ex_editor(|editor| {
        let argument = args.first();
        let buffer = resolve_buffer_argument(editor, argument).ok_or_else(|| {
            let name = argument.map_or_else(String::new, typval_to_text);
            EvalError::new("E158", 0, format!("Invalid buffer name: {name}"))
        })?;
        let state = editor
            .buffer(buffer)
            .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
        let loaded = state.residency.is_loaded();
        let name = state.name().clone();
        let buftype = match editor.options().get_buffer(buffer, "buftype") {
            Ok(OptionValue::String(value)) => value.clone(),
            Ok(_) | Err(_) => String::new(),
        };
        Ok((buffer, name, buftype, loaded))
    })?;
    if loaded {
        return Ok(Typval::Number(0));
    }
    // Upstream `readfile` probes the file with a real open before choosing
    // the event family (`fileio.c:428-516`): a file that opens reads
    // between `BufReadPre` and `BufReadPost`; a missing one fires
    // `BufNewFile` instead; a present but unreadable or non-regular name —
    // like every `buftype` that never reads — loads without an event. The
    // probe is a read because the seam has no bare open.
    let path = if name.as_bytes().is_empty() || is_nofileread(&buftype) {
        None
    } else {
        Some(path_from_ox_str(&name))
    };
    let probe = path
        .as_deref()
        .map(|path| host.runtime.scripts.io().read_to_string(path));
    let (existing, new_file) = match &probe {
        Some(Ok(_)) => (true, false),
        Some(Err(error)) => (false, error.kind() == std::io::ErrorKind::NotFound),
        _ => (false, false),
    };
    if existing {
        let flow = fire_buffer_lifecycle(
            host.runtime,
            host.access,
            scope,
            host.lua,
            &[Event::BufReadPre],
            buffer,
        );
        if !matches!(flow, Flow::Normal) {
            return Err(flow_to_eval_error(flow, "bufload"));
        }
    }
    // The content read still happens after `BufReadPre` — upstream closes
    // and reopens around the pre autocmds so a handler can change the file
    // first; a read that fails after the probe leaves the buffer empty and
    // fires no post event (upstream's E200 exit).
    let content = match (existing, path.as_deref()) {
        (true, Some(path)) => host.runtime.scripts.io().read_to_string(path).ok(),
        _ => None,
    };
    host.access.with_ex_editor(|editor| {
        let text = match &content {
            Some(content) => Buffer::from_bytes(content.as_bytes())
                .map_err(|error| EvalError::new("E474", 0, error.to_string()))?,
            None => Buffer::new(),
        };
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
        state.load(text);
        state.mark_saved();
        state.flags.set(crate::BufferFlags::NOTEDITED, false);
        Ok(())
    })?;
    let post = content
        .is_some()
        .then_some(Event::BufReadPost)
        .or(new_file.then_some(Event::BufNewFile));
    if let Some(event) = post {
        let flow =
            fire_buffer_lifecycle(host.runtime, host.access, scope, host.lua, &[event], buffer);
        if !matches!(flow, Flow::Normal) {
            return Err(flow_to_eval_error(flow, "bufload"));
        }
    }
    Ok(Typval::Number(0))
}
/// Whether this `buftype` value means `bufload()` should not read a file.
/// Mirrors upstream `bt_nofileread` (`buffer.c:4071-4077`).
fn is_nofileread(buftype: &str) -> bool {
    matches!(
        buftype.as_bytes(),
        [b'n', _, b'f', ..] | [b't' | b'q' | b'p', ..]
    )
}

fn call_getchangelist_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: getchangelist",
        ));
    }
    let buffer = resolve_buffer_argument(editor, args.first());
    let entries = buffer
        .and_then(|buffer| editor.changelists().entries(buffer))
        .unwrap_or_default()
        .iter()
        .map(|position| {
            Typval::dict(vec![
                (
                    OxStr::from("lnum"),
                    Typval::Number(i64::try_from(position.lnum).unwrap_or(i64::MAX)),
                ),
                (
                    OxStr::from("col"),
                    Typval::Number(i64::try_from(position.col).unwrap_or(i64::MAX)),
                ),
                (OxStr::from("coladd"), Typval::Number(0)),
            ])
        })
        .collect();
    let index = buffer
        .and_then(|buffer| editor.changelists().index(buffer))
        .unwrap_or_default();
    Ok(Typval::list(vec![
        Typval::list(entries),
        Typval::Number(i64::try_from(index).unwrap_or(i64::MAX)),
    ]))
}

/// `last_buffer_nr()`: the highest buffer number ever used.
fn call_last_buffer_nr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: last_buffer_nr",
        ));
    }
    Ok(Typval::Number(editor.last_buffer_nr()))
}

fn call_getbufvar_builtin(
    editor: &Editor,
    scope: &Scope,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    let fallback = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| Typval::String(OxStr(Vec::new())));
    let Some(buffer) = resolve_buffer_argument(editor, args.first()) else {
        return Ok(fallback);
    };
    let name = args.get(1).map(typval_to_text).unwrap_or_default();
    let state = editor
        .buffer(buffer)
        .map_err(|error| EvalError::new("E86", 0, error.to_string()))?;
    if let Some(option) = name.strip_prefix('&') {
        let Some(metadata) = crate::option_metadata(option) else {
            return Ok(fallback);
        };
        let value = if metadata.scopes.contains(&OptionScope::Buffer) {
            editor.options().get_buffer(buffer, metadata.name).ok()
        } else {
            editor.options().get_global(metadata.name).ok()
        };
        return Ok(value.map_or(fallback, option_to_typval));
    }
    if name.as_bytes() == b"changedtick" {
        return Ok(Typval::Number(
            i64::try_from(state.script_changedtick()).unwrap_or(i64::MAX),
        ));
    }
    if name.is_empty() {
        let mut entries = if editor.current_buffer() == Some(buffer) {
            scope
                .buffer
                .iter()
                .map(|(key, value)| scope_var_entry(ScopeKind::Buffer, key, value))
                .collect::<Vec<_>>()
        } else {
            state
                .variables()
                .0
                .iter()
                .map(|(key, value)| {
                    scope_var_entry(ScopeKind::Buffer, key, &object_to_typval(value))
                })
                .collect::<Vec<_>>()
        };
        entries.retain(|entry| entry.key.as_bytes() != b"changedtick");
        entries.push(scope_var_entry(
            ScopeKind::Buffer,
            &OxStr::from("changedtick"),
            &Typval::Number(i64::try_from(state.script_changedtick()).unwrap_or(i64::MAX)),
        ));
        return Ok(Typval::dict_with_entries(entries));
    }
    if editor.current_buffer() == Some(buffer) {
        return Ok(scope
            .buffer
            .iter()
            .find(|(key, _)| key.as_bytes() == name.as_bytes())
            .map_or(fallback, |(_, value)| value.clone()));
    }
    Ok(state
        .variables()
        .0
        .iter()
        .find(|(key, _)| key.as_bytes() == name.as_bytes())
        .map_or(fallback, |(_, value)| object_to_typval(value)))
}

fn call_setbufvar_builtin(
    editor: &mut Editor,
    scope: &mut Scope,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() != 3 {
        return Err(EvalError::new(
            if args.len() < 3 { "E119" } else { "E118" },
            0,
            "Invalid arguments for setbufvar",
        ));
    }
    let buffer = resolve_buffer_argument(editor, args.first())
        .ok_or_else(|| EvalError::new("E86", 0, "Buffer does not exist"))?;
    let name = typval_to_text(&args[1]);
    let value = args[2].clone();
    if name.as_bytes() == b"changedtick" {
        return Err(EvalError::new(
            "E46",
            0,
            format!("Cannot change read-only variable \"b:{name}\""),
        ));
    }

    if let Some(option) = name.strip_prefix('&') {
        let metadata = crate::option_metadata(option)
            .ok_or_else(|| EvalError::new("E518", 0, format!("Unknown option: {option}")))?;
        if !metadata.scopes.contains(&OptionScope::Buffer) {
            return Err(EvalError::new(
                "E355",
                0,
                format!("Unknown option: {option}"),
            ));
        }
        let converted = typval_to_option(&value, metadata.value_type)
            .map_err(|message| EvalError::new("E474", 0, message))?;
        editor
            .options_mut()
            .set_buffer(buffer, metadata.name, converted)
            .map_err(|error| EvalError::new("E474", 0, error.to_string()))?;
        if editor.current_buffer() == Some(buffer) {
            scope.set_option(EvalOptionScope::Local, metadata.name.as_bytes(), value);
        }
        return Ok(Typval::Number(0));
    }

    if editor.current_buffer() == Some(buffer) {
        scope.replace_pair(ScopeKind::Buffer, &name, value.clone());
    }
    let variables = editor
        .buffer_mut(buffer)
        .map_err(|error| EvalError::new("E86", 0, error.to_string()))?
        .variables_mut();
    variables
        .0
        .retain(|(key, _)| key.as_bytes() != name.as_bytes());
    variables
        .0
        .push((OxStr::from(name.as_str()), typval_to_object(&value)));
    Ok(Typval::Number(0))
}

fn call_append_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: append",
        ));
    }
    if args.len() > 2 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: append",
        ));
    }
    let after = current_line_address(editor, &args[0])?;
    let lines = buffer_lines_arg(&args[1]);
    let buffer = editor
        .current_buffer()
        .ok_or_else(|| EvalError::new("E749", 0, "Empty buffer"))?;
    let cursor = editor
        .current_window()
        .and_then(|window| editor.window(window).ok())
        .map_or(
            Position {
                lnum: after.saturating_add(1),
                col: 0,
            },
            |window| window.cursor,
        );
    editor
        .append_buffer_lines(buffer, after, &lines, cursor, 0)
        .map_err(|error| EvalError::new("E16", 0, error.to_string()))?;
    Ok(Typval::Number(0))
}

#[derive(Clone, Copy)]
enum BufferLineMutation {
    Set,
    Append,
}

/// One buffer-line mutation whose arguments already passed every check in
/// [`call_bufline_mutation_builtin`]: the buffer is live, the address sits in
/// the operation's accepted range, and the line list is non-empty. Building
/// this value is the only route to [`BufferLineMutationRequest::apply`], so
/// application never re-validates.
struct BufferLineMutationRequest {
    operation: BufferLineMutation,
    buffer: BufHandle,
    /// One-based address already normalized to the operation's basis:
    /// `setbufline` targets the line itself, `appendbufline` the line before.
    address: usize,
    /// Buffer line count observed before the mutation, bounding replacement.
    line_count: usize,
    lines: Vec<Vec<u8>>,
    cursor: Position,
}

impl BufferLineMutationRequest {
    /// Applies the mutation, answering whether it failed.
    fn apply(self, editor: &mut Editor) -> bool {
        match self.operation {
            BufferLineMutation::Append => editor
                .append_buffer_lines(self.buffer, self.address, &self.lines, self.cursor, 0)
                .is_err(),
            BufferLineMutation::Set => {
                let replace_count = if self.address <= self.line_count {
                    self.lines.len().min(self.line_count - self.address + 1)
                } else {
                    0
                };
                if replace_count > 0
                    && editor
                        .replace_buffer_lines(LineReplaceRequest {
                            buffer: self.buffer,
                            start: self.address,
                            end: self.address + replace_count - 1,
                            lines: &self.lines[..replace_count],
                            cursor_before: self.cursor,
                            cursor_after: self.cursor,
                            timestamp: 0,
                        })
                        .is_err()
                {
                    return true;
                }
                let after = self.address.saturating_sub(1).saturating_add(replace_count);
                replace_count < self.lines.len()
                    && editor
                        .append_buffer_lines(
                            self.buffer,
                            after,
                            &self.lines[replace_count..],
                            self.cursor,
                            0,
                        )
                        .is_err()
            }
        }
    }
}

fn call_bufline_mutation_builtin(
    editor: &mut Editor,
    operation: BufferLineMutation,
    name: &str,
    args: &[Typval],
) -> ox_eval::Result<Typval> {
    if args.len() < 3 {
        return Err(EvalError::new(
            "E119",
            0,
            format!("Not enough arguments for function: {name}"),
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            format!("Too many arguments for function: {name}"),
        ));
    }
    let Some(buffer) = resolve_buffer_argument(editor, args.first()) else {
        return Ok(Typval::Number(1));
    };
    let requested = match &args[1] {
        Typval::String(address) if address.as_bytes() == b"$" => None,
        value => Some(number_value(value)?),
    };
    let Ok(state) = editor.buffer(buffer) else {
        return Ok(Typval::Number(1));
    };
    let Ok(text) = state.text() else {
        return Ok(Typval::Number(1));
    };
    let line_count = text.line_count();
    let line_count_number = i64::try_from(line_count).unwrap_or(i64::MAX);
    let address = requested.unwrap_or(line_count_number);
    let minimum = match operation {
        BufferLineMutation::Set => 1,
        BufferLineMutation::Append => 0,
    };
    if address < minimum {
        return Ok(Typval::Number(1));
    }

    let lines = buffer_lines_arg(&args[2]);
    if lines.is_empty() {
        return Ok(Typval::Number(0));
    }
    let maximum = match operation {
        BufferLineMutation::Set => line_count_number.saturating_add(1),
        BufferLineMutation::Append => line_count_number,
    };
    if address > maximum {
        return Ok(Typval::Number(1));
    }
    if matches!(
        editor.options().get_buffer(buffer, "modifiable"),
        Ok(OptionValue::Boolean(false))
    ) {
        return Err(EvalError::new(
            "E21",
            0,
            "Cannot make changes, 'modifiable' is off",
        ));
    }

    let address = usize::try_from(address).unwrap_or(usize::MAX);
    let fallback_line = match operation {
        BufferLineMutation::Set => address,
        BufferLineMutation::Append => address.saturating_add(1),
    };
    let cursor = editor
        .windows()
        .into_iter()
        .find(|window| {
            editor
                .window(*window)
                .is_ok_and(|state| state.buffer == buffer)
        })
        .and_then(|window| editor.window(window).ok())
        .map_or(
            Position {
                lnum: fallback_line,
                col: 0,
            },
            |state| state.cursor,
        );

    let failed = BufferLineMutationRequest {
        operation,
        buffer,
        address,
        line_count,
        lines,
        cursor,
    }
    .apply(editor);
    Ok(Typval::Number(i64::from(failed)))
}

fn buffer_lines_arg(value: &Typval) -> Vec<Vec<u8>> {
    let to_line = |value: &Typval| {
        let mut line = typval_to_text(value).into_bytes();
        for byte in &mut line {
            if *byte == b'\n' {
                *byte = 0;
            }
        }
        line
    };
    match value {
        Typval::List(values) => values.borrow().items.iter().map(to_line).collect(),
        value => vec![to_line(value)],
    }
}

fn call_getbufline_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: getbufline",
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: getbufline",
        ));
    }
    let empty = || Typval::list(Vec::new());
    let Some(buffer) = resolve_buffer_argument(editor, args.first()) else {
        return Ok(empty());
    };
    let Ok(state) = editor.buffer(buffer) else {
        return Ok(empty());
    };
    let Ok(text) = state.text() else {
        return Ok(empty());
    };

    let line_count = text.line_count();
    let line_count_number = i64::try_from(line_count).unwrap_or(i64::MAX);
    let parse_line = |value: &Typval| match value {
        Typval::String(address) if address.as_bytes() == b"$" => Ok(line_count_number),
        _ => number_value(value),
    };
    let first = parse_line(&args[1])?;
    let last = args.get(2).map_or(Ok(first), parse_line)?;
    if first < 0 || last < first {
        return Ok(empty());
    }

    let first = usize::try_from(first.max(1)).unwrap_or(usize::MAX);
    let last = usize::try_from(last).unwrap_or(usize::MAX).min(line_count);
    if first > last {
        return Ok(empty());
    }
    let lines = (first..=last)
        .map(|lnum| {
            text.line(lnum)
                .map(|line| Typval::String(OxStr(line.clone())))
                .map_err(|error| EvalError::new("E16", 0, error.to_string()))
        })
        .collect::<ox_eval::Result<Vec<_>>>()?;
    Ok(Typval::list(lines))
}

/// `getbufinfo([{buf}])`: returns a list with one dict per buffer
/// (`f_getbufinfo`, `eval/buffer.c:711`).  When a buffer number is given,
/// only that buffer's info is returned; with no argument, all buffers.
/// Each dict carries `bufnr`, `name`, `loaded`, `hidden`, `listed`,
/// `changed`, `changedtick`, and `linecount` — the fields the functional
/// suite checks.
fn call_getbufinfo_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    enum BufSelection {
        All,
        One(BufHandle),
        None,
    }
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: getbufinfo",
        ));
    }
    // With a dict argument, upstream filters by buflisted/bufloaded/bufmodified.
    // The functional suite never uses that form, so we only handle the
    // no-arg (all buffers) and numeric-arg (one buffer) cases.
    let selection = if args.is_empty() {
        BufSelection::All
    } else {
        match &args[0] {
            Typval::Number(0) => match editor.current_buffer() {
                Some(handle) => BufSelection::One(handle),
                None => BufSelection::None,
            },
            Typval::Number(nr) => match BufHandle::try_from(*nr)
                .ok()
                .filter(|handle| editor.buffer(*handle).is_ok())
            {
                Some(handle) => BufSelection::One(handle),
                None => BufSelection::None,
            },
            Typval::String(name) if name.as_bytes().is_empty() || name.as_bytes() == b"%" => {
                match editor.current_buffer() {
                    Some(handle) => BufSelection::One(handle),
                    None => BufSelection::None,
                }
            }
            Typval::String(name) => {
                match editor.buffers().into_iter().find(|handle| {
                    editor
                        .buffer(*handle)
                        .is_ok_and(|state| state.name() == name)
                }) {
                    Some(handle) => BufSelection::One(handle),
                    None => BufSelection::None,
                }
            }
            _ => BufSelection::None,
        }
    };
    let handles: Vec<BufHandle> = match selection {
        BufSelection::All => editor.buffers(),
        BufSelection::One(handle) => vec![handle],
        BufSelection::None => Vec::new(),
    };
    let entries: Vec<Typval> = handles
        .into_iter()
        .filter_map(|handle| {
            let state = editor.buffer(handle).ok()?;
            let loaded = state.residency.is_loaded();
            let hidden = loaded && state.attachments == 0;
            let listed = state.flags.contains(crate::BufferFlags::LISTED);
            let changed = state.flags.contains(crate::BufferFlags::MODIFIED);
            let changedtick = i64::try_from(state.script_changedtick()).unwrap_or(i64::MAX);
            let linecount = state.text().ok().map_or(0, |text| {
                i64::try_from(text.line_count()).unwrap_or(i64::MAX)
            });
            let name = state.name().clone();
            Some(Typval::dict(vec![
                (OxStr::from("bufnr"), Typval::Number(i64::from(handle))),
                (OxStr::from("name"), Typval::String(name)),
                (OxStr::from("lnum"), Typval::Number(1)),
                (OxStr::from("linecount"), Typval::Number(linecount)),
                (OxStr::from("loaded"), Typval::Number(i64::from(loaded))),
                (OxStr::from("listed"), Typval::Number(i64::from(listed))),
                (OxStr::from("changed"), Typval::Number(i64::from(changed))),
                (OxStr::from("changedtick"), Typval::Number(changedtick)),
                (OxStr::from("hidden"), Typval::Number(i64::from(hidden))),
                (OxStr::from("command"), Typval::Number(0)),
                (OxStr::from("lastused"), Typval::Number(0)),
            ]))
        })
        .collect();
    Ok(Typval::list(entries))
}

/// Deletes an inclusive line range from a loaded buffer.
fn call_deletebufline_builtin(editor: &mut Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() < 2 {
        return Err(EvalError::new(
            "E119",
            0,
            "Not enough arguments for function: deletebufline",
        ));
    }
    if args.len() > 3 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: deletebufline",
        ));
    }
    let Some(buffer) = resolve_buffer_argument(editor, args.first()) else {
        return Ok(Typval::Number(1));
    };
    let parse_line = |value: &Typval| -> ox_eval::Result<Option<i64>> {
        match value {
            Typval::String(text) if text.as_bytes() == b"$" => Ok(None),
            _ => number_value(value).map(Some),
        }
    };
    let first_value = parse_line(&args[1])?;
    let last_value = args.get(2).map(parse_line).transpose()?;

    let Ok(state) = editor.buffer(buffer) else {
        return Ok(Typval::Number(1));
    };
    if !state.residency.is_loaded() {
        return Ok(Typval::Number(1));
    }
    let Ok(text) = state.text() else {
        return Ok(Typval::Number(1));
    };
    let line_count = text.line_count();
    let line_count_number = i64::try_from(line_count).unwrap_or(i64::MAX);
    let first_number = first_value.unwrap_or(line_count_number);
    let Ok(first) = usize::try_from(first_number) else {
        return Ok(Typval::Number(1));
    };
    if first == 0 || first > line_count {
        return Ok(Typval::Number(1));
    }
    let last_number = match last_value {
        None => first_number,
        Some(value) => value.unwrap_or(line_count_number),
    };
    if last_number < first_number {
        return Ok(Typval::Number(1));
    }
    let last = usize::try_from(last_number)
        .unwrap_or(usize::MAX)
        .min(line_count);
    if matches!(
        editor.options().get_buffer(buffer, "modifiable"),
        Ok(OptionValue::Boolean(false))
    ) {
        return Err(EvalError::new(
            "E21",
            0,
            "Cannot make changes, 'modifiable' is off",
        ));
    }

    let cursor = editor
        .windows()
        .into_iter()
        .find(|window| {
            editor
                .window(*window)
                .is_ok_and(|state| state.buffer == buffer)
        })
        .and_then(|window| editor.window(window).ok())
        .map_or(Position { lnum: 1, col: 0 }, |state| state.cursor);
    let result = editor.replace_buffer_lines(LineReplaceRequest {
        buffer,
        start: first,
        end: last,
        lines: &[],
        cursor_before: cursor,
        cursor_after: cursor,
        timestamp: 0,
    });
    Ok(Typval::Number(i64::from(result.is_err())))
}

/// `changenr()`: the sequence number of the buffer's current undo state
/// (`f_changenr`, `eval/funcs.c:604-607`, reading `b_u_seq_cur`).
///
/// Because a header keeps collecting edits until the block closes, every
/// change that joins an open block answers the same number — which is the
/// whole point of the grouping this reads.
fn call_changenr_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: changenr",
        ));
    }
    let seq = editor
        .current_buffer()
        .and_then(|buffer| editor.buffer_undo_tree(buffer).ok())
        .map_or(0, ox_text::UndoTree::current_seq);
    Ok(Typval::Number(i64::try_from(seq).unwrap_or(i64::MAX)))
}

/// `undotree([{buf}])`: the buffer's undo state and header list
/// (`f_undotree`, `undo.c:3243-3263`).
///
/// `save_last` and `save_cur` are always zero: they count `:write`s recorded
/// into headers through `u_unchanged`/`uh_save_nr`, which this port's undo
/// tree does not carry. Every other field is real, including `synced`, which
/// reports whether a block is still open.
fn call_undotree_builtin(editor: &Editor, args: &[Typval]) -> ox_eval::Result<Typval> {
    if args.len() > 1 {
        return Err(EvalError::new(
            "E118",
            0,
            "Too many arguments for function: undotree",
        ));
    }
    let buffer = match args.first() {
        // `get_buf_arg` returning NULL leaves the empty dict upstream builds.
        Some(argument) => match resolve_buffer_argument(editor, Some(argument)) {
            Some(buffer) => buffer,
            None => return Ok(Typval::dict(Vec::new())),
        },
        None => editor
            .current_buffer()
            .ok_or_else(|| EvalError::new("E749", 0, "Empty buffer"))?,
    };
    let tree = editor
        .buffer_undo_tree(buffer)
        .map_err(|error| EvalError::new("E749", 0, error.to_string()))?;
    let summary = tree.summary();
    let entry = |name: &str, value: i64| (OxStr::from(name), Typval::Number(value));
    let fields = vec![
        entry("synced", i64::from(tree.is_synced())),
        entry(
            "seq_last",
            i64::try_from(summary.seq_last).unwrap_or(i64::MAX),
        ),
        entry("save_last", 0),
        entry(
            "seq_cur",
            i64::try_from(summary.seq_cur).unwrap_or(i64::MAX),
        ),
        entry("time_cur", summary.time_cur),
        entry("save_cur", 0),
        (OxStr::from("entries"), undotree_entries(&tree.entries())),
    ];
    Ok(Typval::dict(fields))
}

/// One `undotree()` header list, with `newhead`, `curhead` and `alt` present
/// only when they apply, exactly as `u_eval_tree` adds them.
fn undotree_entries(nodes: &[ox_text::UndoTreeNode]) -> Typval {
    let items = nodes
        .iter()
        .map(|node| {
            let mut fields = vec![
                (
                    OxStr::from("seq"),
                    Typval::Number(i64::try_from(node.seq).unwrap_or(i64::MAX)),
                ),
                (OxStr::from("time"), Typval::Number(node.timestamp)),
            ];
            if node.newhead {
                fields.push((OxStr::from("newhead"), Typval::Number(1)));
            }
            if node.curhead {
                fields.push((OxStr::from("curhead"), Typval::Number(1)));
            }
            if !node.alt.is_empty() {
                fields.push((OxStr::from("alt"), undotree_entries(&node.alt)));
            }
            Typval::dict(fields)
        })
        .collect();
    Typval::list(items)
}

fn current_line_address(editor: &mut Editor, value: &Typval) -> ox_eval::Result<usize> {
    let seam = CurrentBuffer(editor);
    let line = match value {
        Typval::String(address) if address.as_bytes() == b"$" => {
            i64::try_from(seam.line_count()?).unwrap_or(i64::MAX)
        }
        Typval::String(address) => seam.address_line(&address.to_string_lossy())?.unwrap_or(0),
        _ => typval_number(value).unwrap_or(0),
    };
    Ok(usize::try_from(line.max(0)).unwrap_or(usize::MAX))
}

#[cfg(test)]
#[cfg_attr(test, path = "buffer_lifecycle_tests.rs")]
mod buffer_lifecycle_tests;
