//! Deprecated API compatibility entries from `api/deprecated.c`.

use ox_editor::{Editor, Message, MessageKind};

use crate::{api, ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, TabHandle, WinHandle};

#[api(since = 7, deprecated_since = 11)]
pub fn nvim_exec(editor: &mut Editor, src: OxStr, _output: bool) -> Result<OxStr, ApiError> {
    crate::global::nvim_command(editor, src)?;
    Ok(OxStr::from(""))
}

#[api(since = 1, deprecated_since = 7)]
pub fn nvim_command_output(editor: &mut Editor, command: OxStr) -> Result<OxStr, ApiError> {
    crate::global::nvim_command(editor, command)?;
    Ok(OxStr::from(""))
}

#[api(since = 3, deprecated_since = 7)]
pub fn nvim_execute_lua(_editor: &mut Editor, _code: OxStr, _args: Vec<Object>) -> Result<Object, ApiError> {
    Err(ApiError::exception("Lua execution requires an attached Lua host"))
}

#[api(since = 1, deprecated_since = 2, method)]
pub fn nvim_buf_get_number(editor: &mut Editor, buffer: BufHandle) -> Result<i64, ApiError> {
    let buffer = if buffer.is_current() { editor.current_buffer().ok_or_else(|| ApiError::validation("No current buffer"))? } else { buffer };
    editor.buffer(buffer).map_err(|_| ApiError::validation(format!("Invalid buffer id: {}", i64::from(buffer))))?;
    Ok(i64::from(buffer))
}

#[api(since = 1, deprecated_since = 7, method)]
pub fn nvim_buf_clear_highlight(editor: &mut Editor, buffer: BufHandle, ns_id: i64, line_start: i64, line_end: i64) -> Result<(), ApiError> {
    crate::extmark::nvim_buf_clear_namespace(editor, buffer, ns_id, line_start, line_end)
}

#[api(since = 1, deprecated_since = 13, method)]
pub fn nvim_buf_add_highlight(editor: &mut Editor, buffer: BufHandle, ns_id: i64, hl_group: OxStr, line: i64, col_start: i64, col_end: i64) -> Result<i64, ApiError> {
    let namespace = if ns_id == 0 { crate::extmark::nvim_create_namespace(editor, OxStr::from(""))? } else { ns_id };
    let opts = Dict(vec![
        (OxStr::from("hl_group"), Object::String(hl_group)),
        (OxStr::from("end_row"), Object::Integer(line)),
        (OxStr::from("end_col"), Object::Integer(col_end)),
    ]);
    crate::extmark::nvim_buf_set_extmark(editor, buffer, namespace, line, col_start, opts)?;
    Ok(namespace)
}

#[api(since = 5, deprecated_since = 8, method)]
pub fn nvim_buf_set_virtual_text(editor: &mut Editor, buffer: BufHandle, src_id: i64, line: i64, chunks: Vec<Object>, _opts: Dict) -> Result<i64, ApiError> {
    let namespace = if src_id == 0 { crate::extmark::nvim_create_namespace(editor, OxStr::from(""))? } else { src_id };
    let opts = Dict(vec![(OxStr::from("virt_text"), Object::Array(chunks))]);
    crate::extmark::nvim_buf_set_extmark(editor, buffer, namespace, line, 0, opts)?;
    Ok(namespace)
}

#[api(since = 3, deprecated_since = 9)]
pub fn nvim_get_hl_by_id(editor: &mut Editor, hl_id: i64, _rgb: bool) -> Result<Dict, ApiError> {
    crate::ui::nvim_get_hl(editor, 0, Dict(vec![(OxStr::from("id"), Object::Integer(hl_id))]))
}

#[api(since = 3, deprecated_since = 9)]
pub fn nvim_get_hl_by_name(editor: &mut Editor, name: OxStr, _rgb: bool) -> Result<Dict, ApiError> {
    crate::ui::nvim_get_hl(editor, 0, Dict(vec![(OxStr::from("name"), Object::String(name))]))
}

#[api(since = 1, deprecated_since = 12)]
pub fn nvim_call_atomic(editor: &mut Editor, calls: Vec<Object>) -> Result<Vec<Object>, ApiError> {
    let registry = crate::core().map_err(|error| ApiError::exception(error.to_string()))?;
    let mut results = Vec::with_capacity(calls.len());
    for (index, call) in calls.into_iter().enumerate() {
        let Object::Array(call) = call else { return Err(ApiError::validation("each call must be an Array")); };
        let (Some(Object::String(name)), Some(Object::Array(args))) = (call.first(), call.get(1)) else { return Err(ApiError::validation("each call must contain name and arguments")); };
        let name = String::from_utf8(name.0.clone()).map_err(|_| ApiError::validation("call name must be UTF-8"))?;
        let Some((_, dispatch)) = registry.get(&name) else { return Err(ApiError::validation(format!("Invalid method: {name}"))); };
        match dispatch(editor, args) {
            Ok(value) => results.push(value),
            Err(error) => return Ok(vec![Object::Array(results), Object::Array(vec![
                Object::Integer(i64::try_from(index).unwrap_or(i64::MAX)),
                Object::Integer(error.error_type()),
                Object::String(OxStr::from(error.message())),
            ])]),
        }
    }
    Ok(vec![Object::Array(results), Object::Nil])
}

#[api(since = 1, deprecated_since = 13)]
pub fn nvim_out_write(editor: &mut Editor, str: OxStr) -> Result<(), ApiError> {
    editor.push_message(Message { kind: MessageKind::Echo, content: Object::String(str), history: false });
    Ok(())
}

#[api(since = 1, deprecated_since = 13)]
pub fn nvim_err_write(editor: &mut Editor, str: OxStr) -> Result<(), ApiError> {
    editor.push_message(Message { kind: MessageKind::Error, content: Object::String(str), history: false });
    Ok(())
}

#[api(since = 7, deprecated_since = 13)]
pub fn nvim_notify(editor: &mut Editor, msg: OxStr, _log_level: i64, _opts: Dict) -> Result<Object, ApiError> {
    editor.push_message(Message { kind: MessageKind::Echo, content: Object::String(msg), history: true });
    Ok(Object::Nil)
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_insert(editor: &mut Editor, buffer: BufHandle, lnum: i64, lines: Vec<OxStr>) -> Result<(), ApiError> {
    crate::buffer::nvim_buf_set_lines(editor, buffer, lnum, lnum, true, lines)
}

fn legacy_index(index: i64) -> i64 { if index < 0 { index.saturating_sub(1) } else { index } }

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_get_line(editor: &mut Editor, buffer: BufHandle, index: i64) -> Result<OxStr, ApiError> {
    crate::buffer::nvim_buf_get_lines(editor, buffer, legacy_index(index), legacy_index(index).saturating_add(1), true)?.into_iter().next().ok_or_else(|| ApiError::validation("line index out of bounds"))
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_set_line(editor: &mut Editor, buffer: BufHandle, index: i64, line: OxStr) -> Result<(), ApiError> {
    let index = legacy_index(index); crate::buffer::nvim_buf_set_lines(editor, buffer, index, index.saturating_add(1), true, vec![line])
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_del_line(editor: &mut Editor, buffer: BufHandle, index: i64) -> Result<(), ApiError> {
    let index = legacy_index(index); crate::buffer::nvim_buf_set_lines(editor, buffer, index, index.saturating_add(1), true, Vec::new())
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_get_line_slice(editor: &mut Editor, buffer: BufHandle, start: i64, end: i64, include_start: bool, include_end: bool) -> Result<Vec<OxStr>, ApiError> {
    crate::buffer::nvim_buf_get_lines(editor, buffer, legacy_index(start).saturating_add(i64::from(!include_start)), legacy_index(end).saturating_add(i64::from(include_end)), false)
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_set_line_slice(editor: &mut Editor, buffer: BufHandle, start: i64, end: i64, include_start: bool, include_end: bool, replacement: Vec<OxStr>) -> Result<(), ApiError> {
    crate::buffer::nvim_buf_set_lines(editor, buffer, legacy_index(start).saturating_add(i64::from(!include_start)), legacy_index(end).saturating_add(i64::from(include_end)), false, replacement)
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_set_var(editor: &mut Editor, buffer: BufHandle, name: OxStr, value: Object) -> Result<Object, ApiError> {
    let old = crate::buffer::nvim_buf_get_var(editor, buffer, name.clone()).unwrap_or(Object::Nil); crate::buffer::nvim_buf_set_var(editor, buffer, name, value)?; Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_del_var(editor: &mut Editor, buffer: BufHandle, name: OxStr) -> Result<Object, ApiError> {
    let old = crate::buffer::nvim_buf_get_var(editor, buffer, name.clone()).unwrap_or(Object::Nil); crate::buffer::nvim_buf_del_var(editor, buffer, name)?; Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn window_set_var(editor: &mut Editor, window: WinHandle, name: OxStr, value: Object) -> Result<Object, ApiError> {
    let old = crate::window::nvim_win_get_var(editor, window, name.clone()).unwrap_or(Object::Nil); crate::window::nvim_win_set_var(editor, window, name, value)?; Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn window_del_var(editor: &mut Editor, window: WinHandle, name: OxStr) -> Result<Object, ApiError> {
    let old = crate::window::nvim_win_get_var(editor, window, name.clone()).unwrap_or(Object::Nil); crate::window::nvim_win_del_var(editor, window, name)?; Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn tabpage_set_var(editor: &mut Editor, tabpage: TabHandle, name: OxStr, value: Object) -> Result<Object, ApiError> {
    let old = crate::tabpage::nvim_tabpage_get_var(editor, tabpage, name.clone()).unwrap_or(Object::Nil); crate::tabpage::nvim_tabpage_set_var(editor, tabpage, name, value)?; Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn tabpage_del_var(editor: &mut Editor, tabpage: TabHandle, name: OxStr) -> Result<Object, ApiError> {
    let old = crate::tabpage::nvim_tabpage_get_var(editor, tabpage, name.clone()).unwrap_or(Object::Nil); crate::tabpage::nvim_tabpage_del_var(editor, tabpage, name)?; Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn vim_set_var(editor: &mut Editor, name: OxStr, value: Object) -> Result<Object, ApiError> {
    let old = editor.vvars().get(&name).cloned().unwrap_or(Object::Nil); editor.vvars_mut().insert(name, value); Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn vim_del_var(editor: &mut Editor, name: OxStr) -> Result<Object, ApiError> {
    let index = editor.vvars().0.iter().position(|(key, _)| key == &name).ok_or_else(|| ApiError::validation("Key not found"))?;
    Ok(editor.vvars_mut().0.remove(index).1)
}

#[api(since = 7, deprecated_since = 11)]
pub fn nvim_get_option_info(_editor: &mut Editor, name: OxStr) -> Result<Dict, ApiError> {
    let name_text = std::str::from_utf8(name.as_bytes()).map_err(|_| ApiError::validation("option name must be UTF-8"))?;
    let metadata = ox_editor::option_metadata(name_text).ok_or_else(|| ApiError::validation(format!("Unknown option: {name_text}")))?;
    let scope = metadata.scopes.first().copied().unwrap_or(ox_editor::OptionScope::Global);
    Ok(Dict(vec![(OxStr::from("name"), Object::String(name)), (OxStr::from("scope"), Object::String(OxStr::from(match scope { ox_editor::OptionScope::Global => "global", ox_editor::OptionScope::Buffer => "buf", ox_editor::OptionScope::Window => "win", ox_editor::OptionScope::Tab => "tab" }))), (OxStr::from("global_local"), Object::Boolean(metadata.scopes.len() > 1))]))
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(buffer_insert__API_META(), buffer_insert__API_DISPATCH)?;
    registry.register(buffer_get_line__API_META(), buffer_get_line__API_DISPATCH)?;
    registry.register(buffer_set_line__API_META(), buffer_set_line__API_DISPATCH)?;
    registry.register(buffer_del_line__API_META(), buffer_del_line__API_DISPATCH)?;
    registry.register(buffer_get_line_slice__API_META(), buffer_get_line_slice__API_DISPATCH)?;
    registry.register(buffer_set_line_slice__API_META(), buffer_set_line_slice__API_DISPATCH)?;
    registry.register(buffer_set_var__API_META(), buffer_set_var__API_DISPATCH)?;
    registry.register(buffer_del_var__API_META(), buffer_del_var__API_DISPATCH)?;
    registry.register(window_set_var__API_META(), window_set_var__API_DISPATCH)?;
    registry.register(window_del_var__API_META(), window_del_var__API_DISPATCH)?;
    registry.register(tabpage_set_var__API_META(), tabpage_set_var__API_DISPATCH)?;
    registry.register(tabpage_del_var__API_META(), tabpage_del_var__API_DISPATCH)?;
    registry.register(vim_set_var__API_META(), vim_set_var__API_DISPATCH)?;
    registry.register(vim_del_var__API_META(), vim_del_var__API_DISPATCH)?;
    registry.register(nvim_get_option_info__API_META(), nvim_get_option_info__API_DISPATCH)?;
    registry.register(nvim_exec__API_META(), nvim_exec__API_DISPATCH)?;
    registry.register(nvim_command_output__API_META(), nvim_command_output__API_DISPATCH)?;
    registry.register(nvim_execute_lua__API_META(), nvim_execute_lua__API_DISPATCH)?;
    registry.register(nvim_buf_get_number__API_META(), nvim_buf_get_number__API_DISPATCH)?;
    registry.register(nvim_buf_clear_highlight__API_META(), nvim_buf_clear_highlight__API_DISPATCH)?;
    registry.register(nvim_buf_add_highlight__API_META(), nvim_buf_add_highlight__API_DISPATCH)?;
    registry.register(nvim_buf_set_virtual_text__API_META(), nvim_buf_set_virtual_text__API_DISPATCH)?;
    registry.register(nvim_get_hl_by_id__API_META(), nvim_get_hl_by_id__API_DISPATCH)?;
    registry.register(nvim_get_hl_by_name__API_META(), nvim_get_hl_by_name__API_DISPATCH)?;
    registry.register(nvim_call_atomic__API_META(), nvim_call_atomic__API_DISPATCH)?;
    registry.register(nvim_out_write__API_META(), nvim_out_write__API_DISPATCH)?;
    registry.register(nvim_err_write__API_META(), nvim_err_write__API_DISPATCH)?;
    registry.register(nvim_notify__API_META(), nvim_notify__API_DISPATCH)?;
    Ok(())
}
