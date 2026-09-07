//! RPC channel metadata, subscriptions, and runtime-file lookup.

use crate::runtime::ChannelInfo;
use crate::session::ApiSession;
use ox_rpc::ChannelId;

use crate::{ApiError, Dict, Object, OxStr, Registry, RegistryError, api};

fn channel_dict(info: &ChannelInfo) -> Result<Dict, ApiError> {
    let id = i64::try_from(info.id)
        .map_err(|_| ApiError::exception("channel id exceeds API Integer range"))?;
    let mut values = vec![
        (OxStr::from("id"), Object::Integer(id)),
        (OxStr::from("stream"), Object::String(info.stream.clone())),
        (OxStr::from("mode"), Object::String(info.mode.clone())),
    ];
    if let Some(pty) = &info.pty {
        values.push((OxStr::from("pty"), Object::String(pty.clone())));
    }
    if let Some(buffer) = info.buffer {
        values.push((OxStr::from("buffer"), Object::Integer(buffer)));
    }
    if info.mode == OxStr::from("rpc") {
        values.push((OxStr::from("client"), Object::Dict(info.client.clone())));
    }
    Ok(Dict(values))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes the payload as an owned String"
)]
#[api(since = 4)]
pub fn nvim_chan_send(session: &ApiSession, chan: i64, data: OxStr) -> Result<(), ApiError> {
    let channel = u64::try_from(chan).map_err(|_| ApiError::validation("Invalid channel id"))?;
    let terminal_input = session.with_state_mut(|state| {
        let Some(terminal) = state.terminal_inputs.get_mut(&channel) else {
            return false;
        };
        if data
            .as_bytes()
            .windows(8)
            .any(|bytes| bytes == b"\x1b[?2004h")
        {
            terminal.bracketed_paste = true;
        }
        if data
            .as_bytes()
            .windows(8)
            .any(|bytes| bytes == b"\x1b[?2004l")
        {
            terminal.bracketed_paste = false;
        }
        true
    });
    let output = if terminal_input {
        Vec::new()
    } else {
        let rpc_channel = session.with_state(|state| state.channels.contains_key(&channel));
        let job_channel = !rpc_channel
            && session.with_editor(|editor| editor.terminal_channel(channel).is_some());
        if !rpc_channel && !job_channel {
            return Err(ApiError::validation(format!("Invalid channel: {chan}")));
        }
        // The sink is host code: take it out so no state borrow spans `send`.
        let mut sink = session.with_state_mut(|state| {
            if rpc_channel {
                state.channel_sink.take()
            } else {
                state.job_sink.take()
            }
        });
        let sent = sink
            .as_mut()
            .ok_or_else(|| ApiError::exception("channel has no writable sink"))
            .and_then(|sink| {
                sink.send(channel, data.as_bytes())
                    .map_err(ApiError::exception)?;
                sink.take_pty_output(channel).map_err(ApiError::exception)
            });
        session.with_state_mut(|state| {
            let slot = if rpc_channel {
                &mut state.channel_sink
            } else {
                &mut state.job_sink
            };
            *slot = sink;
        });
        sent?
    };
    session
        .with_editor_mut(|editor| editor.append_terminal_buffer(channel, &output))
        .map_err(|error| ApiError::exception(error.to_string()))
}

#[api(since = 1, deprecated_since = 13)]
pub fn nvim_subscribe(session: &ApiSession, event: OxStr) -> Result<(), ApiError> {
    let channel = session
        .requesting_channel()
        .ok_or_else(|| ApiError::validation("E515: nvim_subscribe requires an RPC request"))?;
    session.with_state_mut(|state| {
        state
            .subscriptions
            .entry(channel.get())
            .or_default()
            .insert(event);
    });
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes the event name as an owned String"
)]
#[api(since = 1, deprecated_since = 13)]
pub fn nvim_unsubscribe(session: &ApiSession, event: OxStr) -> Result<(), ApiError> {
    let channel = session
        .requesting_channel()
        .ok_or_else(|| ApiError::validation("E516: nvim_unsubscribe requires an RPC request"))?;
    session.with_state_mut(|state| {
        if let Some(events) = state.subscriptions.get_mut(&channel.get()) {
            events.remove(&event);
        }
    });
    Ok(())
}

#[api(since = 4)]
pub fn nvim_list_chans(session: &ApiSession) -> Result<Vec<Dict>, ApiError> {
    let mut channels = session.with_state(|state| {
        state
            .channels
            .values()
            .map(channel_dict)
            .collect::<Result<Vec<_>, _>>()
    })?;
    let terminal_channels = session.with_editor(|editor| {
        editor
            .terminal_channel_ids()
            .map(|id| terminal_channel_dict(editor, id))
            .collect::<Result<Vec<_>, _>>()
    })?;
    channels.extend(terminal_channels.into_iter().flatten());
    channels.sort_by_key(
        |dict| match dict.0.iter().find(|(key, _)| *key == OxStr::from("id")) {
            Some((_, Object::Integer(id))) => *id,
            _ => i64::MAX,
        },
    );
    channels.dedup_by_key(
        |dict| match dict.0.iter().find(|(key, _)| *key == OxStr::from("id")) {
            Some((_, Object::Integer(id))) => *id,
            _ => i64::MAX,
        },
    );
    Ok(channels)
}

#[api(since = 4)]
pub fn nvim_get_chan_info(session: &ApiSession, chan: i64) -> Result<Dict, ApiError> {
    let channel = if chan == 0 {
        match session.requesting_channel() {
            Some(channel) => channel,
            None => return Ok(Dict(Vec::new())),
        }
    } else {
        ChannelId::new(u64::try_from(chan).map_err(|_| ApiError::validation("Invalid channel id"))?)
    };
    let key = channel.get();
    let registered =
        session.with_state(|state| state.channels.get(&key).map(channel_dict).transpose())?;
    if let Some(info) = registered {
        return Ok(info);
    }
    Ok(session
        .with_editor(|editor| terminal_channel_dict(editor, key))?
        .unwrap_or_else(|| Dict(Vec::new())))
}

fn terminal_channel_dict(editor: &ox_editor::Editor, key: u64) -> Result<Option<Dict>, ApiError> {
    let Some(term) = editor.terminal_channel(key) else {
        return Ok(None);
    };
    let id = i64::try_from(key)
        .map_err(|_| ApiError::exception("channel id exceeds API Integer range"))?;
    let mut values = vec![
        (OxStr::from("id"), Object::Integer(id)),
        (OxStr::from("stream"), Object::String(OxStr::from("job"))),
        (OxStr::from("mode"), Object::String(OxStr::from("terminal"))),
    ];
    if let Some(pty) = &term.pty {
        values.push((
            OxStr::from("pty"),
            Object::String(OxStr::from(pty.as_str())),
        ));
    }
    values.push((
        OxStr::from("buffer"),
        Object::Integer(i64::from(term.buffer)),
    ));
    Ok(Some(Dict(values)))
}

fn utf8(value: &OxStr, field: &str) -> Result<String, ApiError> {
    String::from_utf8(value.0.clone())
        .map_err(|_| ApiError::validation(format!("{field} must be valid UTF-8")))
}

#[api(since = 4)]
pub fn nvim_set_client_info(
    session: &ApiSession,
    name: OxStr,
    version: Dict,
    client_type: OxStr,
    methods: Dict,
    attributes: Dict,
) -> Result<(), ApiError> {
    if name.0.is_empty() {
        return Err(ApiError::validation("client name must not be empty"));
    }
    let client_type_text = utf8(&client_type, "type")?;
    if !["remote", "ui", "embedder", "host", "plugin", "msgpack-rpc"]
        .contains(&client_type_text.as_str())
    {
        return Err(ApiError::validation(format!(
            "Invalid client type: {client_type_text}"
        )));
    }
    for (method, value) in &methods.0 {
        let Object::Dict(spec) = value else {
            return Err(ApiError::validation(format!(
                "method {} must be a Dictionary",
                method.to_string_lossy()
            )));
        };
        if let Some(value) = spec.get(&OxStr::from("async"))
            && !matches!(value, Object::Boolean(_))
        {
            return Err(ApiError::validation("method async must be a boolean"));
        }
    }
    let client = Dict(vec![
        (OxStr::from("name"), Object::String(name)),
        (OxStr::from("version"), Object::Dict(version)),
        (OxStr::from("type"), Object::String(client_type)),
        (OxStr::from("methods"), Object::Dict(methods)),
        (OxStr::from("attributes"), Object::Dict(attributes)),
    ]);
    let channel = session
        .requesting_channel()
        .ok_or_else(|| ApiError::validation("nvim_set_client_info requires an RPC request"))?;
    crate::runtime::update_channel_client(session, channel, client)
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires handlers to return typed API errors"
)]
#[api(since = 1)]
pub fn nvim_list_runtime_paths(session: &ApiSession) -> Result<Vec<OxStr>, ApiError> {
    // api/vim.c: nvim_list_runtime_paths() is nvim_get_runtime_file("", true).
    Ok(runtime_file_strings(session, "", true))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes the pattern as an owned String"
)]
#[api(since = 7)]
pub fn nvim_get_runtime_file(
    session: &ApiSession,
    name: OxStr,
    all: bool,
) -> Result<Vec<OxStr>, ApiError> {
    let pattern = utf8(&name, "name")?;
    Ok(runtime_file_strings(session, &pattern, all))
}

fn runtime_file_strings(session: &ApiSession, name: &str, all: bool) -> Vec<OxStr> {
    crate::runtime::find_runtime_files(session, name, all)
        .iter()
        .map(|path| OxStr::from(path.to_string_lossy().as_bytes()))
        .collect()
}

/// Reads a field of `/proc/<pid>/stat` split safely past the comm field,
/// which may itself contain spaces and parentheses.
fn proc_stat_field(pid: i64, field: usize) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm is parenthesized and may contain ' ' and ')'; the fields after
    // it start at the LAST ')'.
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace()
        .nth(field.saturating_sub(3))
        .map(str::to_owned)
}

/// Gets info describing process `pid` (upstream `nvim_get_proc`,
/// api/vim.c:1983-2015: NIL when not found, `{name, pid}` from the OS).
#[api(since = 4)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` requires a `Result` return; upstream has no error path"
)]
pub fn nvim_get_proc(_session: &ApiSession, pid: i64) -> Result<Object, ApiError> {
    // Upstream's VALIDATE_INT block returns NIL, not an error
    // (api/vim.c:1988-1990).
    if !(pid > 0 && pid <= i32::MAX.into()) {
        return Ok(Object::Nil);
    }
    let Some(name) = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|raw| raw.trim_end().to_owned())
    else {
        return Ok(Object::Nil);
    };
    // Upstream's helper reports {name, pid, ppid}
    // (runtime/lua/vim/_core/editor.lua:215-219); ppid is stat field 4.
    let parent = proc_stat_field(pid, 4)
        .and_then(|field| field.trim().parse::<i64>().ok())
        .unwrap_or(0);
    Ok(Object::Dict(Dict(vec![
        (
            OxStr::from("name"),
            Object::String(OxStr::from(name.as_bytes())),
        ),
        (OxStr::from("pid"), Object::Integer(pid)),
        (OxStr::from("ppid"), Object::Integer(parent)),
    ])))
}

/// Gets the child process ids of `pid` (upstream `nvim_get_proc_children`,
/// api/vim.c:1943-1977: syscall children listing, empty when none).
#[api(since = 4)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "`#[api]` requires a `Result` return; upstream has no error path"
)]
pub fn nvim_get_proc_children(_session: &ApiSession, pid: i64) -> Result<Vec<Object>, ApiError> {
    // Upstream's VALIDATE_INT block jumps to the empty-array end, not an
    // error (api/vim.c:1949-1952).
    if !(pid > 0 && pid <= i32::MAX.into()) {
        return Ok(Vec::new());
    }
    // The kernel's /proc children listing needs CONFIG_PROC_CHILDREN; the
    // portable route upstream falls back to (a `ps` walk) is a PPID scan.
    let mut children = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Ok(children);
    };
    for entry in entries.flatten() {
        let Ok(candidate) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(candidate_pid) = candidate.parse::<i64>() else {
            continue;
        };
        // stat field 4 is PPID (1-based fields after the comm parenthesis).
        if proc_stat_field(candidate_pid, 4).is_some_and(|ppid| ppid == pid.to_string()) {
            children.push(Object::Integer(candidate_pid));
        }
    }
    Ok(children)
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_chan_send__API_META(), nvim_chan_send__API_DISPATCH)?;
    registry.register(nvim_subscribe__API_META(), nvim_subscribe__API_DISPATCH)?;
    registry.register(nvim_unsubscribe__API_META(), nvim_unsubscribe__API_DISPATCH)?;
    registry.register(nvim_list_chans__API_META(), nvim_list_chans__API_DISPATCH)?;
    registry.register(
        nvim_get_chan_info__API_META(),
        nvim_get_chan_info__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_client_info__API_META(),
        nvim_set_client_info__API_DISPATCH,
    )?;
    registry.register(
        nvim_list_runtime_paths__API_META(),
        nvim_list_runtime_paths__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_runtime_file__API_META(),
        nvim_get_runtime_file__API_DISPATCH,
    )?;
    registry.register(nvim_get_proc__API_META(), nvim_get_proc__API_DISPATCH)?;
    registry.register(
        nvim_get_proc_children__API_META(),
        nvim_get_proc_children__API_DISPATCH,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use ox_editor::Editor;
    use ox_rpc::CHAN_STDIO;

    fn session_with(editor: Editor) -> ApiSession {
        ApiSession::new(Rc::new(RefCell::new(editor)))
    }

    #[expect(
        clippy::unwrap_used,
        clippy::panic,
        reason = "test asserts decoded channel state"
    )]
    #[test]
    fn nvim_get_chan_info_reports_job_stream_for_terminal_channel() {
        let mut editor = Editor::new();
        editor.allocate_terminal_buffer(7).unwrap();
        let session = session_with(editor);
        let info = nvim_get_chan_info(&session, 7).unwrap();
        let Object::String(stream) = info
            .0
            .iter()
            .find(|(k, _)| k.to_string_lossy() == "stream")
            .map(|(_, v)| v)
            .unwrap()
        else {
            panic!("stream must be a String");
        };
        assert_eq!(stream.to_string_lossy(), "job");
        let Object::String(mode) = info
            .0
            .iter()
            .find(|(k, _)| k.to_string_lossy() == "mode")
            .map(|(_, v)| v)
            .unwrap()
        else {
            panic!("mode must be a String");
        };
        assert_eq!(mode.to_string_lossy(), "terminal");
    }

    #[expect(clippy::unwrap_used, reason = "test asserts decoded channel state")]
    #[test]
    fn nvim_list_chans_includes_terminal_channel_metadata() {
        let mut editor = Editor::new();
        editor.allocate_terminal_buffer(7).unwrap();
        let session = session_with(editor);
        let listed = nvim_list_chans(&session)
            .unwrap()
            .into_iter()
            .find(|dict| dict_field(dict, "id") == Object::Integer(7))
            .unwrap();
        assert_eq!(listed, nvim_get_chan_info(&session, 7).unwrap());
    }

    #[expect(clippy::unwrap_used, reason = "test asserts decoded channel state")]
    #[test]
    fn seeded_channels_expose_stdio_and_stderr_metadata() {
        let session = session_with(Editor::new());
        let chans = nvim_list_chans(&session).unwrap();
        let stdio = &chans[0];
        let stderr = &chans[1];
        assert_eq!(dict_field(stdio, "id"), Object::Integer(1));
        assert_eq!(
            dict_field(stdio, "stream"),
            Object::String(OxStr::from("stdio"))
        );
        assert_eq!(
            dict_field(stdio, "mode"),
            Object::String(OxStr::from("rpc"))
        );
        assert_eq!(dict_field(stdio, "client"), Object::Dict(Dict(Vec::new())));
        assert_eq!(dict_field(stderr, "id"), Object::Integer(2));
        assert_eq!(
            dict_field(stderr, "stream"),
            Object::String(OxStr::from("stderr"))
        );
        assert_eq!(
            dict_field(stderr, "mode"),
            Object::String(OxStr::from("bytes"))
        );
        assert!(
            stderr
                .0
                .iter()
                .all(|(key, _)| key.to_string_lossy() != "client")
        );
        assert_eq!(nvim_get_chan_info(&session, 1).unwrap(), *stdio);
        assert_eq!(
            nvim_get_chan_info(&session, i64::try_from(CHAN_STDIO.get()).unwrap()).unwrap(),
            *stdio
        );
    }

    #[expect(
        clippy::unwrap_used,
        clippy::panic,
        reason = "test exercises guard drop across a deliberate unwind"
    )]
    #[test]
    fn chan_zero_resolves_to_caller_or_empty_dict() {
        let session = session_with(Editor::new());
        assert_eq!(nvim_get_chan_info(&session, 0).unwrap(), Dict(Vec::new()));
        let id = session.with_editor_mut(|editor| editor.allocate_channel_id());
        session.with_state_mut(|state| {
            state
                .channels
                .insert(id, ChannelInfo::socket_rpc(ChannelId::new(id)));
        });
        let outer = session.enter_rpc_call(ChannelId::new(id));
        assert_eq!(
            dict_field(&nvim_get_chan_info(&session, 0).unwrap(), "id"),
            Object::Integer(i64::try_from(id).unwrap())
        );
        let inner = session.enter_internal_call();
        assert_eq!(nvim_get_chan_info(&session, 0).unwrap(), Dict(Vec::new()));
        drop(inner);
        assert_eq!(
            dict_field(&nvim_get_chan_info(&session, 0).unwrap(), "id"),
            Object::Integer(i64::try_from(id).unwrap())
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _panicked = session.enter_internal_call();
            panic!("abort dispatch");
        }));
        assert!(result.is_err());
        drop(outer);
        assert_eq!(nvim_get_chan_info(&session, 0).unwrap(), Dict(Vec::new()));
    }

    #[expect(clippy::unwrap_used, reason = "test asserts decoded channel state")]
    #[test]
    fn out_of_order_guard_drop_preserves_attribution() {
        let session = session_with(Editor::new());
        let id_a = session.with_editor_mut(|editor| editor.allocate_channel_id());
        let id_b = session.with_editor_mut(|editor| editor.allocate_channel_id());
        session.with_state_mut(|state| {
            state
                .channels
                .insert(id_a, ChannelInfo::socket_rpc(ChannelId::new(id_a)));
            state
                .channels
                .insert(id_b, ChannelInfo::socket_rpc(ChannelId::new(id_b)));
        });
        let outer = session.enter_rpc_call(ChannelId::new(id_a));
        let inner = session.enter_rpc_call(ChannelId::new(id_b));
        assert_eq!(
            dict_field(&nvim_get_chan_info(&session, 0).unwrap(), "id"),
            Object::Integer(i64::try_from(id_b).unwrap())
        );
        drop(outer);
        assert_eq!(
            dict_field(&nvim_get_chan_info(&session, 0).unwrap(), "id"),
            Object::Integer(i64::try_from(id_b).unwrap())
        );
        drop(inner);
        assert_eq!(nvim_get_chan_info(&session, 0).unwrap(), Dict(Vec::new()));
    }

    #[expect(
        clippy::unwrap_used,
        clippy::panic,
        reason = "test asserts decoded channel state"
    )]
    #[test]
    fn caller_owned_mutations_skip_stdio_channel() {
        let session = session_with(Editor::new());
        let id = session.with_editor_mut(|editor| editor.allocate_channel_id());
        session.with_state_mut(|state| {
            state
                .channels
                .insert(id, ChannelInfo::socket_rpc(ChannelId::new(id)));
        });
        assert!(
            nvim_set_client_info(
                &session,
                OxStr::from("peer"),
                dict(&[]),
                OxStr::from("remote"),
                dict(&[]),
                dict(&[])
            )
            .is_err()
        );
        assert!(nvim_subscribe(&session, OxStr::from("event")).is_err());
        let _scope = session.enter_rpc_call(ChannelId::new(id));
        nvim_set_client_info(
            &session,
            OxStr::from("peer"),
            dict(&[("major", Object::Integer(1))]),
            OxStr::from("remote"),
            dict(&[]),
            dict(&[]),
        )
        .unwrap();
        nvim_subscribe(&session, OxStr::from("key")).unwrap();
        let peer_info = nvim_get_chan_info(&session, 0).unwrap();
        let Object::Dict(client) = dict_field(&peer_info, "client") else {
            panic!("peer client must be a Dict")
        };
        assert_eq!(
            client.get(&OxStr::from("name")),
            Some(&Object::String(OxStr::from("peer")))
        );
        assert_eq!(
            dict_field(&nvim_get_chan_info(&session, 1).unwrap(), "client"),
            Object::Dict(Dict(Vec::new()))
        );
        assert!(session.with_state(|state| { !state.subscriptions.contains_key(&1) }));
        assert_eq!(
            session.with_state(|state| state
                .subscriptions
                .get(&id)
                .map(std::collections::BTreeSet::len)),
            Some(1)
        );
        nvim_unsubscribe(&session, OxStr::from("key")).unwrap();
        assert!(
            session
                .with_state(|state| {
                    state
                        .subscriptions
                        .get(&id)
                        .map(std::collections::BTreeSet::is_empty)
                })
                .unwrap()
        );
    }

    #[expect(
        clippy::unwrap_used,
        reason = "assertion primitive: panics name the missing field"
    )]
    fn dict_field(info: &Dict, field: &str) -> Object {
        info.0
            .iter()
            .find(|(key, _)| key.to_string_lossy() == field)
            .map(|(_, value)| value.clone())
            .unwrap()
    }
    fn dict(entries: &[(&str, Object)]) -> Dict {
        Dict(
            entries
                .iter()
                .map(|(key, value)| (OxStr::from(*key), value.clone()))
                .collect(),
        )
    }
}
