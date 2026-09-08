//! Buffer-scoped API functions.

use ox_editor::{
    BufferAttachSubscription, BufferEditMode, BufferFlags, BufferRelease, BufferTextEditRequest,
    Editor, ExtmarkPosition, MarkLocation, Mode, NormalState, OptionValue, VisualKind, VisualState,
};
use ox_text::{Buffer, Position};

use crate::{
    ApiError, BufHandle, Dict, LuaRef, Object, OxStr, Registry, RegistryError, WinHandle, api,
    session::ApiSession,
};

const API_CHANNEL_ID: u64 = 0;
const API_TIMESTAMP: i64 = 0;

/// Syncs the editor's `edit_mode` from the live mode machine, returning the
/// previous mode so the caller can restore it after the mutation. When the
/// current window is in Insert/Replace mode and shows the target buffer, the
/// editor needs `BufferEditMode::Insert` so `adjust_text_cursor` treats the
/// cursor as between characters (matching `mark_col_adjust` skipping
/// `restart_edit` cursors in upstream `mark.c`).
fn sync_edit_mode(session: &ApiSession, buffer: BufHandle) -> BufferEditMode {
    let insert = crate::runtime::mode_machine(session)
        .and_then(|machine| {
            machine
                .try_borrow()
                .ok()
                .map(|m| matches!(m.mode(), Mode::Insert(_) | Mode::Replace(_)))
        })
        .unwrap_or(false);
    let saved = session.with_editor(Editor::edit_mode);
    let new_mode =
        if insert && session.with_editor(|editor| editor.current_buffer() == Some(buffer)) {
            BufferEditMode::Insert
        } else {
            BufferEditMode::Normal
        };
    session.with_editor_mut(|editor| editor.set_edit_mode(new_mode));
    saved
}

pub(crate) fn resolve_buffer(
    session: &ApiSession,
    buffer: BufHandle,
) -> Result<BufHandle, ApiError> {
    session.with_editor(|editor| {
        let resolved = if buffer.is_current() {
            editor
                .current_buffer()
                .ok_or_else(|| ApiError::validation("No current buffer"))?
        } else {
            buffer
        };
        editor.buffer(resolved).map_err(|_| {
            ApiError::validation(format!("Invalid buffer id: {}", i64::from(resolved)))
        })?;
        Ok(resolved)
    })
}

pub(crate) fn resolve_buffer_if_valid(
    session: &ApiSession,
    buffer: BufHandle,
) -> Option<BufHandle> {
    session.with_editor(|editor| {
        let resolved = if buffer.is_current() {
            editor.current_buffer()?
        } else {
            buffer
        };
        editor.buffer(resolved).ok().map(|_| resolved)
    })
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
        match usize::try_from(raw) {
            Ok(value) => (value, false),
            Err(_) => unreachable!("validated line boundary must fit usize"),
        }
    }
}

/// Converts an inclusive get/set-text row to a zero-based row.
///
/// Unlike line boundaries, `-1` denotes the last actual row.
fn text_row(index: i64, line_count: usize) -> Option<usize> {
    let maximum = line_count.saturating_sub(1) as i128;
    let raw = if index < 0 {
        maximum + 1 + i128::from(index)
    } else {
        i128::from(index)
    };
    if raw < 0 || raw > maximum {
        return None;
    }
    usize::try_from(raw).ok()
}

fn normalize_set_text_row(index: i64, line_count: usize, name: &str) -> Result<usize, ApiError> {
    text_row(index, line_count)
        .ok_or_else(|| ApiError::validation(format!("Invalid '{name}': out of range")))
}

/// Normalizes a byte column for set-text. Negative columns use
/// `line_length + 1 + column`, making `-1` the byte position after the final byte.
fn normalize_set_text_column(
    column: i64,
    line_length: usize,
    name: &str,
) -> Result<usize, ApiError> {
    let maximum = line_length as i128;
    let raw = if column < 0 {
        maximum + 1 + i128::from(column)
    } else {
        i128::from(column)
    };
    if raw < 0 || raw > maximum {
        return Err(ApiError::validation(format!(
            "Invalid '{name}': out of range"
        )));
    }
    usize::try_from(raw)
        .map_err(|_| ApiError::validation(format!("Invalid '{name}': out of range")))
}

/// Applies get-text's permissive column semantics after row validation.
fn clamp_text_column(column: i64, line_length: usize) -> usize {
    let maximum = line_length as i128;
    let raw = if column < 0 {
        maximum + 1 + i128::from(column)
    } else {
        i128::from(column)
    };
    usize::try_from(raw.clamp(0, maximum)).unwrap_or(line_length)
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
        return Err(ApiError::validation(
            "'replacement string' item contains newlines",
        ));
    }
    Ok(lines.iter().map(|line| line.as_bytes().to_vec()).collect())
}

fn buffer_bool_option(
    session: &ApiSession,
    buffer: BufHandle,
    name: &str,
) -> Result<bool, ApiError> {
    session.with_editor(|editor| match editor.options().get_buffer(buffer, name) {
        Ok(OptionValue::Boolean(value)) => Ok(*value),
        Ok(_) => Err(ApiError::exception(format!(
            "Option '{name}' must be a boolean"
        ))),
        Err(error) => Err(ApiError::exception(error.to_string())),
    })
}

pub(crate) fn dict_bool(options: &Dict, key: &str, default: bool) -> Result<bool, ApiError> {
    let key = OxStr::from(key);
    match options.get(&key) {
        None => Ok(default),
        Some(Object::Boolean(value)) => Ok(*value),
        Some(_) => Err(ApiError::validation(format!("'{key:?}' must be a boolean"))),
    }
}

fn validate_dict_keys(options: &Dict, allowed: &[&str]) -> Result<(), ApiError> {
    if let Some((key, _)) = options.iter().find(|(key, _)| {
        !allowed
            .iter()
            .any(|allowed| key.as_bytes() == allowed.as_bytes())
    }) {
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

fn cursor_at(line: usize, column: usize) -> Position {
    Position {
        lnum: line.max(1),
        col: column,
    }
}

fn replace_lines(
    session: &ApiSession,
    buffer: BufHandle,
    start: usize,
    end: usize,
    replacement: &[Vec<u8>],
    cursor: Position,
) -> Result<(), ApiError> {
    session.with_editor_mut(|editor| {
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
                .replace_buffer_lines(ox_editor::LineReplaceRequest {
                    buffer,
                    start: start + 1,
                    end,
                    lines: replacement,
                    cursor_before: cursor,
                    cursor_after: cursor,
                    timestamp: API_TIMESTAMP,
                })
                .map(|_| ())
                .map_err(|error| ApiError::exception(error.to_string()))
        }
    })
}

#[api(since = 1)]
pub fn nvim_get_current_line(session: &ApiSession) -> Result<OxStr, ApiError> {
    let cursor = session.with_editor(|editor| {
        let window = editor
            .current_window()
            .ok_or_else(|| ApiError::validation("No current window"))?;
        Ok(editor
            .window(window)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .cursor)
    })?;
    let buffer = resolve_buffer(session, BufHandle::CURRENT)?;
    session.with_editor(|editor| {
        let line = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?
            .line(cursor.lnum)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        Ok(OxStr(line))
    })
}

#[api(since = 1, textlock)]
pub fn nvim_set_current_line(session: &ApiSession, line: OxStr) -> Result<(), ApiError> {
    let end = session.with_editor(|editor| {
        let window = editor
            .current_window()
            .ok_or_else(|| ApiError::validation("No current window"))?;
        let cursor = editor
            .window(window)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .cursor;
        i64::try_from(cursor.lnum)
            .map_err(|_| ApiError::exception("Cursor line exceeds API Integer range"))
    })?;
    nvim_buf_set_lines(session, BufHandle::CURRENT, end - 1, end, true, vec![line])
}

#[api(since = 1, method)]
pub fn nvim_buf_line_count(session: &ApiSession, buffer: BufHandle) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor(|editor| {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if !state.residency.is_loaded() {
            return Ok(0);
        }
        i64::try_from(
            state
                .text()
                .map_err(|error| ApiError::exception(error.to_string()))?
                .line_count(),
        )
        .map_err(|_| ApiError::exception("Buffer line count exceeds API Integer range"))
    })
}

#[api(since = 1, method)]
pub fn nvim_buf_get_lines(
    session: &ApiSession,
    buffer: BufHandle,
    start: i64,
    end: i64,
    strict_indexing: bool,
) -> Result<Vec<OxStr>, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor(|editor| {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if !state.residency.is_loaded() {
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
                    .map(OxStr)
                    .map_err(|error| ApiError::exception(error.to_string()))
            })
            .collect()
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes replacement as an owned Array"
)]
#[api(since = 1, method, textlock)]
pub fn nvim_buf_set_lines(
    session: &ApiSession,
    buffer: BufHandle,
    start: i64,
    end: i64,
    strict_indexing: bool,
    replacement: Vec<OxStr>,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    // Auto-load unloaded buffers before mutating (upstream `buf_load`):
    // `nvim_buf_set_lines` on an unloaded buffer loads it with empty text.
    let line_count = session.with_editor(|editor| {
        editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))
            .and_then(|state| {
                if state.residency.is_loaded() {
                    state
                        .text()
                        .map(Buffer::line_count)
                        .map_err(|error| ApiError::exception(error.to_string()))
                } else {
                    Ok(0)
                }
            })
    })?;
    let (start, end) = normalized_line_range(start, end, line_count, strict_indexing)?;
    if start > end {
        return Err(ApiError::validation("'start' is higher than 'end'"));
    }
    let replacement = validate_replacement(&replacement)?;
    if !buffer_bool_option(session, buffer, "modifiable")? {
        return Err(ApiError::exception("Buffer is not 'modifiable'"));
    }
    // Auto-load: if the buffer is unloaded, load empty text so the splice
    // can proceed (upstream `buf_load` in `nvim_buf_set_lines`).
    let needs_load = session.with_editor(|editor| {
        editor
            .buffer(buffer)
            .is_ok_and(|state| !state.residency.is_loaded())
    });
    if needs_load {
        session.with_editor_mut(|editor| {
            if let Ok(state) = editor.buffer_mut(buffer) {
                state.load(ox_text::Buffer::new());
            }
        });
    }
    session.with_editor_mut(|editor| editor.sync_buffer_undo(buffer));
    let old_count = end.saturating_sub(start);
    let new_count = replacement.len();
    let mode_machine = crate::runtime::mode_machine(session);
    replace_lines(
        session,
        buffer,
        start,
        end,
        &replacement,
        cursor_at(start + 1, 0),
    )?;
    if let Some(machine) = &mode_machine {
        session.with_editor(|editor| {
            let mut machine = machine.borrow_mut();
            if let Mode::Visual(state) = machine.mode_mut() {
                state.anchor = editor.adjust_position_for_line_edit(
                    buffer,
                    state.anchor,
                    start,
                    old_count,
                    new_count,
                );
            }
        });
    }
    session.with_editor_mut(|editor| editor.sync_buffer_undo(buffer));
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes options as an owned Dictionary"
)]
#[api(since = 9, method)]
pub fn nvim_buf_get_text(
    session: &ApiSession,
    buffer: BufHandle,
    start_row: i64,
    start_col: i64,
    end_row: i64,
    end_col: i64,
    options: Dict,
) -> Result<Vec<OxStr>, ApiError> {
    validate_dict_keys(&options, &[])?;
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor(|editor| {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if !state.residency.is_loaded() {
            return Ok(Vec::new());
        }
        let text = state
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let start_row = text_row(start_row, text.line_count())
            .ok_or_else(|| ApiError::validation("Index out of bounds"))?;
        let end_row = text_row(end_row, text.line_count())
            .ok_or_else(|| ApiError::validation("Index out of bounds"))?;
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
        let start_col = clamp_text_column(start_col, first.len());
        let end_col = clamp_text_column(end_col, last.len());
        if start_row == end_row {
            if start_col > end_col {
                return Err(ApiError::validation(
                    "start_col must be less than or equal to end_col",
                ));
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
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes replacement as an owned Array"
)]
#[api(since = 7, method, textlock)]
pub fn nvim_buf_set_text(
    session: &ApiSession,
    buffer: BufHandle,
    start_row: i64,
    start_col: i64,
    end_row: i64,
    end_col: i64,
    replacement: Vec<OxStr>,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    // Auto-load unloaded buffers before mutating (upstream `buf_load`).
    let needs_load = session.with_editor(|editor| {
        editor
            .buffer(buffer)
            .is_ok_and(|state| !state.residency.is_loaded())
    });
    if needs_load {
        session.with_editor_mut(|editor| {
            if let Ok(state) = editor.buffer_mut(buffer) {
                state.load(ox_text::Buffer::new());
            }
        });
    }
    let (start_row, end_row, start_col, end_col) = session.with_editor(|editor| {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let text = state
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let start_row = normalize_set_text_row(start_row, text.line_count(), "start_row")?;
        let end_row = normalize_set_text_row(end_row, text.line_count(), "end_row")?;
        let first = text
            .line(start_row + 1)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let last = if start_row == end_row {
            first.clone()
        } else {
            text.line(end_row + 1)
                .map_err(|error| ApiError::exception(error.to_string()))?
        };
        let start_col = normalize_set_text_column(start_col, first.len(), "start_col")?;
        let end_col = normalize_set_text_column(end_col, last.len(), "end_col")?;
        if start_row > end_row || (start_row == end_row && start_col > end_col) {
            return Err(ApiError::validation("'start' is higher than 'end'"));
        }
        Ok((start_row, end_row, start_col, end_col))
    })?;

    let replacement = validate_replacement(&replacement)?;
    if !buffer_bool_option(session, buffer, "modifiable")? {
        return Err(ApiError::exception("Buffer is not 'modifiable'"));
    }
    let replacement_for_anchor = replacement.clone();
    let saved_edit_mode = sync_edit_mode(session, buffer);
    let mode_machine = crate::runtime::mode_machine(session);
    session.with_editor_mut(|editor| {
        editor.sync_buffer_undo(buffer);
        let cursor = cursor_at(start_row + 1, start_col);
        editor
            .replace_buffer_text(
                buffer,
                &BufferTextEditRequest {
                    start: ExtmarkPosition::new(start_row, start_col),
                    end: ExtmarkPosition::new(end_row, end_col),
                    replacement,
                },
                cursor,
                cursor,
                API_TIMESTAMP,
            )
            .map_err(|error| ApiError::exception(error.to_string()))?;
        // Adjust the mode machine's visual anchor for the splice, the same
        // way the editor cursor was adjusted inside replace_buffer_text.
        if let Some(machine) = &mode_machine {
            let mut machine = machine.borrow_mut();
            if let Mode::Visual(state) = machine.mode_mut() {
                let block = state.kind == VisualKind::Block;
                state.anchor = editor.adjust_position_for_text_edit(
                    buffer,
                    state.anchor,
                    ExtmarkPosition::new(start_row, start_col),
                    ExtmarkPosition::new(end_row, end_col),
                    &replacement_for_anchor,
                    block,
                );
            }
        }
        editor.set_edit_mode(saved_edit_mode);
        Ok(())
    })
}

#[api(since = 5, method)]
pub fn nvim_buf_get_offset(
    session: &ApiSession,
    buffer: BufHandle,
    index: i64,
) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor(|editor| {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if !state.residency.is_loaded() {
            return Ok(-1);
        }
        let text = state
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        if index < 0 || i128::from(index) > text.line_count() as i128 {
            return Err(ApiError::validation("Index out of bounds"));
        }
        let line_count = text.line_count();
        let line =
            usize::try_from(index).map_err(|_| ApiError::validation("Index out of bounds"))? + 1;
        let offset = text
            .byte_of_line(line)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        // Neovim's `ml_find_line_or_offset` with `no_ff=true` (the
        // `nvim_buf_get_offset` path) counts the final line break for the
        // EOF pseudo-line unless both `'eol'` and `'fixeol'` are off
        // (memline.c:4155-4162). `byte_of_line` returns the serialized
        // length without the absent terminator, so add the virtual newline
        // when the buffer lacks a real one and either option still
        // requests it.
        let offset = if line == line_count + 1 && !text.has_eol() {
            let eol = match editor.options().get_buffer(buffer, "eol") {
                Ok(OptionValue::Boolean(value)) => *value,
                _ => true,
            };
            let fixeol = match editor.options().get_buffer(buffer, "fixeol") {
                Ok(OptionValue::Boolean(value)) => *value,
                _ => true,
            };
            if eol || fixeol { offset + 1 } else { offset }
        } else {
            offset
        };
        i64::try_from(offset)
            .map_err(|_| ApiError::exception("Buffer offset exceeds API Integer range"))
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes options as an owned Dictionary"
)]
#[api(since = 7, method, textlock)]
pub fn nvim_buf_delete(
    session: &ApiSession,
    buffer: BufHandle,
    options: Dict,
) -> Result<(), ApiError> {
    validate_dict_keys(&options, &["force", "unload"])?;
    let force = dict_bool(&options, "force", false)?;
    let unload = dict_bool(&options, "unload", false)?;
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor_mut(|editor| {
        // `force` only overrides unsaved-change protection: without it a
        // modified buffer fails with the E89 `do_buffer` raises
        // (`command_buffer_remove`). Once deletion proceeds, windows showing
        // the target buffer are rehomed onto a replacement REGARDLESS of
        // `force` (src/nvim/api/buffer.c:1039-1059, src/nvim/buffer.c:1039-1059).
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        if !force && state.flags.contains(BufferFlags::MODIFIED) {
            return Err(ApiError::exception(
                "E89: No write since last change (add ! to override)",
            ));
        }
        if state.attachments != 0 {
            let replacement = match editor
                .buffers()
                .into_iter()
                .find(|candidate| *candidate != buffer)
            {
                Some(candidate) => candidate,
                None => editor
                    .create_buffer(true)
                    .map_err(|error| ApiError::exception(error.to_string()))?,
            };
            let attached = editor
                .windows()
                .into_iter()
                .filter(|window| {
                    editor
                        .window(*window)
                        .is_ok_and(|state| state.buffer == buffer)
                })
                .collect::<Vec<_>>();
            for window in attached {
                editor
                    .set_window_buffer(window, replacement, BufferRelease::KeepLoaded)
                    .map_err(|error| ApiError::exception(error.to_string()))?;
            }
        }
        Ok(())
    })?;
    if unload {
        session.with_editor_mut(|editor| {
            editor
                .unload_buffer(buffer)
                .map_err(|error| ApiError::exception(error.to_string()))
        })
    } else {
        session.with_editor_mut(|editor| {
            editor
                .wipe_buffer(buffer)
                .map(|_| ())
                .map_err(|error| ApiError::exception(error.to_string()))
        })?;
        notify_wipe(session, buffer)?;
        Ok(())
    }
}

/// Drops the wiped buffer's command entries through the installed host. Only
/// the absent-host condition is ignored; real removal errors surface, and
/// unloads never clean up (`:bdelete` keeps buffer-local commands).
fn notify_wipe(session: &ApiSession, buffer: BufHandle) -> Result<(), ApiError> {
    let result = crate::runtime::with_command_executor(session, |_session, executor| {
        executor.remove_buffer(buffer)
    });
    match result {
        Err(ApiError::Exception(message)) if message == "no Ex-command host is installed" => Ok(()),
        result => result,
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires fallible method responses"
)]
#[api(since = 5, method)]
pub fn nvim_buf_is_loaded(session: &ApiSession, buffer: BufHandle) -> Result<bool, ApiError> {
    Ok(
        resolve_buffer_if_valid(session, buffer).is_some_and(|buffer| {
            session.with_editor(|editor| {
                editor
                    .buffer(buffer)
                    .is_ok_and(|state| state.residency.is_loaded())
            })
        }),
    )
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires fallible method responses"
)]
#[api(since = 1, method)]
pub fn nvim_buf_is_valid(session: &ApiSession, buffer: BufHandle) -> Result<bool, ApiError> {
    Ok(resolve_buffer_if_valid(session, buffer).is_some())
}

#[api(since = 1, method)]
pub fn nvim_buf_get_name(session: &ApiSession, buffer: BufHandle) -> Result<OxStr, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor(|editor| {
        editor
            .buffer(buffer)
            .map(|state| state.name().clone())
            .map_err(|error| ApiError::validation(error.to_string()))
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
#[api(since = 1, method)]
pub fn nvim_buf_set_name(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor_mut(|editor| {
        // Upstream `nvim_buf_set_name` calls `fname_expand` before
        // `rename_buffer` (api/buffer.c:997, buffer.c:3621): relative names
        // become absolute, directory names get a trailing separator, and
        // symlink spelling is preserved.  `rename_buffer` itself stays
        // generic — `:file` and other internal callers pass names as-is.
        let resolved = ox_editor::expand_buffer_name(&name);
        let is_current = editor
            .current_buffer()
            .is_some_and(|current| current == buffer);
        editor
            .rename_buffer(buffer, resolved)
            .map(|_| ())
            .map_err(|error| ApiError::exception(error.to_string()))?;
        // `rename_buffer` (ex_cmds.c:1764): change directories when 'acd'
        // is set.  Upstream disables `p_acd` for non-current buffers
        // (api/buffer.c:991), so only the current buffer triggers it.
        if is_current {
            editor.do_autochdir();
        }
        Ok(())
    })
}

#[api(since = 2, method)]
pub fn nvim_buf_get_changedtick(session: &ApiSession, buffer: BufHandle) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor(|editor| {
        let changedtick = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .script_changedtick();
        i64::try_from(changedtick)
            .map_err(|_| ApiError::exception("Buffer changedtick exceeds API Integer range"))
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes variable names as owned Strings"
)]
#[api(since = 1, method)]
pub fn nvim_buf_get_var(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor(|editor| {
        if name.as_bytes() == b"changedtick" {
            let changedtick = editor
                .buffer(buffer)
                .map_err(|error| ApiError::validation(error.to_string()))?
                .script_changedtick();
            return Ok(Object::Integer(
                i64::try_from(changedtick).unwrap_or(i64::MAX),
            ));
        }
        editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .variables()
            .get(&name)
            .cloned()
            .ok_or_else(|| {
                ApiError::validation(format!("Key not found: {}", name.to_string_lossy()))
            })
    })
}

#[api(since = 1, method)]
pub fn nvim_buf_set_var(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor_mut(|editor| {
        if name.as_bytes() == b"changedtick" {
            return Err(ApiError::validation(format!(
                "Key is read-only: {}",
                name.to_string_lossy()
            )));
        }
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if state.is_var_locked(&name) {
            return Err(ApiError::validation(format!(
                "Key is locked: {}",
                name.to_string_lossy()
            )));
        }
        state.variables_mut().insert(name, value);
        Ok(())
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes variable names as owned Strings"
)]
#[api(since = 1, method)]
pub fn nvim_buf_del_var(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor_mut(|editor| {
        if name.as_bytes() == b"changedtick" {
            return Err(ApiError::validation(format!(
                "Key is read-only: {}",
                name.to_string_lossy()
            )));
        }
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if state.is_var_locked(&name) {
            return Err(ApiError::validation(format!(
                "Key is locked: {}",
                name.to_string_lossy()
            )));
        }
        let variables = state.variables_mut();
        let Some(index) = variables.iter().position(|(key, _)| key == &name) else {
            return Err(ApiError::validation(format!(
                "Key not found: {}",
                name.to_string_lossy()
            )));
        };
        variables.0.remove(index);
        Ok(())
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes option names as owned Strings"
)]
#[api(since = 1, deprecated_since = 11, method)]
pub fn nvim_buf_get_option(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let name = option_name(&name)?;
    session.with_editor(|editor| {
        editor
            .options()
            .get_buffer(buffer, name)
            .map(option_to_object)
            .map_err(|error| ApiError::validation(error.to_string()))
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes option names as owned Strings"
)]
#[api(since = 1, deprecated_since = 11, method)]
pub fn nvim_buf_set_option(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let name = option_name(&name)?.to_owned();
    let metadata = ox_editor::OptionStore::metadata(name.as_str())
        .map_err(|error| ApiError::validation(error.to_string()))?;
    let value = crate::global::object_to_legacy_option_value(metadata, name.as_str(), value)?;
    session.with_editor_mut(|editor| {
        editor
            .options_mut()
            .set_buffer(buffer, name.as_str(), value)
            .map_err(|error| ApiError::validation(error.to_string()))
    })
}

#[api(since = 7, method)]
pub fn nvim_buf_call(
    session: &ApiSession,
    buffer: BufHandle,
    function: LuaRef,
) -> Result<Object, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let reference = usize::try_from(function.0)
        .map_err(|_| ApiError::exception("Lua callback reference is out of range"))?;
    let machine = crate::runtime::mode_machine(session);
    // Save the caller context: which window, what buffer it shows, and the
    // previous-window handle.  Mirrors upstream `ctx_switch` saving
    // `cs_curwin`, `cs_prevwin`, and `cs_new_curbuf` (`context.c:527-621`).
    let (caller, caller_buffer, previous_before) = session.with_editor(|editor| {
        let caller = editor.current_window();
        let caller_buffer = caller.and_then(|w| editor.window(w).ok().map(|s| s.buffer));
        (caller, caller_buffer, editor.previous_window())
    });
    // Enter the target buffer context.  A window already showing the buffer
    // is entered (preferring the caller); a hidden buffer temporarily takes
    // over the caller window.  `entered` tracks `(window, original_buffer)`
    // for restoration, mirroring `cs_new_curwin` / `cs_new_curbuf`.
    let entered: Option<(WinHandle, BufHandle)> = match caller {
        Some(caller) => session.with_editor_mut(|editor| {
            let current_buf = editor.window(caller).ok().map(|s| s.buffer);
            if current_buf == Some(buffer) {
                return None; // Caller already shows target — no switch needed.
            }
            // Look for a window already showing the target buffer.
            let visible = editor
                .windows()
                .into_iter()
                .find(|w| editor.window(*w).is_ok_and(|s| s.buffer == buffer));
            match visible {
                Some(window) if window != caller => {
                    if editor.set_current_window(window).is_ok() {
                        Some((window, buffer))
                    } else {
                        None
                    }
                }
                Some(_) => None, // caller shows target (covered above)
                None => {
                    // Hidden buffer: take over the caller window.
                    let original = caller_buffer.unwrap_or(buffer);
                    if editor
                        .set_current_buffer(buffer, BufferRelease::KeepLoaded)
                        .is_ok()
                    {
                        Some((caller, original))
                    } else {
                        None
                    }
                }
            }
        }),
        None => None,
    };
    // Park the caller's visual selection when the current buffer changes
    // (cross-window or hidden-buffer takeover), mirroring `context.c:574-578`
    // (`Visual.active = false` when `!cs_same_win`).  A same-buffer call
    // leaves visual mode untouched in both directions.
    let same_buffer = entered.is_none();
    let visual_saved = if same_buffer {
        None
    } else {
        machine.as_ref().and_then(|machine| {
            let mut guard = machine.try_borrow_mut().ok()?;
            match std::mem::replace(&mut guard.mode, Mode::Normal(NormalState::default())) {
                Mode::Visual(state) => Some(state),
                other => {
                    guard.mode = other;
                    None
                }
            }
        })
    };
    // Call the Lua callback with no editor borrow held.
    let outcome = crate::runtime::with_lua_executor(session, |session, executor| {
        executor
            .call_ref(session, reference, Vec::new())
            .map_err(ApiError::exception)
    });
    // Restore context on every path (including errors), mirroring
    // `ctx_restore` (`context.c:649-747`): buffer, window, previous-window,
    // and visual state are all unwound without masking the callback's result.
    if let Some(caller) = caller {
        session.with_editor_mut(|editor| {
            // Restore the entered window's buffer if the callback changed it
            // and the window is still valid (`ctx_restore` `kCtxSwitchBuf`).
            if let Some((window, expected)) = entered
                && editor.window(window).is_ok_and(|s| s.buffer != expected)
                && editor.buffer(expected).is_ok()
            {
                let _ = editor.set_window_buffer(window, expected, BufferRelease::KeepLoaded);
            }
            // Switch back to the caller window if it is still valid and not
            // already current (`ctx_restore_curwin` with fallback).
            let prior_previous = editor.previous_window();
            if editor.current_window() != Some(caller) && editor.window(caller).is_ok() {
                let _ = editor.set_current_window(caller);
            }
            if prior_previous == Some(caller) {
                editor.set_previous_window(previous_before);
            }
            // Restore visual state.  Always restored, even on error, matching
            // `ctx_restore` running inside `TRY_WRAP`.
            if !same_buffer {
                restore_visual_state(editor, machine.as_ref(), visual_saved.as_ref());
            }
        });
    }
    crate::runtime::release_lua_callback(session, reference);
    // Array is the internal retstack carrier. The Lua binding expands it and
    // therefore preserves the distinction between no return and one nil.
    Ok(Object::Array(outcome?))
}

/// Restores the parked selection after a cross-buffer call, clamping its
/// endpoints to the now-current buffer's bounds like the motion clamp does
/// (`motion.rs:55-62`, `context.c:715-717`).
fn restore_visual_state(
    editor: &mut Editor,
    machine: Option<&std::rc::Rc<std::cell::RefCell<ox_editor::ModeMachine>>>,
    saved: Option<&VisualState>,
) {
    let Some(machine) = machine else {
        return;
    };
    let Ok(mut guard) = machine.try_borrow_mut() else {
        return;
    };
    match saved {
        Some(state) => {
            let mut state = state.clone();
            if let Some(cursor) = clamp_visual_position(editor, state.cursor) {
                state.cursor = cursor;
            }
            if let Some(anchor) = clamp_visual_position(editor, state.anchor) {
                state.anchor = anchor;
            }
            if let Some(window) = editor.current_window() {
                let _ = editor.set_window_cursor(window, state.cursor);
            }
            guard.mode = Mode::Visual(state);
        }
        None => {
            // Visual was forced off for the call; a selection the callback
            // started must not leak past it.
            if matches!(guard.mode, Mode::Visual(_)) {
                guard.mode = Mode::Normal(NormalState::default());
            }
        }
    }
}

/// Clamps one visual endpoint to the buffer's bounds: rows stay within the
/// line count and columns on a char boundary within their line.
fn clamp_visual_position(editor: &Editor, position: Position) -> Option<Position> {
    let buffer = editor.current_buffer()?;
    let text = editor.buffer(buffer).ok()?.text().ok()?;
    let lnum = position.lnum.clamp(1, text.line_count().max(1));
    let line = text.line(lnum).ok()?;
    let col = position
        .col
        .min(ox_editor::motion::prev_char_boundary(&line, line.len()));
    Some(Position {
        lnum,
        col: ox_editor::motion::prev_char_boundary(&line, col.saturating_add(1)),
    })
}

fn mark_name(name: &OxStr) -> Result<char, ApiError> {
    let bytes = name.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
        Ok(char::from(bytes[0]))
    } else {
        Err(ApiError::validation("Invalid mark name"))
    }
}

#[api(since = 8, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_buf_get_mark(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Vec<i64>, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let name = mark_name(&name)?;
    let position = session.with_editor(|editor| {
        if name.is_ascii_lowercase() {
            editor
                .local_mark(buffer, name)
                .map_err(|error| ApiError::validation(error.to_string()))
        } else {
            Ok(editor
                .global_marks()
                .get(name)
                .map_err(|error| ApiError::validation(error.to_string()))?
                .filter(|location| location.buffer() == Some(buffer))
                .map(|location| location.position))
        }
    })?;
    Ok(position.map_or_else(
        || vec![0, 0],
        |position| {
            vec![
                i64::try_from(position.lnum).unwrap_or(i64::MAX),
                i64::try_from(position.col).unwrap_or(i64::MAX),
            ]
        },
    ))
}

#[api(since = 8, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_buf_set_mark(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
    line: i64,
    col: i64,
    opts: Dict,
) -> Result<bool, ApiError> {
    drop(opts);
    let buffer = resolve_buffer(session, buffer)?;
    let name = mark_name(&name)?;
    let lnum = usize::try_from(line).map_err(|_| ApiError::validation("Invalid line"))?;
    let col = usize::try_from(col).map_err(|_| ApiError::validation("Invalid column"))?;
    session.with_editor_mut(|editor| {
        if !editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .residency
            .is_loaded()
        {
            editor
                .buffer_mut(buffer)
                .map_err(|error| ApiError::validation(error.to_string()))?
                .load(Buffer::new());
        }
        let position = Position { lnum, col };
        if name.is_ascii_lowercase() {
            editor
                .set_local_mark(buffer, name, position)
                .map_err(|error| ApiError::validation(error.to_string()))?;
        } else {
            editor
                .global_marks_mut()
                .set(name, MarkLocation::in_buffer(buffer, position))
                .map_err(|error| ApiError::validation(error.to_string()))?;
        }
        Ok(true)
    })
}

#[api(since = 8, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_buf_del_mark(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let name = mark_name(&name)?;
    session.with_editor_mut(|editor| {
        if name.is_ascii_lowercase() {
            return editor
                .buffer_mut(buffer)
                .map_err(|error| ApiError::validation(error.to_string()))?
                .marks
                .remove(name)
                .map(|position| position.is_some())
                .map_err(|error| ApiError::validation(error.to_string()));
        }
        let belongs_to_buffer = editor
            .global_marks()
            .get(name)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .is_some_and(|location| location.buffer() == Some(buffer));
        if !belongs_to_buffer {
            return Ok(false);
        }
        Ok(editor
            .global_marks_mut()
            .remove(name)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .is_some())
    })
}

#[api(since = 4, method)]
pub fn nvim_buf_attach(
    session: &ApiSession,
    buffer: BufHandle,
    send_buffer: bool,
    options: Dict,
) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor_mut(|editor| {
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if !state.residency.is_loaded() {
            return Ok(false);
        }
        match session.requesting_channel() {
            Some(channel) => {
                // RPC subscriptions are keyed by their channel id, so a
                // channel can attach at most once per buffer.
                let subscription_id = u128::from(channel.get());
                let subscription = BufferAttachSubscription {
                    channel_id: channel.get(),
                    send_buffer,
                    options,
                };
                state.subscriptions_mut().insert(subscription_id, subscription);
            }
            None => {
                // In-process Lua calls get a distinct id per attach so
                // multiple plugins on the same buffer do not overwrite each
                // other. Detachment is by a truthy callback return.
                let subscription = BufferAttachSubscription {
                    channel_id: API_CHANNEL_ID,
                    send_buffer,
                    options,
                };
                state.attach_lua(subscription);
            }
        }
        Ok(true)
    })
}

#[api(since = 4, method)]
pub fn nvim_buf_detach(session: &ApiSession, buffer: BufHandle) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    session.with_editor_mut(|editor| {
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        if !state.residency.is_loaded() {
            return Ok(false);
        }
        match session.requesting_channel() {
            Some(channel) => {
                state.remove_subscriptions_by_channel(channel.get());
            }
            None => {
                // In-process Lua calls are not tied to an RPC channel;
                // detach every Lua callback for this buffer.
                state.remove_subscriptions_by_channel(API_CHANNEL_ID);
            }
        }
        Ok(true)
    })
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(
        nvim_get_current_line__API_META(),
        nvim_get_current_line__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_current_line__API_META(),
        nvim_set_current_line__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_line_count__API_META(),
        nvim_buf_line_count__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_lines__API_META(),
        nvim_buf_get_lines__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_set_lines__API_META(),
        nvim_buf_set_lines__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_text__API_META(),
        nvim_buf_get_text__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_set_text__API_META(),
        nvim_buf_set_text__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_offset__API_META(),
        nvim_buf_get_offset__API_DISPATCH,
    )?;
    registry.register(nvim_buf_delete__API_META(), nvim_buf_delete__API_DISPATCH)?;
    registry.register(
        nvim_buf_is_loaded__API_META(),
        nvim_buf_is_loaded__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_is_valid__API_META(),
        nvim_buf_is_valid__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_name__API_META(),
        nvim_buf_get_name__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_set_name__API_META(),
        nvim_buf_set_name__API_DISPATCH,
    )?;
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
    registry.register(
        nvim_buf_get_mark__API_META(),
        nvim_buf_get_mark__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_set_mark__API_META(),
        nvim_buf_set_mark__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_del_mark__API_META(),
        nvim_buf_del_mark__API_DISPATCH,
    )?;
    registry.register(nvim_buf_call__API_META(), nvim_buf_call__API_DISPATCH)?;
    registry.register(nvim_buf_attach__API_META(), nvim_buf_attach__API_DISPATCH)?;
    registry.register(nvim_buf_detach__API_META(), nvim_buf_detach__API_DISPATCH)?;
    Ok(())
}
