//! Autocommand API over the editor's ordered firing planner.

use ox_editor::{
    AugroupId, AutocmdContext, AutocmdKind, AutocmdOptions, DeleteAutocmds, Editor, Event,
};

use crate::runtime::with_state_mut;
use crate::{api, ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError};

fn text(value: &OxStr, what: &str) -> Result<String, ApiError> {
    String::from_utf8(value.0.clone()).map_err(|_| ApiError::validation(format!("{what} must be valid UTF-8")))
}

fn strings(value: &Object, what: &str) -> Result<Vec<String>, ApiError> {
    match value {
        Object::String(value) => Ok(vec![text(value, what)?]),
        Object::Array(values) => values.iter().map(|value| match value {
            Object::String(value) => text(value, what),
            _ => Err(ApiError::validation(format!("{what} must be a String or Array"))),
        }).collect(),
        _ => Err(ApiError::validation(format!("{what} must be a String or Array"))),
    }
}

fn events(value: &Object) -> Result<Vec<Event>, ApiError> {
    strings(value, "event")?.into_iter().map(|name| {
        Event::from_name(&name).ok_or_else(|| ApiError::validation(format!("unexpected event: {name}")))
    }).collect()
}

fn group(editor: &Editor, value: Option<&Object>) -> Result<AugroupId, ApiError> {
    match value {
        None | Some(Object::Nil) => Ok(AugroupId::default()),
        Some(Object::Integer(id)) if *id >= 0 => {
            let id = AugroupId(u64::try_from(*id).map_err(|_| ApiError::validation("invalid group"))?);
            if editor.autocmds().has_group(id)
                { Ok(id) } else { Err(ApiError::validation(format!("Invalid 'group': {id:?}"))) }
        }
        Some(Object::String(name)) => editor.autocmds().group(&text(name, "group")?)
            .ok_or_else(|| ApiError::validation("Invalid 'group'")),
        _ => Err(ApiError::validation("group must be a String or Integer")),
    }
}

fn bool_opt(opts: &Dict, name: &str, default: bool) -> Result<bool, ApiError> {
    match opts.get(&OxStr::from(name)) {
        None => Ok(default),
        Some(Object::Boolean(value)) => Ok(*value),
        Some(_) => Err(ApiError::validation(format!("'{name}' must be a boolean"))),
    }
}

fn buffer(editor: &Editor, value: &Object) -> Result<BufHandle, ApiError> {
    let raw = match value { Object::Integer(value) => *value, Object::Buffer(value) => i64::from(*value), _ => return Err(ApiError::validation("buf must be an Integer")) };
    let handle = BufHandle::try_from(raw).map_err(|_| ApiError::validation("invalid buffer"))?;
    let handle = if handle.is_current() { editor.current_buffer().ok_or_else(|| ApiError::validation("No current buffer"))? } else { handle };
    editor.buffer(handle).map_err(|_| ApiError::validation(format!("Invalid buffer id: {raw}")))?;
    Ok(handle)
}

#[api(since = 9, fast)]
pub fn nvim_create_autocmd(editor: &mut Editor, event: Object, opts: Dict) -> Result<i64, ApiError> {
    let events = events(&event)?;
    let command = opts.get(&OxStr::from("command"));
    let callback = opts.get(&OxStr::from("callback"));
    let kind = match (command, callback) {
        (Some(Object::String(command)), None) => AutocmdKind::ExString(text(command, "command")?),
        (None, Some(Object::LuaRef(callback))) => AutocmdKind::LuaCallback(u64::try_from(*callback).map_err(|_| ApiError::validation("invalid callback"))?),
        (None, Some(Object::String(callback))) => AutocmdKind::ExString(text(callback, "callback")?),
        (Some(_), Some(_)) => return Err(ApiError::validation("Cannot use both 'callback' and 'command'")),
        _ => return Err(ApiError::validation("Required: 'command' or 'callback'")),
    };
    let group = group(editor, opts.get(&OxStr::from("group")))?;
    let once = bool_opt(&opts, "once", false)?;
    let nested = bool_opt(&opts, "nested", false)?;
    let description = opts.get(&OxStr::from("desc")).map(|value| match value {
        Object::String(value) => text(value, "desc"),
        _ => Err(ApiError::validation("desc must be a String")),
    }).transpose()?;
    let selected_buffer = opts.get(&OxStr::from("buffer")).or_else(|| opts.get(&OxStr::from("buf"))).map(|value| buffer(editor, value)).transpose()?;
    if selected_buffer.is_some() && opts.get(&OxStr::from("pattern")).is_some() {
        return Err(ApiError::validation("Cannot use both 'pattern' and 'buffer'"));
    }
    let patterns = match opts.get(&OxStr::from("pattern")) {
        Some(value) => strings(value, "pattern")?,
        None if selected_buffer.is_some() => vec!["<buffer>".to_owned()],
        None => vec!["*".to_owned()],
    };
    let pattern = patterns.join(",");
    let mut first = None;
    for event in events {
        let ids = editor.autocmds_mut().register(event, &pattern, kind.clone(), AutocmdOptions {
            group, buffer: selected_buffer, once, nested, description: description.clone(),
        }).map_err(|error| ApiError::validation(error.to_string()))?;
        first = first.or_else(|| ids.first().copied());
    }
    i64::try_from(first.ok_or_else(|| ApiError::validation("no autocmd was created"))?)
        .map_err(|_| ApiError::exception("autocmd id exceeds Integer range"))
}

#[api(since = 9)]
pub fn nvim_del_autocmd(editor: &mut Editor, id: i64) -> Result<(), ApiError> {
    if id <= 0 || !editor.autocmds_mut().delete_id(u64::try_from(id).unwrap_or(0)) {
        return Err(ApiError::validation(format!("Invalid 'id': {id}")));
    }
    Ok(())
}

fn filter_events(opts: &Dict) -> Result<Option<Vec<Event>>, ApiError> {
    opts.get(&OxStr::from("event")).map(events).transpose()
}

fn filter_patterns(opts: &Dict) -> Result<Option<Vec<String>>, ApiError> {
    opts.get(&OxStr::from("pattern")).map(|value| strings(value, "pattern")).transpose()
}

#[api(since = 9)]
pub fn nvim_clear_autocmds(editor: &mut Editor, opts: Dict) -> Result<(), ApiError> {
    let selected_group = group(editor, opts.get(&OxStr::from("group")))?;
    let groups = opts.get(&OxStr::from("group")).map(|_| selected_group);
    let selected_events = filter_events(&opts)?.unwrap_or_else(|| vec![Event::ALL[0]]);
    let all_events = opts.get(&OxStr::from("event")).is_none();
    let patterns = filter_patterns(&opts)?;
    if let Some(value) = opts.get(&OxStr::from("buffer")).or_else(|| opts.get(&OxStr::from("buf"))) {
        if patterns.is_some() { return Err(ApiError::validation("Cannot use both 'pattern' and 'buffer'")); }
        let handle = buffer(editor, value)?;
        let event_filter = if all_events { None } else { Some(selected_events.clone()) };
        let ids = editor.autocmds().definitions().into_iter().filter(|definition| {
            definition.buffer == Some(handle)
                && groups.is_none_or(|group| definition.group == group)
                && event_filter.as_ref().is_none_or(|events| events.contains(&definition.event))
        }).map(|definition| definition.id).collect::<Vec<_>>();
        for id in ids { editor.autocmds_mut().delete_id(id); }
        return Ok(());
    }
    if all_events {
        for event in Event::ALL {
            if let Some(patterns) = &patterns {
                for pattern in patterns {
                    editor.autocmds_mut().delete(DeleteAutocmds { group: groups, event: Some(*event), pattern: Some(pattern) })
                        .map_err(|error| ApiError::validation(error.to_string()))?;
                }
            } else {
                editor.autocmds_mut().delete(DeleteAutocmds { group: groups, event: Some(*event), pattern: None })
                    .map_err(|error| ApiError::validation(error.to_string()))?;
            }
        }
    } else {
        for event in selected_events {
            if let Some(patterns) = &patterns {
                for pattern in patterns {
                    editor.autocmds_mut().delete(DeleteAutocmds { group: groups, event: Some(event), pattern: Some(pattern) })
                        .map_err(|error| ApiError::validation(error.to_string()))?;
                }
            } else {
                editor.autocmds_mut().delete(DeleteAutocmds { group: groups, event: Some(event), pattern: None })
                    .map_err(|error| ApiError::validation(error.to_string()))?;
            }
        }
    }
    Ok(())
}

#[api(since = 9)]
pub fn nvim_create_augroup(editor: &mut Editor, name: OxStr, opts: Dict) -> Result<i64, ApiError> {
    let id = editor.autocmds_mut().create_group(&text(&name, "name")?, bool_opt(&opts, "clear", true)?)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    i64::try_from(id.0).map_err(|_| ApiError::exception("augroup id exceeds Integer range"))
}

#[api(since = 9)]
pub fn nvim_del_augroup_by_id(editor: &mut Editor, id: i64) -> Result<(), ApiError> {
    let id = u64::try_from(id).map(AugroupId).map_err(|_| ApiError::validation("invalid augroup id"))?;
    editor.autocmds_mut().delete_group(id).map_err(|error| ApiError::validation(error.to_string()))
}

#[api(since = 9)]
pub fn nvim_del_augroup_by_name(editor: &mut Editor, name: OxStr) -> Result<(), ApiError> {
    let name = text(&name, "name")?;
    let id = editor.autocmds().group(&name).ok_or_else(|| ApiError::validation(format!("Invalid augroup name: {name}")))?;
    editor.autocmds_mut().delete_group(id).map_err(|error| ApiError::validation(error.to_string()))
}

#[api(since = 9)]
pub fn nvim_get_autocmds(editor: &mut Editor, opts: Dict) -> Result<Vec<Dict>, ApiError> {
    let group_filter = opts.get(&OxStr::from("group")).map(|value| group(editor, Some(value))).transpose()?;
    let event_filter = filter_events(&opts)?;
    let pattern_filter = filter_patterns(&opts)?;
    let id_filter = match opts.get(&OxStr::from("id")) { Some(Object::Integer(id)) => Some(*id), Some(_) => return Err(ApiError::validation("id must be an Integer")), None => None };
    let buffer_filter = opts.get(&OxStr::from("buffer")).or_else(|| opts.get(&OxStr::from("buf"))).map(|value| buffer(editor, value)).transpose()?;
    Ok(editor.autocmds().definitions().into_iter().filter(|item| {
        group_filter.is_none_or(|group| item.group == group)
            && event_filter.as_ref().is_none_or(|events| events.contains(&item.event))
            && pattern_filter.as_ref().is_none_or(|patterns| patterns.contains(&item.pattern))
            && id_filter.is_none_or(|id| u64::try_from(id).ok() == Some(item.id))
            && buffer_filter.is_none_or(|buffer| item.buffer == Some(buffer))
    }).map(|item| {
        let pattern = item.pattern.clone();
        let description = item.description.clone().unwrap_or_default();
        let mut entries = vec![
            (OxStr::from("id"), Object::Integer(i64::try_from(item.id).unwrap_or(i64::MAX))),
            (OxStr::from("group"), Object::Integer(i64::try_from(item.group.0).unwrap_or(i64::MAX))),
            (OxStr::from("event"), Object::String(OxStr::from(item.event.as_str()))),
            (OxStr::from("pattern"), Object::String(OxStr::from(pattern.as_str()))),
            (OxStr::from("once"), Object::Boolean(item.once)),
            (OxStr::from("command"), match &item.kind { AutocmdKind::ExString(command) => Object::String(OxStr::from(command.as_str())), AutocmdKind::LuaCallback(_) => Object::String(OxStr::from("")) }),
            (OxStr::from("desc"), Object::String(OxStr::from(description.as_str()))),
        ];
        if let Some(name) = item.group_name { entries.push((OxStr::from("group_name"), Object::String(OxStr::from(name.as_str())))); }
        if let Some(buffer) = item.buffer { entries.push((OxStr::from("buf"), Object::Integer(i64::from(buffer)))); entries.push((OxStr::from("buflocal"), Object::Boolean(true))); }
        if let AutocmdKind::LuaCallback(callback) = item.kind { entries.push((OxStr::from("callback"), Object::LuaRef(i32::try_from(callback).unwrap_or(i32::MAX)))); }
        Dict(entries)
    }).collect())
}

#[api(since = 9)]
pub fn nvim_exec_autocmds(editor: &mut Editor, event: Object, opts: Dict) -> Result<(), ApiError> {
    let selected_events = events(&event)?;
    let selected_buffer = opts.get(&OxStr::from("buffer")).or_else(|| opts.get(&OxStr::from("buf"))).map(|value| buffer(editor, value)).transpose()?;
    let pattern = opts.get(&OxStr::from("pattern")).map(|value| strings(value, "pattern")).transpose()?.and_then(|values| values.into_iter().next());
    for event in selected_events {
        let plan = editor.autocmds_mut().plan(event, AutocmdContext { buffer: selected_buffer, file_name: pattern.as_deref(), nested: true });
        for action in plan.ready {
            with_state_mut(editor, |state| state.autocmd_executor.as_mut().map(|executor| executor.execute(&action)))
                .transpose().map_err(ApiError::exception)?;
            if action.once { editor.autocmds_mut().consume_once(action.id); }
        }
    }
    Ok(())
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_create_autocmd__API_META(), nvim_create_autocmd__API_DISPATCH)?;
    registry.register(nvim_del_autocmd__API_META(), nvim_del_autocmd__API_DISPATCH)?;
    registry.register(nvim_clear_autocmds__API_META(), nvim_clear_autocmds__API_DISPATCH)?;
    registry.register(nvim_create_augroup__API_META(), nvim_create_augroup__API_DISPATCH)?;
    registry.register(nvim_del_augroup_by_id__API_META(), nvim_del_augroup_by_id__API_DISPATCH)?;
    registry.register(nvim_del_augroup_by_name__API_META(), nvim_del_augroup_by_name__API_DISPATCH)?;
    registry.register(nvim_get_autocmds__API_META(), nvim_get_autocmds__API_DISPATCH)?;
    registry.register(nvim_exec_autocmds__API_META(), nvim_exec_autocmds__API_DISPATCH)?;
    Ok(())
}
