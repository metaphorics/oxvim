//! Buffer-scoped API functions.

use ox_editor::{
    BufferAttachSubscription, BufferRelease, Editor, OptionValue, UserCommandDefinition,
};
use ox_text::Position;

use crate::{api, ApiError, BufHandle, Dict, LuaRef, Object, OxStr, Registry, RegistryError};

const API_CHANNEL_ID: u64 = 0;
const API_TIMESTAMP: i64 = 0;

fn resolve_buffer(editor: &Editor, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    let resolved = if buffer.is_current() {
        editor
            .current_buffer()
            .ok_or_else(|| ApiError::validation("No current buffer"))?
    } else {
        buffer
    };
    editor
        .buffer(resolved)
        .map_err(|_| ApiError::validation(format!("Invalid buffer id: {}", i64::from(resolved))))?;
    Ok(resolved)
}

fn resolve_buffer_if_valid(editor: &Editor, buffer: BufHandle) -> Option<BufHandle> {
    let resolved = if buffer.is_current() {
        editor.current_buffer()?
    } else {
        buffer
    };
    editor.buffer(resolved).ok().map(|_| resolved)
}

/// Normalizes the end-exclusive indices used by get/set-lines to `0..=line_count`.
/// Negative indices are `line_count + 1 + index`, so `-1` is one past the end.
fn normalize_line_boundary(index: i64, line_count: usize) -> (usize, bool) {
    let maximum = line_count as i128;
    let raw = if index < 0 {
        maximum + 1 + i128::from(index)
    } else {
        i128::from(index)
    };
    if raw < 0 {
        (0, true)
    } else if raw > maximum {
        (line_count, true)
    } else {
        (raw as usize, false)
    }
}

/// Normalizes the inclusive row indices used by get/set-text to `0..line_count`.
/// Unlike line boundaries, `-1` denotes the last actual row.
fn normalize_text_row(index: i64, line_count: usize, name: &str) -> Result<usize, ApiError> {
    let maximum = line_count.saturating_sub(1) as i128;
    let raw = if index < 0 {
        maximum + 1 + i128::from(index)
    } else {
        i128::from(index)
    };
    if raw < 0 || raw > maximum {
        return Err(ApiError::validation(format!("{name} out of bounds")));
    }
    Ok(raw as usize)
}

/// Normalizes a byte column. Negative columns use `line_length + 1 + column`,
/// making `-1` the byte position just after the final byte.
fn normalize_column(column: i64, line_length: usize, name: &str) -> Result<usize, ApiError> {
    let maximum = line_length as i128;
    let raw = if column < 0 {
        maximum + 1 + i128::from(column)
    } else {
        i128::from(column)
    };
    if raw < 0 || raw > maximum {
        return Err(ApiError::validation(format!("{name} out of bounds")));
    }
    Ok(raw as usize)
}

fn normalized_line_range(
    start: i64,
    end: i64,
    line_count: usize,
    strict_indexing: bool,
) -> Result<(usize, usize), ApiError> {
    let (start, start_oob) = normalize_line_boundary(start, line_count);
    let (end, end_oob) = normalize_line_boundary(end, line_count);
    if strict_indexing && (start_oob || end_oob) {
        return Err(ApiError::validation("Index out of bounds"));
    }
    Ok((start, end))
}

fn validate_replacement(lines: &[OxStr]) -> Result<Vec<Vec<u8>>, ApiError> {
    if lines.iter().any(|line| line.as_bytes().contains(&b'\n')) {
        return Err(ApiError::validation("replacement string contains newlines"));
    }
    Ok(lines.iter().map(|line| line.as_bytes().to_vec()).collect())
}

fn dict_bool(options: &Dict, key: &str, default: bool) -> Result<bool, ApiError> {
    let key = OxStr::from(key);
    match options.get(&key) {
        None => Ok(default),
        Some(Object::Boolean(value)) => Ok(*value),
        Some(_) => Err(ApiError::validation(format!("'{key:?}' must be a boolean"))),
    }
}

fn validate_dict_keys(options: &Dict, allowed: &[&str]) -> Result<(), ApiError> {
    if let Some((key, _)) = options
        .iter()
        .find(|(key, _)| !allowed.iter().any(|allowed| key.as_bytes() == allowed.as_bytes()))
    {
        return Err(ApiError::validation(format!(
            "Invalid key: {}",
            key.to_string_lossy()
        )));
    }
    Ok(())
}

fn option_name(name: &OxStr) -> Result<&str, ApiError> {
    std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Option name must be valid UTF-8"))
}

fn option_to_object(value: &OptionValue) -> Object {
    match value {
        OptionValue::Boolean(value) => Object::Boolean(*value),
        OptionValue::Number(value) => Object::Integer(*value),
        OptionValue::String(value) => Object::String(OxStr::from(value.as_str())),
    }
}

fn object_to_option(value: Object) -> Result<OptionValue, ApiError> {
    match value {
        Object::Boolean(value) => Ok(OptionValue::Boolean(value)),
        Object::Integer(value) => Ok(OptionValue::Number(value)),
        Object::String(value) => String::from_utf8(value.0)
            .map(OptionValue::String)
            .map_err(|_| ApiError::validation("Option string must be valid UTF-8")),
        _ => Err(ApiError::validation("Option value has an unsupported type")),
    }
}

fn cursor_at(line: usize, column: usize) -> Position {
    Position {
        lnum: line.max(1),
        col: column,
    }
}

fn replace_lines(
    editor: &mut Editor,
    buffer: BufHandle,
    start: usize,
    end: usize,
    replacement: &[Vec<u8>],
    cursor: Position,
) -> Result<(), ApiError> {
    if start == end {
        if replacement.is_empty() {
            return Ok(());
        }
        editor
            .append_buffer_lines(buffer, start, replacement, cursor, API_TIMESTAMP)
            .map(|_| ())
            .map_err(|error| ApiError::exception(error.to_string()))
    } else {
        editor
            .replace_buffer_lines(
                buffer,
                start + 1,
                end,
                replacement,
                cursor,
                cursor,
                API_TIMESTAMP,
            )
            .map(|_| ())
            .map_err(|error| ApiError::exception(error.to_string()))
    }
}

#[api(since = 1, method)]
pub fn nvim_buf_line_count(editor: &mut Editor, buffer: BufHandle) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let state = editor
        .buffer(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    if !state.loaded {
        return Ok(0);
    }
    i64::try_from(
        state
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?
            .line_count(),
    )
    .map_err(|_| ApiError::exception("Buffer line count exceeds API Integer range"))
}

#[api(since = 1, method)]
pub fn nvim_buf_get_lines(
    editor: &mut Editor,
    buffer: BufHandle,
    start: i64,
    end: i64,
    strict_indexing: bool,
) -> Result<Vec<OxStr>, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let state = editor
        .buffer(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    if !state.loaded {
        return Ok(Vec::new());
    }
    let text = state
        .text()
        .map_err(|error| ApiError::exception(error.to_string()))?;
    let (start, end) = normalized_line_range(start, end, text.line_count(), strict_indexing)?;
    if end <= start {
        return Ok(Vec::new());
    }
    (start + 1..=end)
        .map(|line| {
            text.line(line)
                .map(|bytes| OxStr(bytes))
                .map_err(|error| ApiError::exception(error.to_string()))
        })
        .collect()
}

#[api(since = 1, method, textlock)]
pub fn nvim_buf_set_lines(
    editor: &mut Editor,
    buffer: BufHandle,
    start: i64,
    end: i64,
    strict_indexing: bool,
    replacement: Vec<OxStr>,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let line_count = editor
        .buffer(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?
        .text()
        .map_err(|error| ApiError::exception(error.to_string()))?
        .line_count();
    let (start, end) = normalized_line_range(start, end, line_count, strict_indexing)?;
    if start > end {
        return Err(ApiError::validation("'start' is higher than 'end'"));
    }
    let replacement = validate_replacement(&replacement)?;
    replace_lines(
        editor,
        buffer,
        start,
        end,
        &replacement,
        cursor_at(start + 1, 0),
    )
}

#[api(since = 9, method)]
pub fn nvim_buf_get_text(
    editor: &mut Editor,
    buffer: BufHandle,
    start_row: i64,
    start_col: i64,
    end_row: i64,
    end_col: i64,
    options: Dict,
) -> Result<Vec<OxStr>, ApiError> {
    validate_dict_keys(&options, &[])?;
    let buffer = resolve_buffer(editor, buffer)?;
    let state = editor
        .buffer(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    if !state.loaded {
        return Ok(Vec::new());
    }
    let text = state
        .text()
        .map_err(|error| ApiError::exception(error.to_string()))?;
    let start_row = normalize_text_row(start_row, text.line_count(), "start_row")?;
    let end_row = normalize_text_row(end_row, text.line_count(), "end_row")?;
    if start_row > end_row {
        return Err(ApiError::validation("'start' is higher than 'end'"));
    }
    let first = text
        .line(start_row + 1)
        .map_err(|error| ApiError::exception(error.to_string()))?;
    let last = if start_row == end_row {
        first.clone()
    } else {
        text.line(end_row + 1)
            .map_err(|error| ApiError::exception(error.to_string()))?
    };
    let start_col = normalize_column(start_col, first.len(), "start_col")?;
    let end_col = normalize_column(end_col, last.len(), "end_col")?;
    if start_row == end_row {
        if start_col > end_col {
            return Err(ApiError::validation("'start' is higher than 'end'"));
        }
        return Ok(vec![OxStr(first[start_col..end_col].to_vec())]);
    }

    let mut result = Vec::with_capacity(end_row - start_row + 1);
    result.push(OxStr(first[start_col..].to_vec()));
    for row in start_row + 1..end_row {
        result.push(OxStr(
            text.line(row + 1)
                .map_err(|error| ApiError::exception(error.to_string()))?,
        ));
    }
    result.push(OxStr(last[..end_col].to_vec()));
    Ok(result)
}

#[api(since = 7, method, textlock)]
pub fn nvim_buf_set_text(
    editor: &mut Editor,
    buffer: BufHandle,
    start_row: i64,
    start_col: i64,
    end_row: i64,
    end_col: i64,
    replacement: Vec<OxStr>,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let (start_row, end_row, start_col, end_col, first, last) = {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let text = state
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let start_row = normalize_text_row(start_row, text.line_count(), "start_row")?;
        let end_row = normalize_text_row(end_row, text.line_count(), "end_row")?;
        if start_row > end_row {
            return Err(ApiError::validation("'start' is higher than 'end'"));
        }
        let first = text
            .line(start_row + 1)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let last = if start_row == end_row {
            first.clone()
        } else {
            text.line(end_row + 1)
                .map_err(|error| ApiError::exception(error.to_string()))?
        };
        let start_col = normalize_column(start_col, first.len(), "start_col")?;
        let end_col = normalize_column(end_col, last.len(), "end_col")?;
        if start_row == end_row && start_col > end_col {
            return Err(ApiError::validation("'start' is higher than 'end'"));
        }
        (start_row, end_row, start_col, end_col, first, last)
    };

    let mut replacement = validate_replacement(&replacement)?;
    if replacement.is_empty() {
        replacement.push(Vec::new());
    }
    let replacement_len = replacement.len();
    replacement[0].splice(0..0, first[..start_col].iter().copied());
    replacement[replacement_len - 1].extend_from_slice(&last[end_col..]);
    replace_lines(
        editor,
        buffer,
        start_row,
        end_row + 1,
        &replacement,
        cursor_at(start_row + 1, start_col),
    )
}

#[api(since = 5, method)]
pub fn nvim_buf_get_offset(
    editor: &mut Editor,
    buffer: BufHandle,
    index: i64,
) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let state = editor
        .buffer(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    if !state.loaded {
        return Ok(-1);
    }
    let text = state
        .text()
        .map_err(|error| ApiError::exception(error.to_string()))?;
    if index < 0 || i128::from(index) > text.line_count() as i128 {
        return Err(ApiError::validation("Index out of bounds"));
    }
    let line = usize::try_from(index)
        .map_err(|_| ApiError::validation("Index out of bounds"))?
        + 1;
    let offset = text
        .byte_of_line(line)
        .map_err(|error| ApiError::exception(error.to_string()))?;
    i64::try_from(offset).map_err(|_| ApiError::exception("Buffer offset exceeds API Integer range"))
}

#[api(since = 7, method, textlock)]
pub fn nvim_buf_delete(
    editor: &mut Editor,
    buffer: BufHandle,
    options: Dict,
) -> Result<(), ApiError> {
    validate_dict_keys(&options, &["force", "unload"])?;
    let force = dict_bool(&options, "force", false)?;
    let unload = dict_bool(&options, "unload", false)?;
    let buffer = resolve_buffer(editor, buffer)?;
    // Windows showing the target buffer must be rehomed onto a replacement
    // REGARDLESS of `force`; `force` only overrides unsaved-change protection
    // (src/nvim/api/buffer.c:1039-1059, src/nvim/buffer.c:1039-1059). Since
    // buffer modified-state is not modeled yet, `force` has no further
    // observable effect.
    if editor
        .buffer(buffer)
        .map_err(|error| ApiError::exception(error.to_string()))?
        .attachments
        != 0
    {
        let replacement = match editor.buffers().into_iter().find(|candidate| *candidate != buffer)
        {
            Some(candidate) => candidate,
            None => editor
                .create_buffer(true)
                .map_err(|error| ApiError::exception(error.to_string()))?,
        };
        let attached = editor
            .windows()
            .into_iter()
            .filter(|window| editor.window(*window).is_ok_and(|state| state.buffer == buffer))
            .collect::<Vec<_>>();
        for window in attached {
            editor
                .set_window_buffer(window, replacement, BufferRelease::KeepLoaded)
                .map_err(|error| ApiError::exception(error.to_string()))?;
        }
    }
    let _ = force;
    if unload {
        editor
            .unload_buffer(buffer)
            .map_err(|error| ApiError::exception(error.to_string()))
    } else {
        editor
            .wipe_buffer(buffer)
            .map(|_| ())
            .map_err(|error| ApiError::exception(error.to_string()))
    }
}

#[api(since = 5, method)]
pub fn nvim_buf_is_loaded(editor: &mut Editor, buffer: BufHandle) -> Result<bool, ApiError> {
    Ok(resolve_buffer_if_valid(editor, buffer)
        .and_then(|buffer| editor.buffer(buffer).ok())
        .is_some_and(|state| state.loaded))
}

#[api(since = 1, method)]
pub fn nvim_buf_is_valid(editor: &mut Editor, buffer: BufHandle) -> Result<bool, ApiError> {
    Ok(resolve_buffer_if_valid(editor, buffer).is_some())
}

#[api(since = 1, method)]
pub fn nvim_buf_get_name(editor: &mut Editor, buffer: BufHandle) -> Result<OxStr, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    editor
        .buffer(buffer)
        .map(|state| state.name().clone())
        .map_err(|error| ApiError::validation(error.to_string()))
}

#[api(since = 1, method)]
pub fn nvim_buf_set_name(
    editor: &mut Editor,
    buffer: BufHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    editor
        .buffer_mut(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?
        .set_name(name);
    Ok(())
}

#[api(since = 2, method)]
pub fn nvim_buf_get_changedtick(
    editor: &mut Editor,
    buffer: BufHandle,
) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let changedtick = editor
        .buffer(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?
        .changedtick();
    i64::try_from(changedtick)
        .map_err(|_| ApiError::exception("Buffer changedtick exceeds API Integer range"))
}

#[api(since = 1, method)]
pub fn nvim_buf_get_var(
    editor: &mut Editor,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    editor
        .buffer(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?
        .variables()
        .get(&name)
        .cloned()
        .ok_or_else(|| ApiError::validation(format!("Key not found: {}", name.to_string_lossy())))
}

#[api(since = 1, method)]
pub fn nvim_buf_set_var(
    editor: &mut Editor,
    buffer: BufHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    editor
        .buffer_mut(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?
        .variables_mut()
        .insert(name, value);
    Ok(())
}

#[api(since = 1, method)]
pub fn nvim_buf_del_var(
    editor: &mut Editor,
    buffer: BufHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let variables = editor
        .buffer_mut(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?
        .variables_mut();
    let Some(index) = variables.iter().position(|(key, _)| key == &name) else {
        return Err(ApiError::validation(format!(
            "Key not found: {}",
            name.to_string_lossy()
        )));
    };
    variables.0.remove(index);
    Ok(())
}

#[api(since = 1, deprecated_since = 11, method)]
pub fn nvim_buf_get_option(
    editor: &mut Editor,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let name = option_name(&name)?;
    editor
        .options()
        .get_buffer(buffer, name)
        .map(option_to_object)
        .map_err(|error| ApiError::validation(error.to_string()))
}

#[api(since = 1, deprecated_since = 11, method)]
pub fn nvim_buf_set_option(
    editor: &mut Editor,
    buffer: BufHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let name = option_name(&name)?.to_owned();
    let value = object_to_option(value)?;
    editor
        .options_mut()
        .set_buffer(buffer, &name, value)
        .map_err(|error| ApiError::validation(error.to_string()))
}

#[api(since = 7, method)]
pub fn nvim_buf_call(
    editor: &mut Editor,
    buffer: BufHandle,
    function: LuaRef,
) -> Result<Object, ApiError> {
    let _buffer = resolve_buffer(editor, buffer)?;
    let _function = function;
    Err(ApiError::exception("Not implemented: nvim_buf_call"))
}

#[api(since = 9, method)]
pub fn nvim_buf_create_user_command(
    editor: &mut Editor,
    buffer: BufHandle,
    name: OxStr,
    command: Object,
    options: Dict,
) -> Result<(), ApiError> {
    let bytes = name.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_uppercase)
        || !bytes.iter().all(u8::is_ascii_alphanumeric)
    {
        return Err(ApiError::validation(
            "Command name must begin with an uppercase letter and contain only alphanumeric characters",
        ));
    }
    if !matches!(command, Object::String(_) | Object::LuaRef(_)) {
        return Err(ApiError::validation(
            "Command must be a string or Lua function reference",
        ));
    }
    let force = dict_bool(&options, "force", true)?;
    let buffer = resolve_buffer(editor, buffer)?;
    let commands = editor
        .buffer_mut(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?
        .user_commands_mut();
    if !force && commands.contains_key(&name) {
        return Err(ApiError::validation(format!(
            "Command already exists: {}",
            name.to_string_lossy()
        )));
    }
    commands.insert(name, UserCommandDefinition { command, options });
    Ok(())
}

#[api(since = 4, method)]
pub fn nvim_buf_attach(
    editor: &mut Editor,
    buffer: BufHandle,
    send_buffer: bool,
    options: Dict,
) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let state = editor
        .buffer_mut(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    if !state.loaded {
        return Ok(false);
    }
    state.subscriptions_mut().insert(
        API_CHANNEL_ID,
        BufferAttachSubscription {
            channel_id: API_CHANNEL_ID,
            send_buffer,
            options,
        },
    );
    Ok(true)
}

#[api(since = 4, method)]
pub fn nvim_buf_detach(editor: &mut Editor, buffer: BufHandle) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let state = editor
        .buffer_mut(buffer)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    if !state.loaded {
        return Ok(false);
    }
    state.subscriptions_mut().remove(&API_CHANNEL_ID);
    Ok(true)
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_buf_line_count__API_META(), nvim_buf_line_count__API_DISPATCH)?;
    registry.register(nvim_buf_get_lines__API_META(), nvim_buf_get_lines__API_DISPATCH)?;
    registry.register(nvim_buf_set_lines__API_META(), nvim_buf_set_lines__API_DISPATCH)?;
    registry.register(nvim_buf_get_text__API_META(), nvim_buf_get_text__API_DISPATCH)?;
    registry.register(nvim_buf_set_text__API_META(), nvim_buf_set_text__API_DISPATCH)?;
    registry.register(nvim_buf_get_offset__API_META(), nvim_buf_get_offset__API_DISPATCH)?;
    registry.register(nvim_buf_delete__API_META(), nvim_buf_delete__API_DISPATCH)?;
    registry.register(nvim_buf_is_loaded__API_META(), nvim_buf_is_loaded__API_DISPATCH)?;
    registry.register(nvim_buf_is_valid__API_META(), nvim_buf_is_valid__API_DISPATCH)?;
    registry.register(nvim_buf_get_name__API_META(), nvim_buf_get_name__API_DISPATCH)?;
    registry.register(nvim_buf_set_name__API_META(), nvim_buf_set_name__API_DISPATCH)?;
    registry.register(
        nvim_buf_get_changedtick__API_META(),
        nvim_buf_get_changedtick__API_DISPATCH,
    )?;
    registry.register(nvim_buf_get_var__API_META(), nvim_buf_get_var__API_DISPATCH)?;
    registry.register(nvim_buf_set_var__API_META(), nvim_buf_set_var__API_DISPATCH)?;
    registry.register(nvim_buf_del_var__API_META(), nvim_buf_del_var__API_DISPATCH)?;
    registry.register(
        nvim_buf_get_option__API_META(),
        nvim_buf_get_option__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_set_option__API_META(),
        nvim_buf_set_option__API_DISPATCH,
    )?;
    registry.register(nvim_buf_call__API_META(), nvim_buf_call__API_DISPATCH)?;
    registry.register(
        nvim_buf_create_user_command__API_META(),
        nvim_buf_create_user_command__API_DISPATCH,
    )?;
    registry.register(nvim_buf_attach__API_META(), nvim_buf_attach__API_DISPATCH)?;
    registry.register(nvim_buf_detach__API_META(), nvim_buf_detach__API_DISPATCH)?;
    Ok(())
}
