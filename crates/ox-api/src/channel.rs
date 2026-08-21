//! RPC channel metadata, subscriptions, and runtime-file lookup.

use std::collections::BTreeSet;

use ox_editor::Editor;

use crate::runtime::{ChannelInfo, with_state, with_state_mut};
use crate::{api, ApiError, Dict, Object, OxStr, Registry, RegistryError};

const CHANNEL_ID: u64 = 1;

fn channel_dict(info: &ChannelInfo) -> Dict {
    let mut values = vec![
        (OxStr::from("id"), Object::Integer(i64::try_from(info.id).unwrap_or(i64::MAX))),
        (OxStr::from("stream"), Object::String(info.stream.clone())),
        (OxStr::from("mode"), Object::String(info.mode.clone())),
    ];
    if let Some(pty) = &info.pty { values.push((OxStr::from("pty"), Object::String(pty.clone()))); }
    if let Some(buffer) = info.buffer { values.push((OxStr::from("buffer"), Object::Integer(buffer))); }
    if !info.client.0.is_empty() { values.push((OxStr::from("client"), Object::Dict(info.client.clone()))); }
    Dict(values)
}

#[api(since = 4)]
pub fn nvim_chan_send(editor: &mut Editor, chan: i64, data: OxStr) -> Result<(), ApiError> {
    let channel = u64::try_from(chan).map_err(|_| ApiError::validation("Invalid channel id"))?;
    with_state_mut(editor, |state| {
        if !state.channels.contains_key(&channel) { return Err(ApiError::validation(format!("Invalid channel: {chan}"))); }
        state.channel_sink.as_mut().ok_or_else(|| ApiError::exception("channel has no writable sink"))?
            .send(channel, data.as_bytes()).map_err(ApiError::exception)
    })
}

#[api(since = 1, deprecated_since = 13)]
pub fn nvim_subscribe(editor: &mut Editor, event: OxStr) -> Result<(), ApiError> {
    with_state_mut(editor, |state| { state.subscriptions.entry(CHANNEL_ID).or_default().insert(event); });
    Ok(())
}

#[api(since = 1, deprecated_since = 13)]
pub fn nvim_unsubscribe(editor: &mut Editor, event: OxStr) -> Result<(), ApiError> {
    with_state_mut(editor, |state| {
        if let Some(events) = state.subscriptions.get_mut(&CHANNEL_ID) { events.remove(&event); }
    });
    Ok(())
}

#[api(since = 4)]
pub fn nvim_list_chans(editor: &mut Editor) -> Result<Vec<Dict>, ApiError> {
    Ok(with_state(editor, |state| state.channels.values().map(channel_dict).collect()))
}

#[api(since = 4)]
pub fn nvim_get_chan_info(editor: &mut Editor, chan: i64) -> Result<Dict, ApiError> {
    let channel = u64::try_from(chan).map_err(|_| ApiError::validation("Invalid channel id"))?;
    Ok(with_state(editor, |state| state.channels.get(&channel).map(channel_dict).unwrap_or_else(|| Dict(Vec::new()))))
}

fn utf8(value: &OxStr, field: &str) -> Result<String, ApiError> {
    String::from_utf8(value.0.clone()).map_err(|_| ApiError::validation(format!("{field} must be valid UTF-8")))
}

#[api(since = 4)]
pub fn nvim_set_client_info(editor: &mut Editor, name: OxStr, version: Dict, client_type: OxStr, methods: Dict, attributes: Dict) -> Result<(), ApiError> {
    if name.0.is_empty() { return Err(ApiError::validation("client name must not be empty")); }
    let client_type_text = utf8(&client_type, "type")?;
    if !["remote", "ui", "embedder", "host", "plugin", "msgpack-rpc"].contains(&client_type_text.as_str()) {
        return Err(ApiError::validation(format!("Invalid client type: {client_type_text}")));
    }
    for (method, value) in &methods.0 {
        let Object::Dict(spec) = value else { return Err(ApiError::validation(format!("method {} must be a Dictionary", method.to_string_lossy()))); };
        if let Some(value) = spec.get(&OxStr::from("async"))
            && !matches!(value, Object::Boolean(_))
        { return Err(ApiError::validation("method async must be a boolean")); }
    }
    let client = Dict(vec![
        (OxStr::from("name"), Object::String(name)),
        (OxStr::from("version"), Object::Dict(version)),
        (OxStr::from("type"), Object::String(client_type)),
        (OxStr::from("methods"), Object::Dict(methods)),
        (OxStr::from("attributes"), Object::Dict(attributes)),
    ]);
    with_state_mut(editor, |state| {
        state.channels.entry(CHANNEL_ID).or_insert_with(|| ChannelInfo {
            id: CHANNEL_ID, stream: OxStr::from("stdio"), mode: OxStr::from("rpc"),
            pty: None, buffer: None, client: Dict(Vec::new()),
        }).client = client;
    });
    Ok(())
}

#[api(since = 1)]
pub fn nvim_list_runtime_paths(editor: &mut Editor) -> Result<Vec<OxStr>, ApiError> {
    Ok(with_state(editor, |state| state.runtime_paths.iter().map(|path| OxStr::from(path.to_string_lossy().as_bytes())).collect()))
}

#[api(since = 7)]
pub fn nvim_get_runtime_file(editor: &mut Editor, name: OxStr, all: bool) -> Result<Vec<OxStr>, ApiError> {
    let pattern = utf8(&name, "name")?;
    with_state(editor, |state| {
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();
        for root in &state.runtime_paths {
            for path in state.file_io.glob(root, &pattern).map_err(ApiError::exception)? {
                let string = OxStr::from(path.to_string_lossy().as_bytes());
                if seen.insert(string.clone()) { found.push(string); }
                if !all && !found.is_empty() { return Ok(found); }
            }
        }
        Ok(found)
    })
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_chan_send__API_META(), nvim_chan_send__API_DISPATCH)?;
    registry.register(nvim_subscribe__API_META(), nvim_subscribe__API_DISPATCH)?;
    registry.register(nvim_unsubscribe__API_META(), nvim_unsubscribe__API_DISPATCH)?;
    registry.register(nvim_list_chans__API_META(), nvim_list_chans__API_DISPATCH)?;
    registry.register(nvim_get_chan_info__API_META(), nvim_get_chan_info__API_DISPATCH)?;
    registry.register(nvim_set_client_info__API_META(), nvim_set_client_info__API_DISPATCH)?;
    registry.register(nvim_list_runtime_paths__API_META(), nvim_list_runtime_paths__API_DISPATCH)?;
    registry.register(nvim_get_runtime_file__API_META(), nvim_get_runtime_file__API_DISPATCH)?;
    Ok(())
}
