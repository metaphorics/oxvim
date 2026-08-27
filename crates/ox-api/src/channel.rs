//! RPC channel metadata, subscriptions, and runtime-file lookup.

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
    let output = with_state_mut(editor, |state| {
        if state.channels.contains_key(&channel) {
            let sink = state.channel_sink.as_mut()
                .ok_or_else(|| ApiError::exception("channel has no writable sink"))?;
            sink.send(channel, data.as_bytes()).map_err(ApiError::exception)?;
            return sink.take_pty_output(channel).map_err(ApiError::exception);
        }
        if editor.terminal_channel(channel).is_some() {
            let sink = state.job_sink.as_mut()
                .ok_or_else(|| ApiError::exception("channel has no writable sink"))?;
            sink.send(channel, data.as_bytes()).map_err(ApiError::exception)?;
            return sink.take_pty_output(channel).map_err(ApiError::exception);
        }
        Err(ApiError::validation(format!("Invalid channel: {chan}")))
    })?;
    editor.append_terminal_buffer(channel, &output).map_err(|error| ApiError::exception(error.to_string()))
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
    Ok(with_state(editor, |state| {
        if let Some(info) = state.channels.get(&channel) {
            return channel_dict(info);
        }
        if let Some(term) = editor.terminal_channel(channel) {
            let mut values = vec![
                (OxStr::from("id"), Object::Integer(i64::try_from(channel).unwrap_or(i64::MAX))),
                (OxStr::from("stream"), Object::String(OxStr::from("job"))),
                (OxStr::from("mode"), Object::String(OxStr::from("terminal"))),
            ];
            if let Some(pty) = &term.pty { values.push((OxStr::from("pty"), Object::String(OxStr::from(pty.as_str())))); }
            values.push((OxStr::from("buffer"), Object::Integer(i64::from(term.buffer))));
            return Dict(values);
        }
        Dict(Vec::new())
    }))
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
    // api/vim.c: nvim_list_runtime_paths() is nvim_get_runtime_file("", true).
    Ok(runtime_file_strings(editor, "", true))
}

#[api(since = 7)]
pub fn nvim_get_runtime_file(editor: &mut Editor, name: OxStr, all: bool) -> Result<Vec<OxStr>, ApiError> {
    let pattern = utf8(&name, "name")?;
    Ok(runtime_file_strings(editor, &pattern, all))
}

fn runtime_file_strings(editor: &Editor, name: &str, all: bool) -> Vec<OxStr> {
    crate::runtime::find_runtime_files(editor, name, all)
        .iter()
        .map(|path| OxStr::from(path.to_string_lossy().as_bytes()))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use ox_editor::Editor;

    #[test]
    fn nvim_get_chan_info_reports_job_stream_for_terminal_channel() {
        let mut editor = Editor::new();
        editor.allocate_terminal_buffer(7).unwrap();
        let info = nvim_get_chan_info(&mut editor, 7).unwrap();
        let Object::String(stream) = info.0.iter().find(|(k, _)| k.to_string_lossy() == "stream").map(|(_, v)| v).unwrap() else {
            panic!("stream must be a String");
        };
        assert_eq!(stream.to_string_lossy(), "job");
        let Object::String(mode) = info.0.iter().find(|(k, _)| k.to_string_lossy() == "mode").map(|(_, v)| v).unwrap() else {
            panic!("mode must be a String");
        };
        assert_eq!(mode.to_string_lossy(), "terminal");
    }
}
