//! Namespace and buffer extmark API.

use ox_editor::{
    Editor, Extmark, ExtmarkAttributes, ExtmarkEnd, ExtmarkGravity, ExtmarkId, ExtmarkPlacement,
    ExtmarkPosition, Extmarks, NamespaceId, VirtualLine, VirtualTextChunk,
};
use ox_text::Buffer;

use crate::runtime::{with_state, with_state_mut};
use crate::{api, ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError};

fn resolve_buffer(editor: &Editor, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    let buffer = if buffer.is_current() { editor.current_buffer().ok_or_else(|| ApiError::validation("No current buffer"))? } else { buffer };
    editor.buffer(buffer).map_err(|_| ApiError::validation(format!("Invalid buffer id: {}", i64::from(buffer))))?;
    Ok(buffer)
}

fn namespace(value: i64) -> Result<NamespaceId, ApiError> {
    u32::try_from(value).map_err(|_| ApiError::validation("Invalid ns_id"))
        .and_then(|value| NamespaceId::new(value).map_err(|error| ApiError::validation(error.to_string())))
}

fn position(row: i64, col: i64) -> Result<ExtmarkPosition, ApiError> {
    if row < 0 || col < 0 { return Err(ApiError::validation("row and col must be non-negative")); }
    Ok(ExtmarkPosition::new(
        usize::try_from(row).map_err(|_| ApiError::validation("row out of range"))?,
        usize::try_from(col).map_err(|_| ApiError::validation("col out of range"))?,
    ))
}

fn boolean(opts: &Dict, key: &str, default: bool) -> Result<bool, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(default),
        Some(Object::Boolean(value)) => Ok(*value),
        Some(_) => Err(ApiError::validation(format!("'{key}' must be a boolean"))),
    }
}

fn integer(opts: &Dict, key: &str) -> Result<Option<i64>, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(None), Some(Object::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(ApiError::validation(format!("'{key}' must be an integer"))),
    }
}

fn string(opts: &Dict, key: &str) -> Result<Option<String>, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(None), Some(Object::String(value)) => String::from_utf8(value.0.clone()).map(Some).map_err(|_| ApiError::validation(format!("'{key}' must be UTF-8"))),
        Some(_) => Err(ApiError::validation(format!("'{key}' must be a string"))),
    }
}

fn chunks(value: &Object) -> Result<Vec<VirtualTextChunk>, ApiError> {
    let Object::Array(items) = value else { return Err(ApiError::validation("virtual text must be an array")); };
    items.iter().map(|item| {
        let Object::Array(parts) = item else { return Err(ApiError::validation("virtual text chunk must be an array")); };
        let Some(Object::String(text)) = parts.first() else { return Err(ApiError::validation("virtual text chunk text must be a string")); };
        let text = String::from_utf8(text.0.clone()).map_err(|_| ApiError::validation("virtual text must be UTF-8"))?;
        let highlight_groups = match parts.get(1) {
            None | Some(Object::Nil) => Vec::new(),
            Some(Object::String(value)) => vec![String::from_utf8(value.0.clone()).map_err(|_| ApiError::validation("highlight group must be UTF-8"))?],
            Some(Object::Array(values)) => values.iter().map(|value| match value { Object::String(value) => String::from_utf8(value.0.clone()).map_err(|_| ApiError::validation("highlight group must be UTF-8")), _ => Err(ApiError::validation("highlight group must be a string")) }).collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(ApiError::validation("highlight group must be a string or array")),
        };
        Ok(VirtualTextChunk { text, highlight_groups })
    }).collect()
}

fn placement(row: i64, col: i64, opts: &Dict) -> Result<ExtmarkPlacement, ApiError> {
    let mut placement = ExtmarkPlacement::new(position(row, col)?);
    placement.gravity = if boolean(opts, "right_gravity", true)? { ExtmarkGravity::Right } else { ExtmarkGravity::Left };
    if let Some(end_row) = integer(opts, "end_row")? {
        let end_col = integer(opts, "end_col")?.unwrap_or(col);
        placement.end = Some(ExtmarkEnd {
            position: position(end_row, end_col)?,
            gravity: if boolean(opts, "end_right_gravity", false)? { ExtmarkGravity::Right } else { ExtmarkGravity::Left },
        });
    } else if integer(opts, "end_col")?.is_some() {
        return Err(ApiError::validation("end_col requires end_row"));
    }
    let mut attributes = ExtmarkAttributes::default();
    attributes.highlight_group = string(opts, "hl_group")?;
    attributes.sign_text = string(opts, "sign_text")?;
    attributes.priority = integer(opts, "priority")?.map_or(Ok(0), |value| u32::try_from(value).map_err(|_| ApiError::validation("priority out of range")))?;
    attributes.invalidate = boolean(opts, "invalidate", false)?;
    if let Some(value) = opts.get(&OxStr::from("virt_text")) { attributes.virtual_text = chunks(value)?; }
    if let Some(Object::Array(lines)) = opts.get(&OxStr::from("virt_lines")) {
        attributes.virtual_lines = lines.iter().map(chunks).collect::<Result<Vec<VirtualLine>, _>>()?;
    } else if opts.get(&OxStr::from("virt_lines")).is_some() { return Err(ApiError::validation("virt_lines must be an array")); }
    placement.attributes = attributes;
    Ok(placement)
}

fn chunk_object(chunk: &VirtualTextChunk) -> Object {
    let mut values = vec![Object::String(OxStr::from(chunk.text.as_str()))];
    if chunk.highlight_groups.len() == 1 {
        values.push(Object::String(OxStr::from(chunk.highlight_groups[0].as_str())));
    } else if !chunk.highlight_groups.is_empty() {
        values.push(Object::Array(chunk.highlight_groups.iter().map(|value| Object::String(OxStr::from(value.as_str()))).collect()));
    }
    Object::Array(values)
}

fn details(mark: &Extmark) -> Dict {
    let placement = &mark.placement;
    let mut values = vec![
        (OxStr::from("ns_id"), Object::Integer(i64::from(mark.namespace.get()))),
        (OxStr::from("right_gravity"), Object::Boolean(placement.gravity == ExtmarkGravity::Right)),
        (OxStr::from("invalidate"), Object::Boolean(placement.attributes.invalidate)),
        (OxStr::from("invalid"), Object::Boolean(mark.invalid)),
        (OxStr::from("priority"), Object::Integer(i64::from(placement.attributes.priority))),
    ];
    if let Some(end) = placement.end {
        values.push((OxStr::from("end_row"), Object::Integer(i64::try_from(end.position.row).unwrap_or(i64::MAX))));
        values.push((OxStr::from("end_col"), Object::Integer(i64::try_from(end.position.column).unwrap_or(i64::MAX))));
        values.push((OxStr::from("end_right_gravity"), Object::Boolean(end.gravity == ExtmarkGravity::Right)));
    }
    if let Some(group) = &placement.attributes.highlight_group { values.push((OxStr::from("hl_group"), Object::String(OxStr::from(group.as_str())))); }
    if let Some(text) = &placement.attributes.sign_text { values.push((OxStr::from("sign_text"), Object::String(OxStr::from(text.as_str())))); }
    if !placement.attributes.virtual_text.is_empty() { values.push((OxStr::from("virt_text"), Object::Array(placement.attributes.virtual_text.iter().map(chunk_object).collect()))); }
    if !placement.attributes.virtual_lines.is_empty() { values.push((OxStr::from("virt_lines"), Object::Array(placement.attributes.virtual_lines.iter().map(|line| Object::Array(line.iter().map(chunk_object).collect())).collect()))); }
    Dict(values)
}

#[api(since = 5)]
pub fn nvim_create_namespace(editor: &mut Editor, name: OxStr) -> Result<i64, ApiError> {
    if !name.0.is_empty() {
        if let Some(id) = with_state(editor, |state| state.namespaces.get(&name).copied()) { return Ok(i64::from(id)); }
    }
    let id = with_state_mut(editor, |state| {
        let id = state.next_namespace;
        state.next_namespace = state.next_namespace.checked_add(1).unwrap_or(u32::MAX);
        if !name.0.is_empty() { state.namespaces.insert(name, id); }
        id
    });
    Ok(i64::from(id))
}

#[api(since = 5)]
pub fn nvim_get_namespaces(editor: &mut Editor) -> Result<Dict, ApiError> {
    Ok(with_state(editor, |state| Dict(state.namespaces.iter().map(|(name, id)| (name.clone(), Object::Integer(i64::from(*id)))).collect())))
}

#[api(since = 7, method)]
pub fn nvim_buf_set_extmark(editor: &mut Editor, buffer: BufHandle, ns_id: i64, line: i64, col: i64, opts: Dict) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let namespace = namespace(ns_id)?;
    let requested = integer(&opts, "id")?.map(|id| u32::try_from(id).map_err(|_| ApiError::validation("id must be positive")).and_then(|id| ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string())))).transpose()?;
    let strict = boolean(&opts, "strict", true)?;
    let placement = placement(line, col, &opts)?;
    let state = editor.buffer_mut(buffer).map_err(|error| ApiError::validation(error.to_string()))?;
    if strict {
        // Per api.txt `nvim_buf_set_extmark()`: strict (default true) rejects a
        // line past end-of-buffer or a column past end-of-line.
        let text = state.text().map_err(|error| ApiError::exception(error.to_string()))?;
        validate_strict_position(text, &placement)?;
    }
    state.extmarks.ensure_namespace(namespace).map_err(|error| ApiError::validation(error.to_string()))?;
    let id = state.extmarks.set(namespace, requested, placement).map_err(|error| ApiError::validation(error.to_string()))?;
    Ok(i64::from(id.get()))
}

fn validate_strict_position(text: &Buffer, placement: &ExtmarkPlacement) -> Result<(), ApiError> {
    let line_count = text.line_count();
    let line = placement.position.row;
    if line > line_count { return Err(ApiError::validation("line: value outside range")); }
    let line_len = if line < line_count { text.line(line + 1).map_err(|error| ApiError::exception(error.to_string()))?.len() } else { 0 };
    if placement.position.column > line_len { return Err(ApiError::validation("col: value outside range")); }
    if let Some(end) = &placement.end {
        let end_line = end.position.row;
        if end_line > line_count { return Err(ApiError::validation("end_row: value outside range")); }
        let end_len = if end_line < line_count { text.line(end_line + 1).map_err(|error| ApiError::exception(error.to_string()))?.len() } else { 0 };
        if end.position.column > end_len { return Err(ApiError::validation("end_col: value outside range")); }
    }
    Ok(())
}

#[api(since = 7, method)]
pub fn nvim_buf_del_extmark(editor: &mut Editor, buffer: BufHandle, ns_id: i64, id: i64) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let namespace = namespace(ns_id)?;
    let id = u32::try_from(id).map_err(|_| ApiError::validation("invalid extmark id")).and_then(|id| ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string())))?;
    editor.buffer_mut(buffer).map_err(|error| ApiError::validation(error.to_string()))?.extmarks.delete(namespace, id).map_err(|error| ApiError::validation(error.to_string()))
}

#[api(since = 7, method)]
pub fn nvim_buf_get_extmark_by_id(editor: &mut Editor, buffer: BufHandle, ns_id: i64, id: i64, opts: Dict) -> Result<Vec<Object>, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let namespace = namespace(ns_id)?;
    let id = u32::try_from(id).map_err(|_| ApiError::validation("invalid extmark id")).and_then(|id| ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string())))?;
    let Some(mark) = editor.buffer(buffer).map_err(|error| ApiError::validation(error.to_string()))?.extmarks.get(namespace, id).map_err(|error| ApiError::validation(error.to_string()))? else { return Ok(Vec::new()); };
    let mut result = vec![Object::Integer(i64::try_from(mark.position().row).unwrap_or(i64::MAX)), Object::Integer(i64::try_from(mark.position().column).unwrap_or(i64::MAX))];
    if boolean(&opts, "details", false)? { result.push(Object::Dict(details(mark))); }
    Ok(result)
}

fn bound(extmarks: &Extmarks, namespace: Option<NamespaceId>, value: Object) -> Result<ExtmarkPosition, ApiError> {
    match value {
        Object::Integer(0) => Ok(ExtmarkPosition::new(0, 0)),
        Object::Integer(-1) => Ok(ExtmarkPosition::new(usize::MAX, usize::MAX)),
        Object::Integer(value) if value > 0 => {
            // A positive integer bound is a valid extmark id whose position
            // defines the bound (api.txt `nvim_buf_get_extmarks()`).
            let namespace = namespace.ok_or_else(|| ApiError::validation("mark id bounds require a namespace"))?;
            let id = u32::try_from(value).map_err(|_| ApiError::validation("invalid extmark id"))?;
            let id = ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string()))?;
            let mark = extmarks.get(namespace, id).map_err(|error| ApiError::validation(error.to_string()))?
                .ok_or_else(|| ApiError::validation(format!("invalid extmark id: {value}")))?;
            Ok(mark.position())
        }
        Object::Integer(_) => Err(ApiError::validation("invalid extmark bound")),
        Object::Array(values) if values.len() == 2 => match (&values[0], &values[1]) { (Object::Integer(row), Object::Integer(col)) => position(*row, *col), _ => Err(ApiError::validation("invalid extmark position")) },
        _ => Err(ApiError::validation("invalid extmark position")),
    }
}

#[api(since = 7, method)]
pub fn nvim_buf_get_extmarks(editor: &mut Editor, buffer: BufHandle, ns_id: i64, start: Object, end: Object, opts: Dict) -> Result<Vec<Vec<Object>>, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let limit = integer(&opts, "limit")?.map(|value| usize::try_from(value).map_err(|_| ApiError::validation("limit must be non-negative"))).transpose()?;
    let include_details = boolean(&opts, "details", false)?;
    let state = editor.buffer(buffer).map_err(|error| ApiError::validation(error.to_string()))?;
    let marks = if ns_id == -1 {
        // ns_id -1 queries every namespace (api.txt `nvim_buf_get_extmarks()`).
        state.extmarks.query_all(bound(&state.extmarks, None, start)?, bound(&state.extmarks, None, end)?, limit)
    } else {
        let namespace = namespace(ns_id)?;
        state.extmarks.query(namespace, bound(&state.extmarks, Some(namespace), start)?, bound(&state.extmarks, Some(namespace), end)?, limit).map_err(|error| ApiError::validation(error.to_string()))?
    };
    Ok(marks.into_iter().map(|mark| {
        let mut row = vec![Object::Integer(i64::from(mark.id.get())), Object::Integer(i64::try_from(mark.position().row).unwrap_or(i64::MAX)), Object::Integer(i64::try_from(mark.position().column).unwrap_or(i64::MAX))];
        if include_details { row.push(Object::Dict(details(&mark))); }
        row
    }).collect())
}

#[api(since = 5, method)]
pub fn nvim_buf_clear_namespace(editor: &mut Editor, buffer: BufHandle, ns_id: i64, line_start: i64, line_end: i64) -> Result<(), ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    if line_start < 0 || line_end < -1 { return Err(ApiError::validation("line range must be non-negative")); }
    if line_end != -1 && line_end <= line_start { return Ok(()); }
    let first = ExtmarkPosition::new(usize::try_from(line_start).map_err(|_| ApiError::validation("line_start out of range"))?, 0);
    let last = if line_end == -1 { ExtmarkPosition::new(usize::MAX, usize::MAX) } else { ExtmarkPosition::new(usize::try_from(line_end.saturating_sub(1)).map_err(|_| ApiError::validation("line_end out of range"))?, usize::MAX) };
    let state = editor.buffer_mut(buffer).map_err(|error| ApiError::validation(error.to_string()))?;
    let namespaces = if ns_id == -1 { state.extmarks.namespace_ids() } else { vec![namespace(ns_id)?] };
    for namespace in namespaces { state.extmarks.clear(namespace, first, last).map_err(|error| ApiError::validation(error.to_string()))?; }
    Ok(())
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_create_namespace__API_META(), nvim_create_namespace__API_DISPATCH)?;
    registry.register(nvim_get_namespaces__API_META(), nvim_get_namespaces__API_DISPATCH)?;
    registry.register(nvim_buf_set_extmark__API_META(), nvim_buf_set_extmark__API_DISPATCH)?;
    registry.register(nvim_buf_del_extmark__API_META(), nvim_buf_del_extmark__API_DISPATCH)?;
    registry.register(nvim_buf_get_extmark_by_id__API_META(), nvim_buf_get_extmark_by_id__API_DISPATCH)?;
    registry.register(nvim_buf_get_extmarks__API_META(), nvim_buf_get_extmarks__API_DISPATCH)?;
    registry.register(nvim_buf_clear_namespace__API_META(), nvim_buf_clear_namespace__API_DISPATCH)?;
    Ok(())
}
