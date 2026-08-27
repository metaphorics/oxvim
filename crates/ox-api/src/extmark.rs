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

fn allocated_namespace(editor: &Editor, value: i64) -> Result<NamespaceId, ApiError> {
    let public = u32::try_from(value).map_err(|_| ApiError::validation(format!("Invalid 'ns_id': {value}")))?;
    if public == 0 {
        return Err(ApiError::validation(format!("Invalid 'ns_id': {value}")));
    }
    let next = with_state(editor, |state| state.next_namespace);
    if public >= next {
        return Err(ApiError::validation(format!("Invalid 'ns_id': {value}")));
    }
    NamespaceId::new(public).map_err(|error| ApiError::validation(error.to_string()))
}

fn position(row: i64, col: i64) -> Result<ExtmarkPosition, ApiError> {
    if row < 0 || col < 0 { return Err(ApiError::validation("row and col must be non-negative")); }
    Ok(ExtmarkPosition::new(
        usize::try_from(row).map_err(|_| ApiError::validation("row out of range"))?,
        usize::try_from(col).map_err(|_| ApiError::validation("col out of range"))?,
    ))
}

fn object_type(value: &Object) -> &'static str {
    match value {
        Object::Nil => "Nil",
        Object::Boolean(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Float(_) => "Float",
        Object::String(_) => "String",
        Object::Array(_) => "Array",
        Object::Dict(_) => "Dictionary",
        Object::LuaRef(_) => "LuaRef",
        Object::Buffer(_) => "Buffer",
        Object::Window(_) => "Window",
        Object::Tabpage(_) => "Tabpage",
    }
}

fn boolean(opts: &Dict, key: &str, default: bool) -> Result<bool, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(default),
        Some(Object::Boolean(value)) => Ok(*value),
        Some(_) => Err(ApiError::validation(format!("Invalid '{key}': expected boolean"))),
    }
}

fn integer(opts: &Dict, key: &str) -> Result<Option<i64>, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::Integer(value)) => Ok(Some(*value)),
        Some(value) => Err(ApiError::validation(format!("Invalid '{key}': expected Integer, got {}", object_type(value)))),
    }
}

fn string(opts: &Dict, key: &str) -> Result<Option<String>, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::String(value)) => String::from_utf8(value.0.clone()).map(Some).map_err(|_| ApiError::validation(format!("'{key}' must be UTF-8"))),
        Some(value) => Err(ApiError::validation(format!("Invalid '{key}': expected String, got {}", object_type(value)))),
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

fn parse_virtual_text_position(opts: &Dict) -> Result<(), ApiError> {
    let _ = integer(opts, "virt_text_win_col")?;
    match string(opts, "virt_text_pos")?.as_deref() {
        None | Some("eol") | Some("overlay") | Some("right_align") | Some("eol_right_align") | Some("inline") => Ok(()),
        Some(value) => Err(ApiError::validation(format!("Invalid 'virt_text_pos': '{value}'"))),
    }
}

fn parse_highlight_mode(opts: &Dict) -> Result<(), ApiError> {
    match string(opts, "hl_mode")?.as_deref() {
        None | Some("replace") | Some("combine") | Some("blend") => Ok(()),
        Some(value) => Err(ApiError::validation(format!("Invalid 'hl_mode': '{value}'"))),
    }
}

fn parse_virtual_lines_overflow(opts: &Dict) -> Result<(), ApiError> {
    match string(opts, "virt_lines_overflow")?.as_deref() {
        None | Some("trunc") | Some("scroll") | Some("wrap") | Some("auto") => Ok(()),
        Some(value) => Err(ApiError::validation(format!("Invalid 'virt_lines_overflow': '{value}'"))),
    }
}

fn parse_highlight_groups(opts: &Dict) -> Result<Option<String>, ApiError> {
    let Some(value) = opts.get(&OxStr::from("hl_group")) else { return Ok(None); };
    match value {
        Object::String(value) => Ok(Some(String::from_utf8(value.0.clone()).map_err(|_| ApiError::validation("'hl_group' must be UTF-8"))?)),
        Object::Array(values) => {
            let mut groups = values.iter().map(|value| match value {
                Object::String(value) => String::from_utf8(value.0.clone()).map_err(|_| ApiError::validation("'hl_group' must be UTF-8")),
                _ => Err(ApiError::validation("'hl_group' must contain strings")),
            }).collect::<Result<Vec<_>, _>>()?;
            Ok((!groups.is_empty()).then(|| groups.remove(0)))
        }
        _ => Err(ApiError::validation("'hl_group' must be a string or array")),
    }
}

fn checked_position(text: &Buffer, row: i64, col: i64, strict: bool, row_key: &str, col_key: &str) -> Result<ExtmarkPosition, ApiError> {
    let line_count = text.line_count();
    let mut row = usize::try_from(row).map_err(|_| ApiError::validation(format!("Invalid '{row_key}': out of range")))?;
    if row > line_count {
        if strict { return Err(ApiError::validation(format!("Invalid '{row_key}': out of range"))); }
        row = line_count;
    }
    let line_len = if row < line_count { text.line(row + 1).map_err(|error| ApiError::exception(error.to_string()))?.len() } else { 0 };
    let column = if col == -1 {
        line_len
    } else {
        let column = usize::try_from(col).map_err(|_| ApiError::validation(format!("Invalid '{col_key}': out of range")))?;
        if strict && column > line_len { return Err(ApiError::validation(format!("Invalid '{col_key}': out of range"))); }
        column.min(line_len)
    };
    Ok(ExtmarkPosition::new(row, column))
}

fn placement(text: &Buffer, row: i64, col: i64, strict: bool, opts: &Dict) -> Result<ExtmarkPlacement, ApiError> {
    parse_virtual_text_position(opts)?;
    parse_highlight_mode(opts)?;
    parse_virtual_lines_overflow(opts)?;
    let mut placement = ExtmarkPlacement::new(checked_position(text, row, col, strict, "line", "col")?);
    placement.gravity = if boolean(opts, "right_gravity", true)? { ExtmarkGravity::Right } else { ExtmarkGravity::Left };
    let has_end_row = opts.get(&OxStr::from("end_row")).is_some();
    let has_end_line = opts.get(&OxStr::from("end_line")).is_some();
    if has_end_row && has_end_line {
        return Err(ApiError::validation("cannot use both 'end_row' and 'end_line'"));
    }
    let end_row = integer(opts, "end_row")?.or(integer(opts, "end_line")?);
    let end_col = integer(opts, "end_col")?;
    if end_row.is_some() || end_col.is_some() {
        let end_row = end_row.unwrap_or(row);
        let end_col = end_col.unwrap_or(0);
        if strict && end_col == -1 { return Err(ApiError::validation("Invalid 'end_col': out of range")); }
        placement.end = Some(ExtmarkEnd {
            position: checked_position(text, end_row, end_col, strict, "end_row", "end_col")?,
            gravity: if boolean(opts, "end_right_gravity", false)? { ExtmarkGravity::Right } else { ExtmarkGravity::Left },
        });
    } else if opts.get(&OxStr::from("end_right_gravity")).is_some() {
        return Err(ApiError::validation("cannot set end_right_gravity without end_row or end_col"));
    }
    let mut attributes = ExtmarkAttributes::default();
    attributes.highlight_group = parse_highlight_groups(opts)?;
    attributes.sign_text = string(opts, "sign_text")?;
    attributes.sign_highlight_group = string(opts, "sign_hl_group")?;
    attributes.number_highlight_group = string(opts, "number_hl_group")?;
    attributes.line_highlight_group = string(opts, "line_hl_group")?;
    attributes.cursorline_highlight_group = string(opts, "cursorline_hl_group")?;
    attributes.priority = match integer(opts, "priority")? {
        Some(value) => {
            attributes.priority_set = true;
            if (0..=i64::from(u16::MAX)).contains(&value) { value as u32 } else { return Err(ApiError::validation("Invalid 'priority': out of range")); }
        }
        None if attributes.has_sign() => 0x1000,
        None => 0,
    };
    attributes.invalidate = boolean(opts, "invalidate", false)?;
    let _ = boolean(opts, "hl_eol", false)?;
    let _ = boolean(opts, "virt_text_hide", false)?;
    let _ = boolean(opts, "virt_text_repeat_linebreak", false)?;
    let _ = boolean(opts, "virt_lines_above", false)?;
    let _ = boolean(opts, "virt_lines_leftcol", false)?;
    let _ = boolean(opts, "ephemeral", false)?;
    let _ = boolean(opts, "ui_watched", false)?;
    attributes.undo_restore = boolean(opts, "undo_restore", true)?;
    if let Some(value) = opts.get(&OxStr::from("virt_text")) { attributes.virtual_text = chunks(value)?; }
    if let Some(value) = opts.get(&OxStr::from("virt_lines")) {
        let Object::Array(lines) = value else {
            return Err(ApiError::validation(format!(
                "Invalid 'virt_lines': expected Array, got {}",
                object_type(value)
            )));
        };
        attributes.virtual_lines = lines.iter().map(chunks).collect::<Result<Vec<VirtualLine>, _>>()?;
    }
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
    let attributes = &placement.attributes;
    let mut values = vec![
        (OxStr::from("ns_id"), Object::Integer(i64::from(mark.namespace.get()))),
        (OxStr::from("right_gravity"), Object::Boolean(placement.gravity == ExtmarkGravity::Right)),
    ];
    if let Some(end) = placement.end {
        values.push((OxStr::from("end_row"), Object::Integer(i64::try_from(end.position.row).unwrap_or(i64::MAX))));
        values.push((OxStr::from("end_col"), Object::Integer(i64::try_from(end.position.column).unwrap_or(i64::MAX))));
        values.push((OxStr::from("end_right_gravity"), Object::Boolean(end.gravity == ExtmarkGravity::Right)));
    }
    if !attributes.undo_restore { values.push((OxStr::from("undo_restore"), Object::Boolean(false))); }
    if attributes.invalidate { values.push((OxStr::from("invalidate"), Object::Boolean(true))); }
    if mark.invalid { values.push((OxStr::from("invalid"), Object::Boolean(true))); }
    if let Some(group) = &attributes.highlight_group { values.push((OxStr::from("hl_group"), Object::String(OxStr::from(group.as_str())))); }
    if let Some(text) = &attributes.sign_text { values.push((OxStr::from("sign_text"), Object::String(OxStr::from(text.as_str())))); }
    if let Some(name) = &attributes.sign_name { values.push((OxStr::from("sign_name"), Object::String(OxStr::from(name.as_str())))); }
    if let Some(group) = &attributes.sign_highlight_group { values.push((OxStr::from("sign_hl_group"), Object::String(OxStr::from(group.as_str())))); }
    if let Some(group) = &attributes.number_highlight_group { values.push((OxStr::from("number_hl_group"), Object::String(OxStr::from(group.as_str())))); }
    if let Some(group) = &attributes.line_highlight_group { values.push((OxStr::from("line_hl_group"), Object::String(OxStr::from(group.as_str())))); }
    if let Some(group) = &attributes.cursorline_highlight_group { values.push((OxStr::from("cursorline_hl_group"), Object::String(OxStr::from(group.as_str())))); }
    if !attributes.virtual_text.is_empty() { values.push((OxStr::from("virt_text"), Object::Array(attributes.virtual_text.iter().map(chunk_object).collect()))); }
    if !attributes.virtual_lines.is_empty() { values.push((OxStr::from("virt_lines"), Object::Array(attributes.virtual_lines.iter().map(|line| Object::Array(line.iter().map(chunk_object).collect())).collect()))); }
    if attributes.highlight_group.is_some() || attributes.has_sign() || !attributes.virtual_text.is_empty() || !attributes.virtual_lines.is_empty() {
        values.push((OxStr::from("priority"), Object::Integer(i64::from(attributes.priority))));
    }
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
    let namespace = allocated_namespace(editor, ns_id)?;
    let requested = integer(&opts, "id")?.map(|id| {
        if id <= 0 {
            return Err(ApiError::validation("Invalid 'id': expected positive Integer"));
        }
        u32::try_from(id)
            .map_err(|_| ApiError::validation("Invalid 'id': expected positive Integer"))
            .and_then(|id| ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string())))
    }).transpose()?;
    let strict = boolean(&opts, "strict", true)?;
    let state = editor.buffer_mut(buffer).map_err(|error| ApiError::validation(error.to_string()))?;
    let text = state.text().map_err(|error| ApiError::exception(error.to_string()))?;
    let placement = placement(text, line, col, strict, &opts)?;
    state.extmarks.ensure_namespace(namespace).map_err(|error| ApiError::validation(error.to_string()))?;
    let id = state.extmarks.set(namespace, requested, placement).map_err(|error| ApiError::validation(error.to_string()))?;
    Ok(i64::from(id.get()))
}

#[api(since = 7, method)]
pub fn nvim_buf_del_extmark(editor: &mut Editor, buffer: BufHandle, ns_id: i64, id: i64) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let namespace = allocated_namespace(editor, ns_id)?;
    let id = u32::try_from(id).map_err(|_| ApiError::validation(format!("Invalid 'id': {id}"))).and_then(|id| ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string())))?;
    editor.buffer_mut(buffer).map_err(|error| ApiError::validation(error.to_string()))?.extmarks.delete(namespace, id).map_err(|error| ApiError::validation(error.to_string()))
}

#[api(since = 7, method)]
pub fn nvim_buf_get_extmark_by_id(editor: &mut Editor, buffer: BufHandle, ns_id: i64, id: i64, opts: Dict) -> Result<Vec<Object>, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let namespace = allocated_namespace(editor, ns_id)?;
    let id = u32::try_from(id).map_err(|_| ApiError::validation(format!("Invalid 'id': {id}"))).and_then(|id| ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string())))?;
    let Some(mark) = editor.buffer(buffer).map_err(|error| ApiError::validation(error.to_string()))?.extmarks.get(namespace, id).map_err(|error| ApiError::validation(error.to_string()))? else { return Ok(Vec::new()); };
    let mut result = vec![Object::Integer(i64::try_from(mark.position().row).unwrap_or(i64::MAX)), Object::Integer(i64::try_from(mark.position().column).unwrap_or(i64::MAX))];
    if boolean(&opts, "details", false)? { result.push(Object::Dict(details(mark))); }
    Ok(result)
}

fn query_type(opts: &Dict) -> Result<Option<String>, ApiError> {
    match string(opts, "type")?.as_deref() {
        None => Ok(None),
        Some("highlight") | Some("sign") | Some("virt_text") | Some("virt_lines") => Ok(string(opts, "type")?),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid 'type': expected sign, virt_text, virt_lines or highlight, got {value}"
        ))),
    }
}

fn bound(extmarks: &Extmarks, namespace: Option<NamespaceId>, value: Object) -> Result<ExtmarkPosition, ApiError> {
    match value {
        Object::Integer(0) => Ok(ExtmarkPosition::new(0, 0)),
        Object::Integer(-1) => Ok(ExtmarkPosition::new(usize::MAX, usize::MAX)),
        Object::Integer(value) if value > 0 => {
            let namespace = namespace.ok_or_else(|| ApiError::validation("Invalid mark position: expected mark id Integer or 2-item Array"))?;
            let id = u32::try_from(value).map_err(|_| ApiError::validation(format!("Invalid mark id: {value}")))?;
            let id = ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string()))?;
            let mark = match extmarks.get(namespace, id) {
                Ok(Some(mark)) => mark,
                Ok(None) | Err(_) => return Err(ApiError::validation(format!("Invalid mark id (not found): {value}"))),
            };
            Ok(mark.position())
        }
        Object::Integer(value) => Err(ApiError::validation(format!("Invalid mark id: {value}"))),
        Object::Array(values) if values.len() == 2 => match (&values[0], &values[1]) {
            (Object::Integer(-1), Object::Integer(-1)) => Ok(ExtmarkPosition::new(usize::MAX, usize::MAX)),
            (Object::Integer(row), Object::Integer(-1)) if *row >= 0 => Ok(ExtmarkPosition::new(usize::try_from(*row).map_err(|_| ApiError::validation("Invalid mark position: expected 2 Integer items"))?, usize::MAX)),
            (Object::Integer(row), Object::Integer(col)) => position(*row, *col),
            _ => Err(ApiError::validation("Invalid mark position: expected 2 Integer items")),
        },
        Object::Array(_) => Err(ApiError::validation("Invalid mark position: expected 2 Integer items")),
        _ => Err(ApiError::validation("Invalid mark position: expected mark id Integer or 2-item Array")),
    }
}

fn mark_has_type(mark: &Extmark, kind: &str) -> bool {
    let attributes = &mark.placement.attributes;
    match kind {
        "sign" => attributes.has_sign(),
        "virt_text" => !attributes.virtual_text.is_empty(),
        "virt_lines" => !attributes.virtual_lines.is_empty(),
        "highlight" => attributes.highlight_group.is_some(),
        _ => false,
    }
}

fn mark_overlaps(mark: &Extmark, lower: ExtmarkPosition, upper: ExtmarkPosition) -> bool {
    let start = mark.position();
    let end = mark.placement.end.map_or(start, |end| end.position);
    start <= upper && end >= lower
}

#[api(since = 7, method)]
pub fn nvim_buf_get_extmarks(editor: &mut Editor, buffer: BufHandle, ns_id: i64, start: Object, end: Object, opts: Dict) -> Result<Vec<Vec<Object>>, ApiError> {
    let buffer = resolve_buffer(editor, buffer)?;
    let limit = integer(&opts, "limit")?.map(|value| if value < 0 { Ok(usize::MAX) } else { usize::try_from(value).map_err(|_| ApiError::validation("Invalid 'limit': out of range")) }).transpose()?;
    let include_details = boolean(&opts, "details", false)?;
    let overlap = boolean(&opts, "overlap", false)?;
    let kind = query_type(&opts)?;
    let state = editor.buffer(buffer).map_err(|error| ApiError::validation(error.to_string()))?;
    let namespace = if ns_id == -1 { None } else { Some(allocated_namespace(editor, ns_id)?) };
    let first = bound(&state.extmarks, namespace, start)?;
    let last = bound(&state.extmarks, namespace, end)?;
    let reverse = first > last;
    let (lower, upper) = if reverse { (last, first) } else { (first, last) };
    let query_first = if overlap { ExtmarkPosition::new(0, 0) } else { first };
    let query_last = if overlap { ExtmarkPosition::new(usize::MAX, usize::MAX) } else { last };
    let query_limit = if kind.is_some() || overlap { None } else { limit };
    let mut marks = match namespace {
        None => state.extmarks.query_all(query_first, query_last, query_limit),
        Some(namespace) => state.extmarks.query(namespace, query_first, query_last, query_limit).map_err(|error| ApiError::validation(error.to_string()))?,
    };
    if overlap {
        marks.retain(|mark| mark_overlaps(mark, lower, upper));
        if reverse { marks.reverse(); }
    }
    if let Some(kind) = kind.as_deref() { marks.retain(|mark| mark_has_type(mark, kind)); }
    if let Some(limit) = limit { marks.truncate(limit); }
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
    let requested = if ns_id == -1 { None } else { Some(allocated_namespace(editor, ns_id)?) };
    let state = editor.buffer_mut(buffer).map_err(|error| ApiError::validation(error.to_string()))?;
    let namespaces = requested.map_or_else(|| state.extmarks.namespace_ids(), |namespace| vec![namespace]);
    for namespace in namespaces { let _ = state.extmarks.clear(namespace, first, last); }
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
