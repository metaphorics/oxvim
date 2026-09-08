//! Namespace and buffer extmark API.

use ox_editor::decoration::{
    BufCallbackId, ConcealLineCallbackId, DecorProviderDef, EndCallbackId, HlDefCallbackId,
    LineCallbackId, ProviderId, RangeCallbackId, SpellNavCallbackId, StartCallbackId,
    WinCallbackId,
};
use ox_editor::{
    Extmark, ExtmarkAttributes, ExtmarkEnd, ExtmarkError, ExtmarkGravity, ExtmarkHighlightMode,
    ExtmarkId, ExtmarkPlacement, ExtmarkPosition, ExtmarkVirtualLinesOverflow,
    ExtmarkVirtualTextPosition, Extmarks, NamespaceId, VirtualLine, VirtualTextChunk,
};
use ox_text::Buffer;

use crate::session::ApiSession;
use crate::{ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, api};

fn resolve_buffer(session: &ApiSession, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    session.with_editor(|editor| {
        let buffer = if buffer.is_current() {
            editor
                .current_buffer()
                .ok_or_else(|| ApiError::validation("No current buffer"))?
        } else {
            buffer
        };
        editor.buffer(buffer).map_err(|_| {
            ApiError::validation(format!("Invalid buffer id: {}", i64::from(buffer)))
        })?;
        Ok(buffer)
    })
}

fn allocated_namespace(session: &ApiSession, value: i64) -> Result<NamespaceId, ApiError> {
    let public = u32::try_from(value)
        .map_err(|_| ApiError::validation(format!("Invalid 'ns_id': {value}")))?;
    if public == 0 {
        return Err(ApiError::validation(format!("Invalid 'ns_id': {value}")));
    }
    let next = session.with_state(|state| state.next_namespace);
    if public >= next {
        return Err(ApiError::validation(format!("Invalid 'ns_id': {value}")));
    }
    NamespaceId::new(public).map_err(|error| ApiError::validation(error.to_string()))
}

fn position(row: i64, col: i64) -> Result<ExtmarkPosition, ApiError> {
    if row < 0 || col < 0 {
        return Err(ApiError::validation("row and col must be non-negative"));
    }
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
        Some(_) => Err(ApiError::validation(format!(
            "Invalid '{key}': expected boolean"
        ))),
    }
}

fn integer(opts: &Dict, key: &str) -> Result<Option<i64>, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::Integer(value)) => Ok(Some(*value)),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid '{key}': expected Integer, got {}",
            object_type(value)
        ))),
    }
}

fn string(opts: &Dict, key: &str) -> Result<Option<String>, ApiError> {
    match opts.get(&OxStr::from(key)) {
        None => Ok(None),
        Some(Object::String(value)) => String::from_utf8(value.0.clone())
            .map(Some)
            .map_err(|_| ApiError::validation(format!("'{key}' must be UTF-8"))),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid '{key}': expected String, got {}",
            object_type(value)
        ))),
    }
}

fn chunks(value: &Object) -> Result<Vec<VirtualTextChunk>, ApiError> {
    let Object::Array(items) = value else {
        return Err(ApiError::validation("virtual text must be an array"));
    };
    items
        .iter()
        .map(|item| {
            let Object::Array(parts) = item else {
                return Err(ApiError::validation("virtual text chunk must be an array"));
            };
            let Some(Object::String(text)) = parts.first() else {
                return Err(ApiError::validation(
                    "virtual text chunk text must be a string",
                ));
            };
            let text = String::from_utf8(text.0.clone())
                .map_err(|_| ApiError::validation("virtual text must be UTF-8"))?;
            let highlight_groups = match parts.get(1) {
                None | Some(Object::Nil) => Vec::new(),
                Some(Object::String(value)) => vec![
                    String::from_utf8(value.0.clone())
                        .map_err(|_| ApiError::validation("highlight group must be UTF-8"))?,
                ],
                Some(Object::Array(values)) => values
                    .iter()
                    .map(|value| match value {
                        Object::String(value) => String::from_utf8(value.0.clone())
                            .map_err(|_| ApiError::validation("highlight group must be UTF-8")),
                        _ => Err(ApiError::validation("highlight group must be a string")),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => {
                    return Err(ApiError::validation(
                        "highlight group must be a string or array",
                    ));
                }
            };
            Ok(VirtualTextChunk {
                text,
                highlight_groups,
            })
        })
        .collect()
}

fn parse_virtual_text_position(opts: &Dict) -> Result<(), ApiError> {
    let _ = integer(opts, "virt_text_win_col")?;
    match string(opts, "virt_text_pos")?.as_deref() {
        None | Some("eol" | "overlay" | "right_align" | "eol_right_align" | "inline") => Ok(()),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid 'virt_text_pos': '{value}'"
        ))),
    }
}

fn parse_highlight_mode(opts: &Dict) -> Result<(), ApiError> {
    match string(opts, "hl_mode")?.as_deref() {
        None | Some("replace" | "combine" | "blend") => Ok(()),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid 'hl_mode': '{value}'"
        ))),
    }
}

fn parse_virtual_lines_overflow(opts: &Dict) -> Result<(), ApiError> {
    match string(opts, "virt_lines_overflow")?.as_deref() {
        None | Some("trunc" | "scroll" | "wrap" | "auto") => Ok(()),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid 'virt_lines_overflow': '{value}'"
        ))),
    }
}

fn parse_highlight_groups(opts: &Dict) -> Result<(Option<String>, Vec<String>), ApiError> {
    let Some(value) = opts.get(&OxStr::from("hl_group")) else {
        return Ok((None, Vec::new()));
    };
    match value {
        Object::String(value) => Ok((
            Some(
                String::from_utf8(value.0.clone())
                    .map_err(|_| ApiError::validation("'hl_group' must be UTF-8"))?,
            ),
            Vec::new(),
        )),
        Object::Array(values) => {
            let mut groups = values
                .iter()
                .map(|value| match value {
                    Object::String(value) => String::from_utf8(value.0.clone())
                        .map_err(|_| ApiError::validation("'hl_group' must be UTF-8")),
                    _ => Err(ApiError::validation("'hl_group' must contain strings")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Extra groups stack after the first (highest-priority last),
            // mirroring the `has_hl_multiple` decor entries upstream
            // (api/extmark.c:876-889).
            let first = if groups.is_empty() {
                None
            } else {
                Some(groups.remove(0))
            };
            Ok((first, groups))
        }
        _ => Err(ApiError::validation("'hl_group' must be a string or array")),
    }
}

fn checked_position(
    text: &Buffer,
    row: i64,
    col: i64,
    strict: bool,
    row_key: &str,
    col_key: &str,
) -> Result<ExtmarkPosition, ApiError> {
    let line_count = text.line_count();
    let mut row = usize::try_from(row)
        .map_err(|_| ApiError::validation(format!("Invalid '{row_key}': out of range")))?;
    if row > line_count {
        if strict {
            return Err(ApiError::validation(format!(
                "Invalid '{row_key}': out of range"
            )));
        }
        row = line_count;
    }
    let line_len = if row < line_count {
        text.line(row + 1)
            .map_err(|error| ApiError::exception(error.to_string()))?
            .len()
    } else {
        0
    };
    let column = if col == -1 {
        line_len
    } else {
        let column = usize::try_from(col)
            .map_err(|_| ApiError::validation(format!("Invalid '{col_key}': out of range")))?;
        if strict && column > line_len {
            return Err(ApiError::validation(format!(
                "Invalid '{col_key}': out of range"
            )));
        }
        column.min(line_len)
    };
    Ok(ExtmarkPosition::new(row, column))
}

fn placement(
    text: &Buffer,
    row: i64,
    col: i64,
    strict: bool,
    opts: &Dict,
) -> Result<ExtmarkPlacement, ApiError> {
    parse_virtual_text_position(opts)?;
    parse_highlight_mode(opts)?;
    parse_virtual_lines_overflow(opts)?;
    let mut placement =
        ExtmarkPlacement::new(checked_position(text, row, col, strict, "line", "col")?);
    placement.gravity = if boolean(opts, "right_gravity", true)? {
        ExtmarkGravity::Right
    } else {
        ExtmarkGravity::Left
    };
    let has_end_row = opts.get(&OxStr::from("end_row")).is_some();
    let has_end_line = opts.get(&OxStr::from("end_line")).is_some();
    if has_end_row && has_end_line {
        return Err(ApiError::validation(
            "cannot use both 'end_row' and 'end_line'",
        ));
    }
    let end_row = integer(opts, "end_row")?.or(integer(opts, "end_line")?);
    let end_col = integer(opts, "end_col")?;
    if end_row.is_some() || end_col.is_some() {
        let end_row = end_row.unwrap_or(row);
        let end_col = end_col.unwrap_or(0);
        if strict && end_col == -1 {
            return Err(ApiError::validation("Invalid 'end_col': out of range"));
        }
        placement.end = Some(ExtmarkEnd {
            position: checked_position(text, end_row, end_col, strict, "end_row", "end_col")?,
            gravity: if boolean(opts, "end_right_gravity", false)? {
                ExtmarkGravity::Right
            } else {
                ExtmarkGravity::Left
            },
        });
    } else if opts.get(&OxStr::from("end_right_gravity")).is_some() {
        return Err(ApiError::validation(
            "cannot set end_right_gravity without end_row or end_col",
        ));
    }
    placement.attributes = parse_extmark_attributes(opts)?;
    Ok(placement)
}

fn parse_extmark_attributes(opts: &Dict) -> Result<ExtmarkAttributes, ApiError> {
    let (highlight_group, additional_highlight_groups) = parse_highlight_groups(opts)?;
    let mut attributes = ExtmarkAttributes {
        highlight_group,
        additional_highlight_groups,
        sign_text: string(opts, "sign_text")?,
        sign_highlight_group: string(opts, "sign_hl_group")?,
        number_highlight_group: string(opts, "number_hl_group")?,
        line_highlight_group: string(opts, "line_hl_group")?,
        cursorline_highlight_group: string(opts, "cursorline_hl_group")?,
        conceal: string(opts, "conceal")?,
        conceal_lines: string(opts, "conceal_lines")?,
        url: string(opts, "url")?,
        ..Default::default()
    };
    if let Some(Object::Boolean(value)) = opts.get(&OxStr::from("spell")) {
        attributes.spell = Some(*value);
    } else if opts.get(&OxStr::from("spell")).is_some() {
        return Err(ApiError::validation("Invalid 'spell': expected boolean"));
    }
    attributes.priority = match integer(opts, "priority")? {
        Some(value) => {
            let Ok(priority) = u16::try_from(value) else {
                return Err(ApiError::validation("Invalid 'priority': out of range"));
            };
            attributes
                .flags
                .set(ox_editor::ExtmarkFlags::PRIORITY_SET, true);
            u32::from(priority)
        }
        None if attributes.has_sign() => 0x1000,
        None => 0,
    };
    attributes.flags.set(
        ox_editor::ExtmarkFlags::INVALIDATE,
        boolean(opts, "invalidate", false)?,
    );
    attributes.flags.set(
        ox_editor::ExtmarkFlags::HIGHLIGHT_EOL,
        boolean(opts, "hl_eol", false)?,
    );
    attributes.virt_text_hide = boolean(opts, "virt_text_hide", false)?;
    attributes.virt_text_repeat_linebreak = boolean(opts, "virt_text_repeat_linebreak", false)?;
    attributes.virt_lines_above = boolean(opts, "virt_lines_above", false)?;
    attributes.virt_lines_leftcol = boolean(opts, "virt_lines_leftcol", false)?;
    let _ = boolean(opts, "ephemeral", false)?;
    attributes.flags.set(
        ox_editor::ExtmarkFlags::UI_WATCHED,
        boolean(opts, "ui_watched", false)?,
    );
    attributes.flags.set(
        ox_editor::ExtmarkFlags::UNDO_RESTORE,
        boolean(opts, "undo_restore", true)?,
    );
    parse_virtual_text_config(&mut attributes, opts)?;
    Ok(attributes)
}

fn parse_virtual_text_config(
    attributes: &mut ExtmarkAttributes,
    opts: &Dict,
) -> Result<(), ApiError> {
    // Parse virt_text_pos string into the enum.
    attributes.virtual_text_position = match string(opts, "virt_text_pos")?.as_deref() {
        Some("overlay") => ExtmarkVirtualTextPosition::Overlay,
        Some("right_align") => ExtmarkVirtualTextPosition::RightAlign,
        Some("eol_right_align") => ExtmarkVirtualTextPosition::EndOfLineRightAlign,
        Some("inline") => ExtmarkVirtualTextPosition::Inline,
        // Validation already performed by `parse_virtual_text_position`.
        None | Some(_) => ExtmarkVirtualTextPosition::EndOfLine,
    };
    // `virt_text_win_col` overrides the position to a fixed window column.
    if let Some(win_col) = integer(opts, "virt_text_win_col")? {
        if win_col < 0 {
            return Err(ApiError::validation(
                "Invalid 'virt_text_win_col': out of range",
            ));
        }
        let win_col = usize::try_from(win_col)
            .map_err(|_| ApiError::validation("Invalid 'virt_text_win_col': out of range"))?;
        attributes.virt_text_win_col = Some(win_col);
        attributes.virtual_text_position = ExtmarkVirtualTextPosition::WindowColumn(win_col);
    }
    // Parse hl_mode string into the enum.
    attributes.highlight_mode = match string(opts, "hl_mode")?.as_deref() {
        Some("replace") => Some(ExtmarkHighlightMode::Replace),
        Some("combine") => Some(ExtmarkHighlightMode::Combine),
        Some("blend") => Some(ExtmarkHighlightMode::Blend),
        // Validation already performed by `parse_highlight_mode`.
        None | Some(_) => None,
    };
    // Parse virt_lines_overflow string into the enum.
    attributes.virt_lines_overflow = match string(opts, "virt_lines_overflow")?.as_deref() {
        Some("scroll") => ExtmarkVirtualLinesOverflow::Scroll,
        Some("wrap") => ExtmarkVirtualLinesOverflow::Wrap,
        Some("auto") => ExtmarkVirtualLinesOverflow::Auto,
        // Validation already performed by `parse_virtual_lines_overflow`.
        None | Some(_) => ExtmarkVirtualLinesOverflow::Trunc,
    };
    if let Some(value) = opts.get(&OxStr::from("virt_text")) {
        attributes.virtual_text = chunks(value)?;
    }
    if let Some(value) = opts.get(&OxStr::from("virt_lines")) {
        let Object::Array(lines) = value else {
            return Err(ApiError::validation(format!(
                "Invalid 'virt_lines': expected Array, got {}",
                object_type(value)
            )));
        };
        attributes.virtual_lines = lines
            .iter()
            .map(chunks)
            .collect::<Result<Vec<VirtualLine>, _>>()?;
    }
    Ok(())
}

fn chunk_object(chunk: &VirtualTextChunk) -> Object {
    let mut values = vec![Object::String(OxStr::from(chunk.text.as_str()))];
    if chunk.highlight_groups.len() == 1 {
        values.push(Object::String(OxStr::from(
            chunk.highlight_groups[0].as_str(),
        )));
    } else if !chunk.highlight_groups.is_empty() {
        values.push(Object::Array(
            chunk
                .highlight_groups
                .iter()
                .map(|value| Object::String(OxStr::from(value.as_str())))
                .collect(),
        ));
    }
    Object::Array(values)
}

fn virt_text_pos_string(pos: ExtmarkVirtualTextPosition) -> &'static str {
    match pos {
        ExtmarkVirtualTextPosition::EndOfLine => "eol",
        ExtmarkVirtualTextPosition::Overlay => "overlay",
        ExtmarkVirtualTextPosition::RightAlign => "right_align",
        ExtmarkVirtualTextPosition::EndOfLineRightAlign => "eol_right_align",
        ExtmarkVirtualTextPosition::Inline => "inline",
        ExtmarkVirtualTextPosition::WindowColumn(_) => "win_col",
    }
}

fn hl_mode_string(mode: ExtmarkHighlightMode) -> &'static str {
    match mode {
        ExtmarkHighlightMode::Replace => "replace",
        ExtmarkHighlightMode::Combine => "combine",
        ExtmarkHighlightMode::Blend => "blend",
    }
}

fn virt_lines_overflow_string(overflow: ExtmarkVirtualLinesOverflow) -> &'static str {
    match overflow {
        ExtmarkVirtualLinesOverflow::Trunc => "trunc",
        ExtmarkVirtualLinesOverflow::Scroll => "scroll",
        ExtmarkVirtualLinesOverflow::Wrap => "wrap",
        ExtmarkVirtualLinesOverflow::Auto => "auto",
    }
}

fn details(mark: &Extmark) -> Dict {
    let placement = &mark.placement;
    let attributes = &placement.attributes;
    let mut values = vec![
        (
            OxStr::from("ns_id"),
            Object::Integer(i64::from(mark.namespace.get())),
        ),
        (
            OxStr::from("right_gravity"),
            Object::Boolean(placement.gravity == ExtmarkGravity::Right),
        ),
    ];
    if let Some(end) = placement.end {
        values.push((
            OxStr::from("end_row"),
            Object::Integer(i64::try_from(end.position.row).unwrap_or(i64::MAX)),
        ));
        values.push((
            OxStr::from("end_col"),
            Object::Integer(i64::try_from(end.position.column).unwrap_or(i64::MAX)),
        ));
        values.push((
            OxStr::from("end_right_gravity"),
            Object::Boolean(end.gravity == ExtmarkGravity::Right),
        ));
    }
    if !attributes
        .flags
        .contains(ox_editor::ExtmarkFlags::UNDO_RESTORE)
    {
        values.push((OxStr::from("undo_restore"), Object::Boolean(false)));
    }
    if attributes
        .flags
        .contains(ox_editor::ExtmarkFlags::INVALIDATE)
    {
        values.push((OxStr::from("invalidate"), Object::Boolean(true)));
    }
    if mark.invalid {
        values.push((OxStr::from("invalid"), Object::Boolean(true)));
    }
    push_string_attributes(&mut values, attributes);
    if let Some(spell) = attributes.spell {
        values.push((OxStr::from("spell"), Object::Boolean(spell)));
    }
    if attributes
        .flags
        .contains(ox_editor::ExtmarkFlags::HIGHLIGHT_EOL)
    {
        values.push((OxStr::from("hl_eol"), Object::Boolean(true)));
    }
    if let Some(mode) = attributes.highlight_mode {
        values.push((
            OxStr::from("hl_mode"),
            Object::String(OxStr::from(hl_mode_string(mode))),
        ));
    }
    if attributes
        .flags
        .contains(ox_editor::ExtmarkFlags::UI_WATCHED)
    {
        values.push((OxStr::from("ui_watched"), Object::Boolean(true)));
    }
    push_virtual_text_details(&mut values, attributes);
    push_virtual_lines_details(&mut values, attributes);
    if attributes.highlight_group.is_some()
        || attributes.has_sign()
        || !attributes.virtual_text.is_empty()
        || !attributes.virtual_lines.is_empty()
        || attributes
            .flags
            .contains(ox_editor::ExtmarkFlags::PRIORITY_SET)
    {
        values.push((
            OxStr::from("priority"),
            Object::Integer(i64::from(attributes.priority)),
        ));
    }
    Dict(values)
}

fn push_string_attributes(values: &mut Vec<(OxStr, Object)>, attributes: &ExtmarkAttributes) {
    for (key, value) in [
        ("hl_group", &attributes.highlight_group),
        ("sign_text", &attributes.sign_text),
        ("sign_name", &attributes.sign_name),
        ("sign_hl_group", &attributes.sign_highlight_group),
        ("number_hl_group", &attributes.number_highlight_group),
        ("line_hl_group", &attributes.line_highlight_group),
        (
            "cursorline_hl_group",
            &attributes.cursorline_highlight_group,
        ),
        ("conceal", &attributes.conceal),
        ("conceal_lines", &attributes.conceal_lines),
        ("url", &attributes.url),
    ] {
        if let Some(value) = value {
            values.push((
                OxStr::from(key),
                Object::String(OxStr::from(value.as_str())),
            ));
        }
    }
}

fn push_virtual_text_details(values: &mut Vec<(OxStr, Object)>, attributes: &ExtmarkAttributes) {
    values.push((
        OxStr::from("virt_text"),
        Object::Array(attributes.virtual_text.iter().map(chunk_object).collect()),
    ));
    values.push((
        OxStr::from("virt_text_pos"),
        Object::String(OxStr::from(virt_text_pos_string(
            attributes.virtual_text_position,
        ))),
    ));
    values.push((
        OxStr::from("virt_text_hide"),
        Object::Boolean(attributes.virt_text_hide),
    ));
    values.push((
        OxStr::from("virt_text_repeat_linebreak"),
        Object::Boolean(attributes.virt_text_repeat_linebreak),
    ));
    if let Some(win_col) = attributes.virt_text_win_col {
        values.push((
            OxStr::from("virt_text_win_col"),
            Object::Integer(i64::try_from(win_col).unwrap_or(i64::MAX)),
        ));
    }
}

fn push_virtual_lines_details(values: &mut Vec<(OxStr, Object)>, attributes: &ExtmarkAttributes) {
    values.push((
        OxStr::from("virt_lines"),
        Object::Array(
            attributes
                .virtual_lines
                .iter()
                .map(|line| Object::Array(line.iter().map(chunk_object).collect()))
                .collect(),
        ),
    ));
    values.push((
        OxStr::from("virt_lines_above"),
        Object::Boolean(attributes.virt_lines_above),
    ));
    values.push((
        OxStr::from("virt_lines_leftcol"),
        Object::Boolean(attributes.virt_lines_leftcol),
    ));
    values.push((
        OxStr::from("virt_lines_overflow"),
        Object::String(OxStr::from(virt_lines_overflow_string(
            attributes.virt_lines_overflow,
        ))),
    ));
}

#[api(since = 5)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "RPC dispatch requires the `Result` return shape"
)]
pub fn nvim_create_namespace(session: &ApiSession, name: OxStr) -> Result<i64, ApiError> {
    if !name.0.is_empty()
        && let Some(id) = session.with_state(|state| state.namespaces.get(&name).copied())
    {
        return Ok(i64::from(id));
    }
    let id = session.with_state_mut(|state| {
        let id = state.next_namespace;
        state.next_namespace = state.next_namespace.saturating_add(1);
        if !name.0.is_empty() {
            state.namespaces.insert(name, id);
        }
        id
    });
    Ok(i64::from(id))
}

/// Which `DecorProviderDef` field a provider key fills.
///
/// The six `on_*` keys feed redraw lifecycle phases; the `_on_*` keys are
/// stored for parity with upstream (`extmark.c:1082-1084`) but have no
/// dispatch site yet — their invocation lands with the corresponding redraw
/// events.
#[derive(Clone, Copy)]
enum ProviderSlot {
    Start,
    Buf,
    Win,
    Line,
    Range,
    End,
    HlDef,
    SpellNav,
    ConcealLine,
}

/// The callback keys `nvim_set_decoration_provider` accepts and the slot each
/// fills, in upstream's `cbs[]` table order (`extmark.c:1075-1085`).
const PROVIDER_KEYS: &[(&str, ProviderSlot)] = &[
    ("on_start", ProviderSlot::Start),
    ("on_buf", ProviderSlot::Buf),
    ("on_win", ProviderSlot::Win),
    ("on_line", ProviderSlot::Line),
    ("on_range", ProviderSlot::Range),
    ("on_end", ProviderSlot::End),
    ("_on_hl_def", ProviderSlot::HlDef),
    ("_on_spell_nav", ProviderSlot::SpellNav),
    ("_on_conceal_line", ProviderSlot::ConcealLine),
];

/// One parsed provider entry: the slot it fills and its Lua registry ref.
struct ParsedCallback {
    slot: ProviderSlot,
    reference: u32,
}

/// Validates every entry of `opts` without touching the live provider.
///
/// Returns the incoming Lua refs so a later failure can release them all.
/// Only keys in `PROVIDER_KEYS` are accepted and every value must be a Lua
/// function reference; anything else is rejected rather than stored and
/// silently ignored.
fn parse_provider_callbacks(opts: &Dict) -> Result<Vec<ParsedCallback>, ApiError> {
    let mut parsed = Vec::new();
    for (key, value) in opts.iter() {
        let key = key.to_string_lossy().into_owned();
        let Some(&(_, slot)) = PROVIDER_KEYS.iter().find(|(name, _)| *name == key.as_str()) else {
            return Err(ApiError::validation(format!("unexpected key: {key}")));
        };
        let Object::LuaRef(reference) = value else {
            return Err(ApiError::validation(format!(
                "Invalid value for '{key}': expected Lua function reference"
            )));
        };
        let positive = u32::try_from(*reference)
            .map_err(|_| ApiError::validation("Invalid Lua callback reference"))?;
        parsed.push(ParsedCallback {
            slot,
            reference: positive,
        });
    }
    Ok(parsed)
}

fn callback_ids(parsed: &[ParsedCallback]) -> DecorProviderDef {
    let mut def = DecorProviderDef::default();
    for entry in parsed {
        let reference = u64::from(entry.reference);
        match entry.slot {
            ProviderSlot::Start => def.start = Some(StartCallbackId::new(reference)),
            ProviderSlot::Buf => def.buf = Some(BufCallbackId::new(reference)),
            ProviderSlot::Win => def.win = Some(WinCallbackId::new(reference)),
            ProviderSlot::Line => def.line = Some(LineCallbackId::new(reference)),
            ProviderSlot::Range => def.range = Some(RangeCallbackId::new(reference)),
            ProviderSlot::End => def.end = Some(EndCallbackId::new(reference)),
            ProviderSlot::HlDef => def.hl_def = Some(HlDefCallbackId::new(reference)),
            ProviderSlot::SpellNav => def.spell_nav = Some(SpellNavCallbackId::new(reference)),
            ProviderSlot::ConcealLine => {
                def.conceal_line = Some(ConcealLineCallbackId::new(reference));
            }
        }
    }
    def
}

fn def_references(def: &DecorProviderDef) -> Vec<u64> {
    [
        def.start.map(|cb| cb.get()),
        def.buf.map(|cb| cb.get()),
        def.win.map(|cb| cb.get()),
        def.line.map(|cb| cb.get()),
        def.range.map(|cb| cb.get()),
        def.end.map(|cb| cb.get()),
        def.hl_def.map(|cb| cb.get()),
        def.spell_nav.map(|cb| cb.get()),
        def.conceal_line.map(|cb| cb.get()),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn release_def(session: &ApiSession, def: &DecorProviderDef) {
    for reference in def_references(def) {
        crate::runtime::release_lua_callback(session, usize::try_from(reference).unwrap_or(0));
    }
}

/// Releases every Lua reference the incoming options dictionary owns.
///
/// The Lua bridge acquires one fresh registry slot per top-level function
/// value while converting arguments and frees only its result references, so
/// until a definition is installed the call itself owns the dictionary's
/// references and must release them on failure.
fn release_incoming_references(session: &ApiSession, opts: &Dict) {
    for (_, value) in opts.iter() {
        if let Object::LuaRef(reference) = value {
            crate::runtime::release_lua_callback(session, usize::try_from(*reference).unwrap_or(0));
        }
    }
}

/// `nvim_set_decoration_provider` (`extmark.c:1061-1101`): installs the
/// namespace-owned provider callbacks used during redraws.
///
/// An empty `opts` clears the provider while keeping its stable registration
/// slot, matching upstream `decor_provider_clear`. Every incoming ref is
/// validated before the provider changes; any failure before the new
/// definition is installed releases every incoming ref at the single cleanup
/// point in [`nvim_set_decoration_provider`] and leaves the old definition
/// live. On success the new definition is installed atomically and every ref
/// from the previous definition is released exactly once; a release error is
/// reported only after all releases are attempted, and never rolls the new
/// definition back (rolling back would double-own or leak the new refs).
#[api(since = 7)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded arguments"
)]
pub fn nvim_set_decoration_provider(
    session: &ApiSession,
    ns_id: i64,
    opts: Dict,
) -> Result<(), ApiError> {
    let outcome = install_provider_callbacks(session, ns_id, &opts);
    if outcome.is_err() {
        release_incoming_references(session, &opts);
    }
    outcome
}

fn install_provider_callbacks(
    session: &ApiSession,
    ns_id: i64,
    opts: &Dict,
) -> Result<(), ApiError> {
    let namespace = allocated_namespace(session, ns_id)?;
    let provider = ProviderId::from_namespace(namespace);
    let incoming = parse_provider_callbacks(opts)?;
    let old = if incoming.is_empty() {
        session.with_editor_mut(|editor| editor.decorations_mut().clear_provider(provider))
    } else {
        session
            .with_editor_mut(|editor| {
                editor
                    .decorations_mut()
                    .replace_provider(provider, callback_ids(&incoming))
            })
            .map_err(|error| ApiError::exception(error.to_string()))?
    };
    if let Some(previous) = &old {
        release_def(session, previous);
    }
    Ok(())
}

#[api(since = 5)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "RPC dispatch requires the `Result` return shape"
)]
pub fn nvim_get_namespaces(session: &ApiSession) -> Result<Dict, ApiError> {
    Ok(session.with_state(|state| {
        Dict(
            state
                .namespaces
                .iter()
                .map(|(name, id)| (name.clone(), Object::Integer(i64::from(*id))))
                .collect(),
        )
    }))
}

#[api(since = 7, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded arguments"
)]
pub fn nvim_buf_set_extmark(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    line: i64,
    col: i64,
    opts: Dict,
) -> Result<i64, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let namespace = allocated_namespace(session, ns_id)?;
    let requested = integer(&opts, "id")?
        .map(|id| {
            if id <= 0 {
                return Err(ApiError::validation(
                    "Invalid 'id': expected positive Integer",
                ));
            }
            u32::try_from(id)
                .map_err(|_| ApiError::validation("Invalid 'id': expected positive Integer"))
                .and_then(|id| {
                    ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string()))
                })
        })
        .transpose()?;
    let strict = boolean(&opts, "strict", true)?;
    session.with_editor_mut(|editor| {
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let text = state
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let placement = placement(text, line, col, strict, &opts)?;
        state
            .extmarks
            .ensure_namespace(namespace)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let id = state
            .extmarks
            .set(namespace, requested, placement)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        Ok(i64::from(id.get()))
    })
}

#[api(since = 7, method)]
pub fn nvim_buf_del_extmark(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    id: i64,
) -> Result<bool, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let namespace = allocated_namespace(session, ns_id)?;
    let id = u32::try_from(id)
        .map_err(|_| ApiError::validation(format!("Invalid 'id': {id}")))
        .and_then(|id| {
            ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string()))
        })?;
    session.with_editor_mut(|editor| {
        editor
            .buffer_mut(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .extmarks
            .delete(namespace, id)
            .map_err(|error| ApiError::validation(error.to_string()))
    })
}

#[api(since = 7, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded arguments"
)]
pub fn nvim_buf_get_extmark_by_id(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    id: i64,
    opts: Dict,
) -> Result<Vec<Object>, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let namespace = allocated_namespace(session, ns_id)?;
    let id = u32::try_from(id)
        .map_err(|_| ApiError::validation(format!("Invalid 'id': {id}")))
        .and_then(|id| {
            ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string()))
        })?;
    session.with_editor(|editor| {
        let Some(mark) = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .extmarks
            .get(namespace, id)
            .map_err(|error| ApiError::validation(error.to_string()))?
        else {
            return Ok(Vec::new());
        };
        let mut result = vec![
            Object::Integer(i64::try_from(mark.position().row).unwrap_or(i64::MAX)),
            Object::Integer(i64::try_from(mark.position().column).unwrap_or(i64::MAX)),
        ];
        if boolean(&opts, "details", false)? {
            result.push(Object::Dict(details(mark)));
        }
        Ok(result)
    })
}

fn query_type(opts: &Dict) -> Result<Option<String>, ApiError> {
    match string(opts, "type")?.as_deref() {
        None => Ok(None),
        Some("highlight" | "sign" | "virt_text" | "virt_lines") => Ok(string(opts, "type")?),
        Some(value) => Err(ApiError::validation(format!(
            "Invalid 'type': expected sign, virt_text, virt_lines or highlight, got {value}"
        ))),
    }
}

fn bound(
    extmarks: &Extmarks,
    namespace: Option<NamespaceId>,
    value: Object,
) -> Result<ExtmarkPosition, ApiError> {
    match value {
        Object::Integer(0) => Ok(ExtmarkPosition::new(0, 0)),
        Object::Integer(-1) => Ok(ExtmarkPosition::new(usize::MAX, usize::MAX)),
        Object::Integer(value) if value > 0 => {
            let namespace = namespace.ok_or_else(|| {
                ApiError::validation(
                    "Invalid mark position: expected mark id Integer or 2-item Array",
                )
            })?;
            let id = u32::try_from(value)
                .map_err(|_| ApiError::validation(format!("Invalid mark id: {value}")))?;
            let id = ExtmarkId::new(id).map_err(|error| ApiError::validation(error.to_string()))?;
            let Ok(Some(mark)) = extmarks.get(namespace, id) else {
                return Err(ApiError::validation(format!(
                    "Invalid mark id (not found): {value}"
                )));
            };
            Ok(mark.position())
        }
        Object::Integer(value) => Err(ApiError::validation(format!("Invalid mark id: {value}"))),
        Object::Array(values) if values.len() == 2 => match (&values[0], &values[1]) {
            (Object::Integer(-1), Object::Integer(-1)) => {
                Ok(ExtmarkPosition::new(usize::MAX, usize::MAX))
            }
            (Object::Integer(row), Object::Integer(-1)) if *row >= 0 => Ok(ExtmarkPosition::new(
                usize::try_from(*row).map_err(|_| {
                    ApiError::validation("Invalid mark position: expected 2 Integer items")
                })?,
                usize::MAX,
            )),
            (Object::Integer(row), Object::Integer(col)) => position(*row, *col),
            _ => Err(ApiError::validation(
                "Invalid mark position: expected 2 Integer items",
            )),
        },
        Object::Array(_) => Err(ApiError::validation(
            "Invalid mark position: expected 2 Integer items",
        )),
        _ => Err(ApiError::validation(
            "Invalid mark position: expected mark id Integer or 2-item Array",
        )),
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
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded arguments"
)]
pub fn nvim_buf_get_extmarks(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    start: Object,
    end: Object,
    opts: Dict,
) -> Result<Vec<Vec<Object>>, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    let limit = integer(&opts, "limit")?
        .map(|value| {
            if value < 0 {
                Ok(usize::MAX)
            } else {
                usize::try_from(value)
                    .map_err(|_| ApiError::validation("Invalid 'limit': out of range"))
            }
        })
        .transpose()?;
    let include_details = boolean(&opts, "details", false)?;
    let overlap = boolean(&opts, "overlap", false)?;
    let kind = query_type(&opts)?;
    session.with_editor(|editor| {
        let state = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let namespace = if ns_id == -1 {
            None
        } else {
            Some(allocated_namespace(session, ns_id)?)
        };
        let first = bound(&state.extmarks, namespace, start)?;
        let last = bound(&state.extmarks, namespace, end)?;
        let reverse = first > last;
        let (lower, upper) = if reverse {
            (last, first)
        } else {
            (first, last)
        };
        let query_first = if overlap {
            ExtmarkPosition::new(0, 0)
        } else {
            first
        };
        let query_last = if overlap {
            ExtmarkPosition::new(usize::MAX, usize::MAX)
        } else {
            last
        };
        let query_limit = if kind.is_some() || overlap {
            None
        } else {
            limit
        };
        let mut marks = match namespace {
            None => state
                .extmarks
                .query_all(query_first, query_last, query_limit),
            // A globally allocated namespace never used on this buffer reads
            // as empty upstream; only truly unallocated ids fail above.
            Some(namespace) => {
                match state
                    .extmarks
                    .query(namespace, query_first, query_last, query_limit)
                {
                    Ok(marks) => marks,
                    Err(ExtmarkError::UnknownNamespace(_)) => Vec::new(),
                    Err(error) => return Err(ApiError::validation(error.to_string())),
                }
            }
        };
        if overlap {
            marks.retain(|mark| mark_overlaps(mark, lower, upper));
            if reverse {
                marks.reverse();
            }
        }
        if let Some(kind) = kind.as_deref() {
            marks.retain(|mark| mark_has_type(mark, kind));
        }
        if let Some(limit) = limit {
            marks.truncate(limit);
        }
        Ok(marks
            .into_iter()
            .map(|mark| {
                let mut row = vec![
                    Object::Integer(i64::from(mark.id.get())),
                    Object::Integer(i64::try_from(mark.position().row).unwrap_or(i64::MAX)),
                    Object::Integer(i64::try_from(mark.position().column).unwrap_or(i64::MAX)),
                ];
                if include_details {
                    row.push(Object::Dict(details(&mark)));
                }
                row
            })
            .collect())
    })
}

#[api(since = 5, method)]
pub fn nvim_buf_clear_namespace(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    line_start: i64,
    line_end: i64,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    if line_start < 0 || line_end < -1 {
        return Err(ApiError::validation("line range must be non-negative"));
    }
    if line_end != -1 && line_end <= line_start {
        return Ok(());
    }
    let first = ExtmarkPosition::new(
        usize::try_from(line_start).map_err(|_| ApiError::validation("line_start out of range"))?,
        0,
    );
    let last = if line_end == -1 {
        ExtmarkPosition::new(usize::MAX, usize::MAX)
    } else {
        ExtmarkPosition::new(
            usize::try_from(line_end.saturating_sub(1))
                .map_err(|_| ApiError::validation("line_end out of range"))?,
            usize::MAX,
        )
    };
    let requested = if ns_id == -1 {
        None
    } else {
        Some(allocated_namespace(session, ns_id)?)
    };
    session.with_editor_mut(|editor| {
        let state = editor
            .buffer_mut(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?;
        let namespaces = requested.map_or_else(
            || state.extmarks.namespace_ids(),
            |namespace| vec![namespace],
        );
        for namespace in namespaces {
            let _ = state.extmarks.clear(namespace, first, last);
        }
        Ok(())
    })
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(
        nvim_create_namespace__API_META(),
        nvim_create_namespace__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_namespaces__API_META(),
        nvim_get_namespaces__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_set_extmark__API_META(),
        nvim_buf_set_extmark__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_del_extmark__API_META(),
        nvim_buf_del_extmark__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_extmark_by_id__API_META(),
        nvim_buf_get_extmark_by_id__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_extmarks__API_META(),
        nvim_buf_get_extmarks__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_clear_namespace__API_META(),
        nvim_buf_clear_namespace__API_DISPATCH,
    )?;
    registry.register(
        nvim_set_decoration_provider__API_META(),
        nvim_set_decoration_provider__API_DISPATCH,
    )?;
    Ok(())
}
