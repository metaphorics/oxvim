//! Autocommand API over the editor's ordered firing planner.

use ox_editor::{
    AugroupId, AutocmdContext, AutocmdDefinition, AutocmdError, AutocmdFilter, AutocmdKind,
    AutocmdOptions, BufferRelease, Editor, EditorError, Event, FiringPlan, OptionValue,
};

use crate::runtime::{
    AutocmdExecution, release_autocmd_callback, take_pending_autocmd_callback_releases,
    with_autocmd_executor,
};
use crate::{
    ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, WinHandle, api,
    session::ApiSession,
};

/// Executes one planner output.
///
/// `++once` definitions are consumed before the host runs them, matching
/// upstream `do_doautocmd`, so a handler can neither re-fire nor observe
/// them; a failing action aborts the remainder with the consumption already
/// recorded.
///
/// # Errors
///
/// Returns an error when an autocommand action fails or when releasing a
/// removed callback fails.
pub fn execute_firing_plan(session: &ApiSession, plan: FiringPlan) -> Result<(), ApiError> {
    for action in plan.ready {
        if !session.with_editor(|editor| editor.autocmds().is_entry_live(action.entry_id)) {
            continue;
        }
        let mut removed = Vec::new();
        if action.once {
            // One `nvim_create_autocmd` call shares one raw Lua reference
            // across its event×pattern siblings, so the consumed payload
            // releases only when no sibling still references it.
            if let Some(kind) = session
                .with_editor_mut(|editor| editor.autocmds_mut().consume_once(action.entry_id))
            {
                removed.push(kind);
            }
        }
        let execution = with_autocmd_executor(session, &action);
        if execution == Ok(AutocmdExecution::Delete)
            && let Some(kind) = session
                .with_editor_mut(|editor| editor.autocmds_mut().delete_entry(action.entry_id))
        {
            removed.push(kind);
        }
        let cleanup = release_removed(session, removed);
        match execution {
            Err(error) => return Err(error),
            Ok(_) => cleanup?,
        }
    }
    Ok(())
}

/// Releases the executable payloads the editor's removal functions return.
///
/// One `nvim_create_autocmd` call clones one Lua registry reference across
/// every event×pattern entry, so callback ids are deduplicated and each is
/// released only when no remaining definition still references it:
/// consuming or deleting one sibling must never invalidate the survivors.
fn release_removed(
    session: &ApiSession,
    removals: impl IntoIterator<Item = AutocmdKind>,
) -> Result<(), ApiError> {
    let mut references = take_pending_autocmd_callback_releases(session);
    for kind in removals {
        let AutocmdKind::LuaCallback(reference) = kind else {
            continue;
        };
        if !references.contains(&reference) {
            references.push(reference);
        }
    }
    for reference in references {
        let used = session.with_editor(|editor| editor.autocmds().uses_lua_callback(reference));
        if !used {
            release_autocmd_callback(session, reference)?;
        }
    }
    Ok(())
}

/// Runs `run` in the display context of `buffer` for autocmd execution
/// (upstream `ctx_switch`/`ctx_restore`), mirroring
/// `Editor::in_buffer_context` with shortest-scope session borrows: the
/// switch and the restore each hold the editor only for their statement, so
/// `run` re-enters APIs through `session` while no editor borrow is live.
///
/// The first existing window already showing `buffer` and not ignoring
/// `event` through its window-local `eventignorewin` becomes current; if
/// every such window ignores the event, or global `eventignore` gates the
/// event, nothing runs and `None` is returned. A hidden buffer is
/// temporarily displayed in the caller window. Every window change is undone
/// on the way out without masking `run`'s result.
///
/// # Errors
///
/// Returns the context-switch errors as exceptions, exactly like the
/// converted outer result of `Editor::in_buffer_context`.
fn run_in_buffer_context(
    session: &ApiSession,
    event: Event,
    buffer: BufHandle,
    run: impl FnOnce() -> Result<(), ApiError>,
) -> Result<Option<Result<(), ApiError>>, ApiError> {
    let switch_error = |error: EditorError| ApiError::exception(error.to_string());
    if session.with_editor(|editor| editor.autocmds().is_ignored(event)) {
        return Ok(None);
    }
    // Upstream `aucmd_prepbuf` loads the target from disk before entering;
    // the API layer has no file IO, so when the target exists but its text
    // is not resident, fire without entering rather than forcing an
    // unloadable buffer current. Callbacks still observe the target through
    // the event context (`<abuf>`); only the current-buffer switch is lost.
    let enterable = session.with_editor(|editor| {
        let target = if buffer.is_current() {
            editor.current_buffer()
        } else {
            Some(buffer)
        };
        target.map(|handle| {
            editor
                .buffer(handle)
                .is_ok_and(|state| state.residency.is_loaded())
        })
    });
    if enterable == Some(false) {
        return Ok(Some(run()));
    }
    // Decide the entering window without host code in between, so the state
    // cannot move between this read and the switch that follows.
    let (target, caller, caller_buffer, selected, skipped) = session.with_editor(|editor| {
        let target = if buffer.is_current() {
            editor
                .current_buffer()
                .ok_or_else(|| switch_error(EditorError::NoCurrentTabpage))?
        } else {
            buffer
        };
        let caller = editor
            .current_window()
            .ok_or_else(|| switch_error(EditorError::NoCurrentTabpage))?;
        let caller_buffer = editor.window(caller).ok().map(|state| state.buffer);
        let window_ignores =
            |window: WinHandle| match editor.options().get_window(window, "eventignorewin") {
                Ok(OptionValue::String(ignored)) => ignored
                    .split(',')
                    .any(|name| name == "all" || Event::from_name(name) == Some(event)),
                _ => false,
            };
        let visible: Vec<WinHandle> = editor
            .windows()
            .into_iter()
            .filter(|&window| {
                editor
                    .window(window)
                    .is_ok_and(|state| state.buffer == target)
            })
            .collect();
        let selected = visible
            .iter()
            .copied()
            .find(|&window| !window_ignores(window));
        let skipped = !visible.is_empty() && visible.iter().all(|&window| window_ignores(window));
        Ok((target, caller, caller_buffer, selected, skipped))
    })?;
    if skipped {
        return Ok(None);
    }
    if caller_buffer == Some(target) && selected == Some(caller) {
        return Ok(Some(run()));
    }
    let changed = session.with_editor_mut(
        |editor| -> Result<Option<(WinHandle, BufHandle)>, ApiError> {
            Ok(match selected {
                Some(window) if window != caller => {
                    editor.set_current_window(window).map_err(switch_error)?;
                    Some((window, target))
                }
                Some(_) => None,
                None => {
                    let original = caller_buffer.unwrap_or(target);
                    editor
                        .set_current_buffer(target, BufferRelease::KeepLoaded)
                        .map_err(switch_error)?;
                    Some((caller, original))
                }
            })
        },
    )?;
    let result = run();
    session.with_editor_mut(|editor| {
        if let Some((window, original)) = changed
            && editor
                .window(window)
                .is_ok_and(|state| state.buffer != original)
            && editor.buffer(original).is_ok()
        {
            let _ = editor.set_window_buffer(window, original, BufferRelease::KeepLoaded);
        }
        if editor.current_window() != Some(caller) && editor.window(caller).is_ok() {
            let _ = editor.set_current_window(caller);
        }
    });
    Ok(Some(result))
}

/// Plans and executes `Event::FileType` for one committed buffer-local
/// 'filetype' assignment.
///
/// The raw filetype is the match text and the buffer name is the file text;
/// the occurrence stays top-level (`nested`), so only the definitions' own
/// `++nested` flags gate events raised while they run.
pub(crate) fn fire_filetype(
    session: &ApiSession,
    buffer: BufHandle,
    file_type: &str,
) -> Result<(), ApiError> {
    let in_flight = session.with_state(|state| {
        state
            .filetype_dispatches
            .iter()
            .any(|entry| entry.0 == buffer && entry.1 == file_type)
    });
    if in_flight {
        return Ok(());
    }
    let file_name = session
        .with_editor(|editor| {
            editor
                .buffer(buffer)
                .map(|state| state.name().to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let plan = session.with_editor_mut(|editor| {
        editor.autocmds_mut().plan(
            Event::FileType,
            AutocmdContext {
                buffer: Some(buffer),
                file_name: Some(&file_name),
                match_name: Some(file_type),
                nested: true,
                data: None,
            },
        )
    });
    session.with_state_mut(|state| {
        state
            .filetype_dispatches
            .push((buffer, file_type.to_owned()));
    });
    let result = execute_firing_plan(session, plan);
    session.with_state_mut(|state| state.filetype_dispatches.pop());
    result
}

fn text(value: &OxStr, what: &str) -> Result<String, ApiError> {
    String::from_utf8(value.0.clone())
        .map_err(|_| ApiError::validation(format!("{what} must be valid UTF-8")))
}
fn object_type_name(value: &Object) -> &'static str {
    match value {
        Object::Nil => "nil",
        Object::Boolean(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Float(_) => "Float",
        Object::String(_) => "String",
        Object::Array(_) => "Array",
        Object::Dict(_) => "Dict",
        Object::LuaRef(_) => "Function",
        Object::Buffer(_) => "Buffer",
        Object::Window(_) => "Window",
        Object::Tabpage(_) => "Tabpage",
    }
}

fn strings(value: &Object, what: &str) -> Result<Vec<String>, ApiError> {
    match value {
        Object::String(value) => Ok(vec![text(value, what)?]),
        Object::Array(values) => values
            .iter()
            .map(|value| match value {
                Object::String(value) => text(value, what),
                value => Err(ApiError::validation(format!(
                    "Invalid '{what}' item: expected String, got {}",
                    object_type_name(value)
                ))),
            })
            .collect(),
        value => Err(ApiError::validation(format!(
            "Invalid '{what}': expected Array or String, got {}",
            object_type_name(value)
        ))),
    }
}

fn event_names(value: &Object) -> Result<Vec<String>, ApiError> {
    match value {
        Object::String(name) => Ok(vec![text(name, "event")?]),
        Object::Array(values) => values
            .iter()
            .map(|value| match value {
                Object::String(name) => text(name, "event"),
                value => Err(ApiError::validation(format!(
                    "Invalid 'event' item: expected String, got {}",
                    object_type_name(value)
                ))),
            })
            .collect(),
        value => Err(ApiError::validation(format!(
            "Invalid 'event': expected Array or String, got {}",
            object_type_name(value)
        ))),
    }
}

fn parse_events(names: Vec<String>) -> Result<Vec<Event>, ApiError> {
    names
        .into_iter()
        .map(|name| {
            Event::from_name(&name)
                .ok_or_else(|| ApiError::validation(format!("Invalid 'event': '{name}'")))
        })
        .collect()
}

fn events(value: &Object) -> Result<Vec<Event>, ApiError> {
    let names = event_names(value)?;
    if names.is_empty() {
        return Err(ApiError::validation("Required: 'event'"));
    }
    parse_events(names)
}

fn exec_events(value: &Object) -> Result<Vec<Event>, ApiError> {
    parse_events(event_names(value)?)
}

fn group(session: &ApiSession, value: Option<&Object>) -> Result<AugroupId, ApiError> {
    match value {
        None | Some(Object::Nil) => Ok(AugroupId::default()),
        Some(Object::Integer(raw)) if *raw > 0 => {
            let id = AugroupId(
                u64::try_from(*raw)
                    .map_err(|_| ApiError::validation(format!("Invalid 'group': {raw}")))?,
            );
            let live = session.with_editor(|editor| editor.autocmds().is_live_group(id));
            if live {
                Ok(id)
            } else {
                Err(ApiError::validation(format!("Invalid 'group': {raw}")))
            }
        }
        Some(Object::Integer(raw)) => Err(ApiError::validation(format!("Invalid 'group': {raw}"))),
        Some(Object::String(name)) => {
            let name = text(name, "group")?;
            session
                .with_editor(|editor| editor.autocmds().group(&name))
                .ok_or_else(|| ApiError::validation(format!("Invalid 'group': '{name}'")))
        }
        Some(value) => Err(ApiError::validation(format!(
            "Invalid 'group': expected String or Integer, got {}",
            object_type_name(value)
        ))),
    }
}

fn bool_opt(opts: &Dict, name: &str, default: bool) -> Result<bool, ApiError> {
    match opts.get(&OxStr::from(name)) {
        None => Ok(default),
        Some(Object::Boolean(value)) => Ok(*value),
        Some(_) => Err(ApiError::validation(format!("'{name}' must be a boolean"))),
    }
}

fn buffer(session: &ApiSession, value: &Object, key: &str) -> Result<BufHandle, ApiError> {
    let raw = match value {
        Object::Integer(value) => *value,
        Object::Buffer(value) => i64::from(*value),
        value => {
            return Err(ApiError::validation(format!(
                "Invalid '{key}': expected Buffer, got {}",
                object_type_name(value)
            )));
        }
    };
    let handle = BufHandle::try_from(raw)
        .map_err(|_| ApiError::validation(format!("Invalid buffer id: {raw}")))?;
    let handle = if handle.is_current() {
        session
            .with_editor(Editor::current_buffer)
            .ok_or_else(|| ApiError::validation(format!("Invalid buffer id: {raw}")))?
    } else {
        handle
    };
    let live = session.with_editor(|editor| editor.buffer(handle).is_ok());
    if !live {
        return Err(ApiError::validation(format!("Invalid buffer id: {raw}")));
    }
    Ok(handle)
}

fn selected_buffer(session: &ApiSession, opts: &Dict) -> Result<Option<BufHandle>, ApiError> {
    let deprecated_buffer = opts.get(&OxStr::from("buffer"));
    let buf = opts.get(&OxStr::from("buf"));
    if deprecated_buffer.is_some() && buf.is_some() {
        return Err(ApiError::validation(
            "Conflict: 'buf' not allowed with 'buffer'",
        ));
    }
    match (buf, deprecated_buffer) {
        (Some(value), None) => buffer(session, value, "buf").map(Some),
        (None, Some(value)) => buffer(session, value, "buffer").map(Some),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => unreachable!(),
    }
}

fn query_patterns(value: &Object) -> Result<Vec<String>, ApiError> {
    match value {
        Object::String(pattern) => Ok(vec![text(pattern, "pattern")?]),
        Object::Array(values) => values
            .iter()
            .map(|value| match value {
                Object::String(pattern) => text(pattern, "pattern"),
                value => Err(ApiError::validation(format!(
                    "Invalid 'pattern' item: expected String, got {}",
                    object_type_name(value)
                ))),
            })
            .collect(),
        value => Err(ApiError::validation(format!(
            "Invalid 'pattern': expected String or Array, got {}",
            object_type_name(value)
        ))),
    }
}

/// Normalizes `<buffer>`/`<buffer=0>` pattern filters to the canonical
/// `<buffer=N>` form so they compare equal to stored buffer-local patterns.
fn canonical_patterns(session: &ApiSession, opts: &Dict) -> Result<Option<Vec<String>>, ApiError> {
    let Some(value) = opts.get(&OxStr::from("pattern")) else {
        return Ok(None);
    };
    Ok(Some(
        query_patterns(value)?
            .into_iter()
            .map(|pattern| match pattern.as_str() {
                "<buffer>" | "<buffer=0>" => match session.with_editor(Editor::current_buffer) {
                    Some(buffer) => format!("<buffer={}>", i64::from(buffer)),
                    None => pattern,
                },
                _ => pattern,
            })
            .collect(),
    ))
}

/// Parses the `buf`/`buffer` query option of get/clear: one Buffer/Integer or
/// an array of them, each resolved to a live handle. Type errors always name
/// the `buffer` key, matching upstream `nvim_get_autocmds`
/// (`api/autocmd.c:190-223`).
fn buffer_query(session: &ApiSession, opts: &Dict) -> Result<Option<Vec<BufHandle>>, ApiError> {
    const MAX_BUFFERS: usize = 256;
    let Some(value) = opts
        .get(&OxStr::from("buf"))
        .or_else(|| opts.get(&OxStr::from("buffer")))
    else {
        return Ok(None);
    };
    let items: Vec<Object> = match value {
        Object::Integer(_) | Object::Buffer(_) => vec![value.clone()],
        Object::Array(items) => {
            if items.len() > MAX_BUFFERS {
                return Err(ApiError::validation(format!(
                    "Too many buffers (maximum of {MAX_BUFFERS})"
                )));
            }
            items.clone()
        }
        value => {
            return Err(ApiError::validation(format!(
                "Invalid 'buffer': expected Integer or Array, got {}",
                object_type_name(value)
            )));
        }
    };
    let buffers = items
        .into_iter()
        .map(|item| match &item {
            Object::Integer(_) | Object::Buffer(_) => buffer(session, &item, "buffer"),
            value => Err(ApiError::validation(format!(
                "Invalid 'buffer': expected Integer, got {}",
                object_type_name(value)
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(buffers))
}

/// Rejects mutually exclusive query keys at each API's upstream validation
/// point (`api/autocmd.c:153-161`, `557-581`).
fn reject_query_conflicts(opts: &Dict) -> Result<(), ApiError> {
    let has_buffer = opts.get(&OxStr::from("buf")).is_some();
    let has_buffer_deprecated = opts.get(&OxStr::from("buffer")).is_some();
    if has_buffer && has_buffer_deprecated {
        return Err(ApiError::validation(
            "Conflict: 'buf' not allowed with 'buffer'",
        ));
    }
    if opts.get(&OxStr::from("pattern")).is_some() && (has_buffer || has_buffer_deprecated) {
        return Err(ApiError::validation(
            "Conflict: 'pattern' not allowed with 'buf'",
        ));
    }
    Ok(())
}

fn autocmd_kind(opts: &Dict) -> Result<AutocmdKind, ApiError> {
    let command = opts.get(&OxStr::from("command"));
    let callback = opts.get(&OxStr::from("callback"));
    match (command, callback) {
        (Some(Object::String(command)), None) => {
            Ok(AutocmdKind::ExString(text(command, "command")?))
        }
        (None, Some(Object::LuaRef(callback))) => Ok(AutocmdKind::LuaCallback(
            u64::try_from(*callback).map_err(|_| ApiError::validation("Invalid 'callback'"))?,
        )),
        (None, Some(Object::String(callback))) => {
            Ok(AutocmdKind::VimscriptFunction(text(callback, "callback")?))
        }
        (Some(_), Some(_)) => Err(ApiError::validation(
            "Conflict: 'callback' not allowed with 'command'",
        )),
        (None, None) => Err(ApiError::validation("Required: 'command' or 'callback'")),
        (Some(value), None) => Err(ApiError::validation(format!(
            "Invalid 'command': expected String, got {}",
            object_type_name(value)
        ))),
        (None, Some(value)) => Err(ApiError::validation(format!(
            "Invalid 'callback': expected Lua function or Vim function name, got {}",
            object_type_name(value)
        ))),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes event and opts as owned values"
)]
#[api(since = 9, fast)]
pub fn nvim_create_autocmd(
    session: &ApiSession,
    event: Object,
    opts: Dict,
) -> Result<i64, ApiError> {
    // Upstream validates the option keyset before the function body, so an
    // unknown key is rejected ahead of command/callback validation.
    for (key, _) in opts.iter() {
        let key = key.to_string_lossy().into_owned();
        if !matches!(
            key.as_str(),
            "buffer"
                | "buf"
                | "callback"
                | "command"
                | "desc"
                | "group"
                | "nested"
                | "once"
                | "pattern"
        ) {
            return Err(ApiError::validation(format!("Invalid key: {key}")));
        }
    }
    // An empty event list reports the required event, matching upstream's
    // keyset-driven `Required: 'event'`.
    if matches!(&event, Object::Array(items) if items.is_empty()) {
        return Err(ApiError::validation("Required: 'event'"));
    }
    let events = events(&event)?;
    let kind = autocmd_kind(&opts)?;
    let group = group(session, opts.get(&OxStr::from("group")))?;
    let once = bool_opt(&opts, "once", false)?;
    let nested = bool_opt(&opts, "nested", false)?;
    let description = opts
        .get(&OxStr::from("desc"))
        .map(|value| match value {
            Object::String(value) => text(value, "desc"),
            value => Err(ApiError::validation(format!(
                "Invalid 'desc': expected String, got {}",
                object_type_name(value)
            ))),
        })
        .transpose()?;

    let buffer_option = opts.get(&OxStr::from("buffer"));
    let buf_option = opts.get(&OxStr::from("buf"));
    if buffer_option.is_some() && buf_option.is_some() {
        return Err(ApiError::validation(
            "Conflict: 'buf' not allowed with 'buffer'",
        ));
    }
    if opts.get(&OxStr::from("pattern")).is_some()
        && (buffer_option.is_some() || buf_option.is_some())
    {
        return Err(ApiError::validation(
            "Conflict: 'pattern' not allowed with 'buf'",
        ));
    }
    let mut selected_buffer = selected_buffer(session, &opts)?;
    let patterns = match opts.get(&OxStr::from("pattern")) {
        Some(value) => strings(value, "pattern")?,
        None if selected_buffer.is_some() => vec!["<abuf>".to_owned()],
        None => vec!["*".to_owned()],
    };
    // A bare `<buffer>`/`<buffer=0>` pattern item binds the current buffer;
    // explicit `<buffer=N>` items are already canonical and keep their number.
    if selected_buffer.is_none()
        && patterns.iter().any(|pattern| {
            pattern
                .split(',')
                .any(|item| matches!(item, "<buffer>" | "<buffer=0>"))
        })
    {
        selected_buffer = session.with_editor(Editor::current_buffer);
    }
    // One registration assigns one shared API id to every event×pattern
    // entry, so the returned handle deletes the whole batch.
    let api_id = session
        .with_editor_mut(|editor| {
            editor.autocmds_mut().register_api(
                &events,
                &patterns.join(","),
                &kind,
                &AutocmdOptions {
                    group,
                    buffer: selected_buffer,
                    once,
                    nested,
                    description,
                },
            )
        })
        .map_err(|error| match error {
            AutocmdError::EmptyPattern => ApiError::validation("No non-empty patterns specified"),
            error => ApiError::validation(error.to_string()),
        })?;
    i64::try_from(api_id).map_err(|_| ApiError::exception("autocmd id exceeds Integer range"))
}

#[api(since = 9)]
pub fn nvim_del_autocmd(session: &ApiSession, id: i64) -> Result<(), ApiError> {
    let api_id = u64::try_from(id)
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| ApiError::validation(format!("Invalid 'id': {id}")))?;
    let removed = session.with_editor_mut(|editor| editor.autocmds_mut().delete_api_id(api_id));
    if removed.is_empty() {
        return Err(ApiError::validation(format!("Invalid 'id': {id}")));
    }
    release_removed(session, removed)
}

#[api(since = 9)]
pub fn nvim_del_augroup_by_id(session: &ApiSession, id: i64) -> Result<(), ApiError> {
    // Upstream `augroup_del` reports every missing group as E367 through the
    // exception layer; id 0 and unknown ids resolve to "[NULL]", negative ids
    // to the "--Deleted--" gap name.
    let resolved = if id > 0 {
        u64::try_from(id).ok().map(AugroupId).and_then(|group| {
            session.with_editor(|editor| {
                editor
                    .autocmds()
                    .group_name(group)
                    .map(|name| (group, name.to_owned()))
            })
        })
    } else {
        None
    };
    let Some((group, _)) = resolved else {
        let name = if id < 0 { "--Deleted--" } else { "[NULL]" };
        return Err(ApiError::exception(format!(
            "Vim:E367: No such group: \"{name}\""
        )));
    };
    let removed = session
        .with_editor_mut(|editor| editor.autocmds_mut().delete_group(group))
        .map_err(|error| ApiError::validation(error.to_string()))?;
    release_removed(session, removed)?;
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes name as an owned value"
)]
#[api(since = 9)]
pub fn nvim_del_augroup_by_name(session: &ApiSession, name: OxStr) -> Result<(), ApiError> {
    let name = text(&name, "name")?;
    let Some(group) = session.with_editor(|editor| editor.autocmds().group(&name)) else {
        return Err(ApiError::exception(format!(
            "Vim:E367: No such group: \"{name}\""
        )));
    };
    let removed = session
        .with_editor_mut(|editor| editor.autocmds_mut().delete_group(group))
        .map_err(|error| ApiError::validation(error.to_string()))?;
    release_removed(session, removed)?;
    Ok(())
}

fn query_event_names(value: &Object) -> Result<Vec<String>, ApiError> {
    match value {
        Object::String(name) => Ok(vec![text(name, "event")?]),
        Object::Array(values) => values
            .iter()
            .map(|value| match value {
                Object::String(name) => text(name, "event"),
                value => Err(ApiError::validation(format!(
                    "Invalid 'event' item: expected String, got {}",
                    object_type_name(value)
                ))),
            })
            .collect(),
        // Query validation reports the bare expectation, unlike the
        // create/exec paths which name the offending type.
        _ => Err(ApiError::validation(
            "Invalid 'event': expected String or Array",
        )),
    }
}

fn filter_events(opts: &Dict) -> Result<Option<Vec<Event>>, ApiError> {
    match opts.get(&OxStr::from("event")) {
        // Upstream `has_key` treats an explicit nil like an absent key.
        None | Some(Object::Nil) => Ok(None),
        // An explicitly empty event list selects nothing instead of failing.
        Some(Object::Array(items)) if items.is_empty() => Ok(Some(Vec::new())),
        Some(value) => parse_events(query_event_names(value)?).map(Some),
    }
}

fn clear_filter_events(opts: &Dict) -> Result<Option<Vec<Event>>, ApiError> {
    match opts.get(&OxStr::from("event")) {
        None | Some(Object::Nil) => Ok(None),
        Some(Object::Array(items)) if items.is_empty() => Ok(Some(Vec::new())),
        Some(value) => events(value).map(Some),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes opts as an owned value"
)]
#[api(since = 9)]
pub fn nvim_clear_autocmds(session: &ApiSession, opts: Dict) -> Result<(), ApiError> {
    let selected_events = clear_filter_events(&opts)?;
    reject_query_conflicts(&opts)?;
    let selected_group = group(session, opts.get(&OxStr::from("group")))?;
    // Per api.txt `nvim_clear_autocmds()`: when no group is given, only
    // autocommands that are NOT in any group (the default augroup) match,
    // never every group.
    let patterns = canonical_patterns(session, &opts)?;
    let buffers = buffer_query(session, &opts)?;
    let filter = AutocmdFilter {
        group: Some(selected_group),
        events: selected_events.as_deref(),
        patterns: patterns.as_deref(),
        buffers: buffers.as_deref(),
        api_id: None,
    };
    let removed = session.with_editor_mut(|editor| editor.autocmds_mut().clear(&filter));
    release_removed(session, removed)?;
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes name and opts as owned values"
)]
#[api(since = 9)]
pub fn nvim_create_augroup(session: &ApiSession, name: OxStr, opts: Dict) -> Result<i64, ApiError> {
    let id = session.with_editor_mut(|editor| {
        editor
            .autocmds_mut()
            .create_group(&text(&name, "name")?, bool_opt(&opts, "clear", true)?)
            .map_err(|error| ApiError::validation(error.to_string()))
    })?;
    i64::try_from(id.0).map_err(|_| ApiError::exception("augroup id exceeds Integer range"))
}
/// Builds one `nvim_get_autocmds` result entry from a queried definition.
fn autocmd_definition(item: &AutocmdDefinition) -> Dict {
    let mut entries = Vec::new();
    // Group metadata stays on entries of legacy-deleted groups, which
    // report the `--Deleted--` name; only the default group omits it.
    if item.group != AugroupId::default() {
        entries.push((
            OxStr::from("group"),
            Object::Integer(i64::try_from(item.group.0).unwrap_or(i64::MAX)),
        ));
        entries.push((
            OxStr::from("group_name"),
            Object::String(OxStr::from(
                item.group_name.as_deref().unwrap_or("--Deleted--"),
            )),
        ));
    }
    if let Some(api_id) = item.api_id {
        entries.push((
            OxStr::from("id"),
            Object::Integer(i64::try_from(api_id).unwrap_or(i64::MAX)),
        ));
    }
    if let Some(description) = &item.description {
        entries.push((
            OxStr::from("desc"),
            Object::String(OxStr::from(description.as_str())),
        ));
    }
    entries.push((
        OxStr::from("command"),
        match &item.kind {
            AutocmdKind::ExString(command) => Object::String(OxStr::from(command.as_str())),
            AutocmdKind::VimscriptFunction(_) | AutocmdKind::LuaCallback(_) => {
                Object::String(OxStr::from(""))
            }
        },
    ));
    match &item.kind {
        AutocmdKind::VimscriptFunction(name) => {
            entries.push((
                OxStr::from("callback"),
                Object::String(OxStr::from(name.as_str())),
            ));
        }
        AutocmdKind::LuaCallback(callback) => {
            entries.push((
                OxStr::from("callback"),
                Object::LuaRef(i32::try_from(*callback).unwrap_or(i32::MAX)),
            ));
        }
        AutocmdKind::ExString(_) => {}
    }
    entries.push((
        OxStr::from("pattern"),
        Object::String(OxStr::from(item.pattern.as_str())),
    ));
    entries.push((
        OxStr::from("event"),
        Object::String(OxStr::from(item.event.as_str())),
    ));
    entries.push((OxStr::from("once"), Object::Boolean(item.once)));
    entries.push((
        OxStr::from("buflocal"),
        Object::Boolean(item.buffer.is_some()),
    ));
    if let Some(buffer) = item.buffer {
        entries.push((OxStr::from("buf"), Object::Integer(i64::from(buffer))));
        entries.push((OxStr::from("buffer"), Object::Integer(i64::from(buffer))));
    }
    Dict(entries)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes opts as an owned value"
)]
#[api(since = 9)]
pub fn nvim_get_autocmds(session: &ApiSession, opts: Dict) -> Result<Vec<Dict>, ApiError> {
    let group_filter = opts
        .get(&OxStr::from("group"))
        .map(|value| group(session, Some(value)))
        .transpose()?;
    let selected_events = filter_events(&opts)?;
    reject_query_conflicts(&opts)?;
    let patterns = canonical_patterns(session, &opts)?;
    let id_filter = match opts.get(&OxStr::from("id")) {
        Some(Object::Integer(id)) => Some(*id),
        Some(_) => return Err(ApiError::validation("id must be an Integer")),
        None => None,
    };
    // The API id filter matches shared registrations only; an out-of-range
    // id selects nothing.
    let api_id_filter = match id_filter {
        Some(raw) => match u64::try_from(raw) {
            Ok(value) => Some(value),
            Err(_) => return Ok(Vec::new()),
        },
        None => None,
    };
    let buffers = buffer_query(session, &opts)?;
    let filter = AutocmdFilter {
        group: group_filter,
        events: selected_events.as_deref(),
        patterns: patterns.as_deref(),
        buffers: buffers.as_deref(),
        api_id: api_id_filter,
    };
    Ok(session.with_editor(|editor| {
        editor
            .autocmds()
            .query(&filter)
            .into_iter()
            .map(|item| autocmd_definition(&item))
            .collect()
    }))
}
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes event and opts as owned values"
)]
#[api(since = 9)]
pub fn nvim_exec_autocmds(session: &ApiSession, event: Object, opts: Dict) -> Result<(), ApiError> {
    // Upstream validates the option keyset before the function body.
    for (key, _) in opts.iter() {
        let key = key.to_string_lossy().into_owned();
        if !matches!(
            key.as_str(),
            "buf" | "buffer" | "data" | "group" | "modeline" | "pattern"
        ) {
            return Err(ApiError::validation(format!("Invalid key: {key}")));
        }
    }
    // An explicitly empty event list executes nothing.
    if matches!(&event, Object::Array(items) if items.is_empty()) {
        return Ok(());
    }
    let selected_events = exec_events(&event)?;
    let group = group(session, opts.get(&OxStr::from("group")))?;
    if let Some(modeline) = opts.get(&OxStr::from("modeline"))
        && !matches!(modeline, Object::Boolean(_) | Object::Nil)
    {
        return Err(ApiError::validation(format!(
            "Invalid 'modeline': expected Boolean, got {}",
            object_type_name(modeline)
        )));
    }
    let target = selected_buffer(session, &opts)?;
    if target.is_some() && opts.get(&OxStr::from("pattern")).is_some() {
        return Err(ApiError::validation(
            "Conflict: 'pattern' not allowed with 'buf'",
        ));
    }
    let data = opts.get(&OxStr::from("data")).cloned();
    // Comma spans inside every item are separate occurrences; a present but
    // fully empty pattern list executes nothing, while an absent pattern key
    // derives file and match text from the selected or current buffer.
    let occurrences: Vec<Option<String>> = match opts.get(&OxStr::from("pattern")) {
        Some(value) => strings(value, "pattern")?
            .iter()
            .flat_map(|item| item.split(','))
            .filter(|item| !item.is_empty())
            .map(|item| Some(item.to_owned()))
            .collect(),
        None => vec![None],
    };
    for event in selected_events {
        for pattern in &occurrences {
            let (file_name, context_buffer) = if let Some(name) = pattern {
                (
                    Some(name.clone()),
                    target.or_else(|| session.with_editor(Editor::current_buffer)),
                )
            } else {
                let buffer = target.or_else(|| session.with_editor(Editor::current_buffer));
                let name = buffer.and_then(|handle| {
                    session.with_editor(|editor| {
                        editor
                            .buffer(handle)
                            .ok()
                            .map(|state| state.name().to_string_lossy().into_owned())
                    })
                });
                (name, buffer)
            };
            // `match_name` stays unset so the editor derives and normalizes
            // `<amatch>` per event kind.
            let context = AutocmdContext {
                buffer: context_buffer,
                file_name: file_name.as_deref(),
                match_name: None,
                nested: true,
                data: data.as_ref(),
            };
            let plan = if group == AugroupId::default() {
                session.with_editor_mut(|editor| editor.autocmds_mut().plan(event, context))
            } else {
                session.with_editor_mut(|editor| {
                    editor.autocmds_mut().plan_in_group(event, group, context)
                })
            };
            match context_buffer {
                Some(buffer) => {
                    let outcome = run_in_buffer_context(session, event, buffer, || {
                        execute_firing_plan(session, plan)
                    })?;
                    if let Some(outcome) = outcome {
                        outcome?;
                    }
                }
                None => execute_firing_plan(session, plan)?,
            }
        }
    }
    Ok(())
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(
        nvim_create_autocmd__API_META(),
        nvim_create_autocmd__API_DISPATCH,
    )?;
    registry.register(nvim_del_autocmd__API_META(), nvim_del_autocmd__API_DISPATCH)?;
    registry.register(
        nvim_clear_autocmds__API_META(),
        nvim_clear_autocmds__API_DISPATCH,
    )?;
    registry.register(
        nvim_create_augroup__API_META(),
        nvim_create_augroup__API_DISPATCH,
    )?;
    registry.register(
        nvim_del_augroup_by_id__API_META(),
        nvim_del_augroup_by_id__API_DISPATCH,
    )?;
    registry.register(
        nvim_del_augroup_by_name__API_META(),
        nvim_del_augroup_by_name__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_autocmds__API_META(),
        nvim_get_autocmds__API_DISPATCH,
    )?;
    registry.register(
        nvim_exec_autocmds__API_META(),
        nvim_exec_autocmds__API_DISPATCH,
    )?;
    Ok(())
}
