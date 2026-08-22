//! UI attachment, highlight, input, paste, and terminal APIs.

#![allow(non_snake_case)]
use ox_editor::{Editor, Keys, RegisterContent, Remap, TypeaheadFlags};
use ox_text::Position;
use ox_ui::{Highlight, HlAttrs, UiOptions};

use crate::runtime::{ChannelInfo, RuntimeState, with_state, with_state_mut};
use crate::{api, ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError};

const CHANNEL_ID: u64 = 1;

fn dimension(value: i64, name: &str) -> Result<usize, ApiError> {
    usize::try_from(value).ok().filter(|value| *value > 0).ok_or_else(|| ApiError::validation(format!("{name} must be positive")))
}

fn ui_dict(id: u64, channel: &ox_ui::UiChannel) -> Dict {
    let (width, height) = channel.size();
    let opts = channel.options();
    Dict(vec![
        (OxStr::from("chan"), Object::Integer(i64::try_from(id).unwrap_or(i64::MAX))),
        (OxStr::from("width"), Object::Integer(i64::try_from(width).unwrap_or(i64::MAX))),
        (OxStr::from("height"), Object::Integer(i64::try_from(height).unwrap_or(i64::MAX))),
        (OxStr::from("rgb"), Object::Boolean(true)),
        (OxStr::from("ext_linegrid"), Object::Boolean(opts.ext_linegrid)),
        (OxStr::from("ext_multigrid"), Object::Boolean(opts.ext_multigrid)),
        (OxStr::from("ext_messages"), Object::Boolean(opts.ext_messages)),
        (OxStr::from("ext_cmdline"), Object::Boolean(opts.ext_cmdline)),
        (OxStr::from("ext_popupmenu"), Object::Boolean(opts.ext_popupmenu)),
        (OxStr::from("ext_hlstate"), Object::Boolean(opts.ext_hlstate)),
        (OxStr::from("ext_termcolors"), Object::Boolean(opts.ext_termcolors)),
    ])
}

#[api(since = 4)]
pub fn nvim_list_uis(editor: &mut Editor) -> Result<Vec<Dict>, ApiError> {
    Ok(with_state(editor, |state| state.ui_channels.iter().map(|(id, channel)| ui_dict(*id, channel)).collect()))
}

#[api(since = 1)]
pub fn nvim_ui_attach(editor: &mut Editor, width: i64, height: i64, options: Dict) -> Result<(), ApiError> {
    with_state_mut(editor, |state| state.ui_channels.attach(CHANNEL_ID, dimension(width, "width")?, dimension(height, "height")?, UiOptions::from_dict(&options)).map_err(|error| ApiError::exception(error.to_string())))
}

#[api(since = 1)]
pub fn nvim_ui_detach(editor: &mut Editor) -> Result<(), ApiError> {
    with_state_mut(editor, |state| state.ui_channels.detach(CHANNEL_ID).map(|_| ()).map_err(|error| ApiError::exception(error.to_string())))
}

#[api(since = 1)]
pub fn nvim_ui_try_resize(editor: &mut Editor, width: i64, height: i64) -> Result<(), ApiError> {
    with_state_mut(editor, |state| state.ui_channels.try_resize(CHANNEL_ID, dimension(width, "width")?, dimension(height, "height")?).map_err(|error| ApiError::exception(error.to_string())))
}

fn color(value: &Object, name: &str) -> Result<u32, ApiError> {
    let Object::Integer(value) = value else { return Err(ApiError::validation(format!("{name} must be an Integer"))); };
    u32::try_from(*value).map_err(|_| ApiError::validation(format!("{name} out of range")))
}

fn attrs(dict: &Dict) -> Result<HlAttrs, ApiError> {
    let mut attrs = HlAttrs::default();
    for (key, value) in &dict.0 {
        match key.as_bytes() {
            b"fg" | b"foreground" => attrs.foreground = Some(color(value, "foreground")?),
            b"bg" | b"background" => attrs.background = Some(color(value, "background")?),
            b"sp" | b"special" => attrs.special = Some(color(value, "special")?),
            b"blend" => attrs.blend = Some(u8::try_from(color(value, "blend")?).map_err(|_| ApiError::validation("blend out of range"))?.min(100)),
            b"url" => match value { Object::String(value) => attrs.url = Some(value.clone()), _ => return Err(ApiError::validation("url must be a String")) },
            b"bold" | b"italic" | b"underline" | b"undercurl" | b"underdouble" | b"underdotted" | b"underdashed" | b"strikethrough" | b"reverse" | b"standout" | b"altfont" | b"dim" | b"blink" | b"conceal" | b"overline" => {
                let Object::Boolean(enabled) = value else { return Err(ApiError::validation(format!("{} must be a Boolean", key.to_string_lossy()))); };
                match key.as_bytes() {
                    b"bold" => attrs.bold = *enabled, b"italic" => attrs.italic = *enabled,
                    b"underline" => attrs.underline = *enabled, b"undercurl" => attrs.undercurl = *enabled,
                    b"underdouble" => attrs.underdouble = *enabled, b"underdotted" => attrs.underdotted = *enabled,
                    b"underdashed" => attrs.underdashed = *enabled, b"strikethrough" => attrs.strikethrough = *enabled,
                    b"reverse" | b"standout" => attrs.reverse = *enabled, b"altfont" => attrs.altfont = *enabled,
                    b"dim" => attrs.dim = *enabled, b"blink" => attrs.blink = *enabled, b"conceal" => attrs.conceal = *enabled,
                    b"overline" => attrs.overline = *enabled, _ => {},
                }
            }
            b"cterm" | b"link" | b"default" | b"force" => {},
            _ => return Err(ApiError::validation(format!("Invalid highlight key: {}", key.to_string_lossy()))),
        }
    }
    Ok(attrs)
}

fn group_id(state: &ox_ui::HlState, name: &OxStr) -> Option<u64> {
    state.groups().find_map(|(candidate, id)| (candidate == name).then_some(id))
}

/// Rebuilds the active render table from the given namespace's definitions.
fn activate_hl(state: &mut RuntimeState, ns_id: i64) {
    let active = state.hl_namespaces.entry(ns_id).or_default().clone();
    state.highlights = active;
}

#[api(since = 7)]
pub fn nvim_set_hl(editor: &mut Editor, ns_id: i64, name: OxStr, val: Dict) -> Result<(), ApiError> {
    if ns_id < 0 { return Err(ApiError::validation("namespace must be non-negative")); }
    with_state_mut(editor, |state| {
        let highlight = Highlight { rgb: attrs(&val)?, cterm: match val.get(&OxStr::from("cterm")) { Some(Object::Dict(dict)) => attrs(dict)?, _ => HlAttrs::default() }, info: Vec::new() };
        let ns = state.hl_namespaces.entry(ns_id).or_default();
        let (id, _) = ns.intern(highlight).map_err(|error| ApiError::exception(error.to_string()))?;
        ns.set_group(name, id).map_err(|error| ApiError::exception(error.to_string()))?;
        if ns_id == state.current_hl_ns { activate_hl(state, ns_id); }
        Ok(())
    })
}

#[api(since = 9)]
pub fn nvim_get_hl(editor: &mut Editor, ns_id: i64, opts: Dict) -> Result<Dict, ApiError> {
    if ns_id < 0 { return Err(ApiError::validation("namespace must be non-negative")); }
    with_state(editor, |state| {
        // An undefined namespace has no highlight definitions of its own; it
        // never falls back to the global (ns 0) table.
        let ns = state.hl_namespaces.get(&ns_id);
        let id = match (opts.get(&OxStr::from("id")), opts.get(&OxStr::from("name"))) {
            (Some(Object::Integer(id)), _) => u64::try_from(*id).map_err(|_| ApiError::validation("invalid highlight id"))?,
            (_, Some(Object::String(name))) => ns.and_then(|ns| group_id(ns, name)).ok_or_else(|| ApiError::validation("highlight group not found"))?,
            (None, None) => return Ok(Dict(ns.map(|ns| ns.groups().filter_map(|(name, id)| ns.get(id).map(|highlight| (name.clone(), highlight.rgb.to_object()))).collect::<Vec<_>>()).unwrap_or_default())),
            _ => return Err(ApiError::validation("id or name has wrong type")),
        };
        let highlight = ns.and_then(|ns| ns.get(id)).ok_or_else(|| ApiError::validation("highlight id not found"))?;
        match highlight.rgb.to_object() { Object::Dict(dict) => Ok(dict), _ => Ok(Dict(Vec::new())) }
    })
}

#[api(since = 7)]
pub fn nvim_get_hl_id_by_name(editor: &mut Editor, name: OxStr) -> Result<i64, ApiError> {
    with_state_mut(editor, |state| {
        let ns = state.hl_namespaces.entry(0).or_default();
        if let Some(id) = group_id(ns, &name) { return i64::try_from(id).map_err(|_| ApiError::exception("highlight id out of range")); }
        let (id, _) = ns.intern(Highlight::default()).map_err(|error| ApiError::exception(error.to_string()))?;
        ns.set_group(name, id).map_err(|error| ApiError::exception(error.to_string()))?;
        if state.current_hl_ns == 0 { activate_hl(state, 0); }
        i64::try_from(id).map_err(|_| ApiError::exception("highlight id out of range"))
    })
}

#[api(since = 12)]
pub fn nvim_get_hl_ns(editor: &mut Editor, _opts: Dict) -> Result<i64, ApiError> { Ok(with_state(editor, |state| state.current_hl_ns)) }

#[api(since = 10)]
pub fn nvim_set_hl_ns(editor: &mut Editor, ns_id: i64) -> Result<(), ApiError> {
    if ns_id < 0 { return Err(ApiError::validation("namespace must be non-negative")); }
    with_state_mut(editor, |state| { state.current_hl_ns = ns_id; activate_hl(state, ns_id); });
    Ok(())
}

#[api(since = 10, fast)]
pub fn nvim_set_hl_ns_fast(editor: &mut Editor, ns_id: i64) -> Result<(), ApiError> {
    if ns_id < 0 { return Err(ApiError::validation("namespace must be non-negative")); }
    with_state_mut(editor, |state| { state.fast_hl_ns = ns_id; activate_hl(state, ns_id); });
    Ok(())
}

#[api(since = 6)]
pub fn nvim_create_buf(editor: &mut Editor, listed: bool, _scratch: bool) -> Result<BufHandle, ApiError> {
    editor.create_buffer(listed).map_err(|error| ApiError::exception(error.to_string()))
}

#[api(since = 5)]
pub fn nvim_open_term(editor: &mut Editor, buffer: BufHandle, _opts: Dict) -> Result<i64, ApiError> {
    let buffer = if buffer.is_current() {
        editor.current_buffer().ok_or_else(|| ApiError::validation("No current buffer"))?
    } else {
        editor.buffer(buffer).map_err(|error| ApiError::validation(error.to_string()))?;
        buffer
    };
    with_state_mut(editor, |state| {
        let channel = state.next_channel;
        state.next_channel = state.next_channel.checked_add(1)
            .ok_or_else(|| ApiError::exception("channel id space exhausted"))?;
        state.channels.insert(channel, ChannelInfo {
            id: channel,
            stream: OxStr::from("socket"),
            mode: OxStr::from("terminal"),
            pty: None,
            buffer: Some(i64::from(buffer)),
            client: Dict(Vec::new()),
        });
        i64::try_from(channel).map_err(|_| ApiError::exception("channel id exceeds Integer range"))
    })
}

fn queue(editor: &mut Editor, data: &[u8], remap: Remap) -> Result<(), ApiError> {
    let keys = Keys::encode(data);
    editor.typeahead_mut().append(&keys, TypeaheadFlags { remap, ..TypeaheadFlags::default() });
    Ok(())
}

fn normalize_paste(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            output.push(b'\n');
            index += usize::from(bytes.get(index + 1) == Some(&b'\n'));
        } else {
            output.push(bytes[index]);
        }
        index += 1;
    }
    output
}

#[api(since = 6)]
pub fn nvim_paste(editor: &mut Editor, data: OxStr, crlf: bool, phase: i64) -> Result<bool, ApiError> {
    if ![-1, 1, 2, 3].contains(&phase) { return Err(ApiError::validation("phase must be -1, 1, 2, or 3")); }
    let bytes = if crlf { normalize_paste(data.as_bytes()) } else { data.0 };
    queue(editor, &bytes, Remap::No)?;
    with_state_mut(editor, |state| state.paste_phase = if phase == 3 || phase == -1 { -1 } else { phase });
    Ok(true)
}

#[api(since = 6)]
pub fn nvim_put(editor: &mut Editor, lines: Vec<OxStr>, put_type: OxStr, after: bool, follow: bool) -> Result<(), ApiError> {
    let kind = match put_type.as_bytes() { b"l" | b"V" => ox_editor::RegisterKind::LineWise, b"b" | [0x16] => ox_editor::RegisterKind::BlockWise { width: lines.iter().map(|line| line.0.len()).max().unwrap_or(1).max(1) }, _ => ox_editor::RegisterKind::CharacterWise };
    let content = RegisterContent::new(kind, lines.into_iter().map(|line| line.0).collect()).map_err(|error| ApiError::validation(error.to_string()))?;
    let window = editor.current_window().ok_or_else(|| ApiError::validation("No current window"))?;
    let buffer = editor.current_buffer().ok_or_else(|| ApiError::validation("No current buffer"))?;
    let cursor = editor.window(window).map_err(|error| ApiError::validation(error.to_string()))?.cursor;
    let position = Position { lnum: if after { cursor.lnum } else { cursor.lnum.saturating_sub(1).max(1) }, col: cursor.col };
    editor.put_content(buffer, position, &content, 0).map_err(|error| ApiError::exception(error.to_string()))?;
    if follow { editor.set_window_cursor(window, position).map_err(|error| ApiError::exception(error.to_string()))?; }
    Ok(())
}

#[api(since = 1)]
pub fn nvim_feedkeys(editor: &mut Editor, keys: OxStr, mode: OxStr, _escape_ks: bool) -> Result<(), ApiError> {
    let remap = if mode.as_bytes().contains(&b'n') { Remap::No } else { Remap::Yes };
    if mode.as_bytes().contains(&b'x') { editor.typeahead_mut().flush(); }
    queue(editor, keys.as_bytes(), remap)
}

#[api(since = 6)]
pub fn nvim_select_popupmenu_item(editor: &mut Editor, item: i64, _insert: bool, _finish: bool, _opts: Dict) -> Result<(), ApiError> {
    with_state_mut(editor, |state| state.chrome.select_popupmenu(item)); Ok(())
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_list_uis__API_META(), nvim_list_uis__API_DISPATCH)?;
    registry.register(nvim_ui_attach__API_META(), nvim_ui_attach__API_DISPATCH)?;
    registry.register(nvim_ui_detach__API_META(), nvim_ui_detach__API_DISPATCH)?;
    registry.register(nvim_ui_try_resize__API_META(), nvim_ui_try_resize__API_DISPATCH)?;
    registry.register(nvim_set_hl__API_META(), nvim_set_hl__API_DISPATCH)?;
    registry.register(nvim_get_hl__API_META(), nvim_get_hl__API_DISPATCH)?;
    registry.register(nvim_get_hl_id_by_name__API_META(), nvim_get_hl_id_by_name__API_DISPATCH)?;
    registry.register(nvim_get_hl_ns__API_META(), nvim_get_hl_ns__API_DISPATCH)?;
    registry.register(nvim_set_hl_ns__API_META(), nvim_set_hl_ns__API_DISPATCH)?;
    registry.register(nvim_set_hl_ns_fast__API_META(), nvim_set_hl_ns_fast__API_DISPATCH)?;
    registry.register(nvim_create_buf__API_META(), nvim_create_buf__API_DISPATCH)?;
    registry.register(nvim_open_term__API_META(), nvim_open_term__API_DISPATCH)?;
    registry.register(nvim_paste__API_META(), nvim_paste__API_DISPATCH)?;
    registry.register(nvim_put__API_META(), nvim_put__API_DISPATCH)?;
    registry.register(nvim_feedkeys__API_META(), nvim_feedkeys__API_DISPATCH)?;
    registry.register(nvim_select_popupmenu_item__API_META(), nvim_select_popupmenu_item__API_DISPATCH)?;
    Ok(())
}
