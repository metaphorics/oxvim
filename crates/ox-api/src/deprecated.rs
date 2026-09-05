//! Deprecated API compatibility entries from `api/deprecated.c`.

use ox_editor::{Editor, HighlightDefinition, Message, MessageKind};

use crate::{
    ApiError, ApiSession, BufHandle, Dict, Object, OxStr, Registry, RegistryError, TabHandle,
    WinHandle, api,
};

#[api(since = 7, deprecated_since = 11)]
#[expect(
    unused_variables,
    reason = "deprecated RPC entry keeps the upstream `output` argument; dispatch passes it positionally"
)]
pub fn nvim_exec(session: &ApiSession, src: OxStr, output: bool) -> Result<OxStr, ApiError> {
    crate::global::nvim_command(session, src)?;
    Ok(OxStr::from(""))
}

#[api(since = 1, deprecated_since = 7)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_command_output(session: &ApiSession, command: OxStr) -> Result<OxStr, ApiError> {
    crate::global::command_output(session, &command)
}

#[api(since = 0, deprecated_since = 1)]
pub fn vim_eval(session: &ApiSession, expr: OxStr) -> Result<Object, ApiError> {
    crate::global::nvim_eval(session, expr)
}

#[api(since = 3, deprecated_since = 7)]
#[expect(
    unused_variables,
    clippy::needless_pass_by_value,
    reason = "deprecated RPC entry keeps the upstream `code` and `args` arguments; dispatch passes them positionally"
)]
pub fn nvim_execute_lua(
    _session: &ApiSession,
    code: OxStr,
    args: Vec<Object>,
) -> Result<Object, ApiError> {
    Err(ApiError::exception(
        "Lua execution requires an attached Lua host",
    ))
}

#[api(since = 1, deprecated_since = 2, method)]
pub fn nvim_buf_get_number(session: &ApiSession, buffer: BufHandle) -> Result<i64, ApiError> {
    let buffer: Result<BufHandle, ApiError> = session.with_editor(|editor| {
        if !buffer.is_current() {
            editor
                .buffer(buffer)
                .map_err(|error| ApiError::exception(error.to_string()))?;
            return Ok(buffer);
        }
        let buffer = editor
            .current_buffer()
            .ok_or_else(|| ApiError::validation("No current buffer"))?;
        editor
            .buffer(buffer)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        Ok(buffer)
    });
    buffer.map(i64::from)
}

#[api(since = 1, deprecated_since = 7, method)]
pub fn nvim_buf_clear_highlight(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    line_start: i64,
    line_end: i64,
) -> Result<(), ApiError> {
    crate::extmark::nvim_buf_clear_namespace(session, buffer, ns_id, line_start, line_end)
}

#[api(since = 1, deprecated_since = 13, method)]
pub fn nvim_buf_add_highlight(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    hl_group: OxStr,
    line: i64,
    col_start: i64,
    col_end: i64,
) -> Result<i64, ApiError> {
    const MAXCOL: i64 = 0x7fff_ffff;
    let namespace = if ns_id <= 0 {
        crate::extmark::nvim_create_namespace(session, OxStr::from("nvim.buf.add_highlight"))?
    } else {
        ns_id
    };
    let (end_row, end_col) = if (0..MAXCOL).contains(&col_end) {
        (line, col_end)
    } else {
        (line + 1, 0)
    };
    let opts = Dict(vec![
        (OxStr::from("hl_group"), Object::String(hl_group)),
        (OxStr::from("end_row"), Object::Integer(end_row)),
        (OxStr::from("end_col"), Object::Integer(end_col)),
    ]);
    crate::extmark::nvim_buf_set_extmark(session, buffer, namespace, line, col_start, opts)?;
    Ok(namespace)
}

#[api(since = 5, deprecated_since = 8, method)]
#[expect(
    unused_variables,
    clippy::needless_pass_by_value,
    reason = "deprecated RPC entry keeps the upstream `opts` argument; dispatch passes it positionally"
)]
pub fn nvim_buf_set_virtual_text(
    session: &ApiSession,
    buffer: BufHandle,
    src_id: i64,
    line: i64,
    chunks: Vec<Object>,
    opts: Dict,
) -> Result<i64, ApiError> {
    let namespace = if src_id == 0 {
        crate::extmark::nvim_create_namespace(session, OxStr::from(""))?
    } else {
        src_id
    };
    let opts = Dict(vec![(OxStr::from("virt_text"), Object::Array(chunks))]);
    crate::extmark::nvim_buf_set_extmark(session, buffer, namespace, line, 0, opts)?;
    Ok(namespace)
}

#[api(since = 3, deprecated_since = 9)]
pub fn nvim_get_hl_by_id(session: &ApiSession, hl_id: i64, rgb: bool) -> Result<Dict, ApiError> {
    let definition = session
        .with_editor(|editor| hl_by_id(editor, hl_id).map(|(_, definition)| definition.clone()));
    let Some(definition) = definition else {
        return Err(ApiError::exception(format!(
            "Invalid highlight id: {hl_id}"
        )));
    };
    Ok(deprecated_hl_dict(&definition, rgb))
}

#[api(since = 3, deprecated_since = 9)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_get_hl_by_name(session: &ApiSession, name: OxStr, rgb: bool) -> Result<Dict, ApiError> {
    if name.as_bytes().is_empty() {
        return Err(ApiError::exception("Invalid highlight name"));
    }
    let definition = session
        .with_editor(|editor| hl_by_name(editor, &name).map(|(_, definition)| definition.clone()));
    let Some(definition) = definition else {
        return Err(ApiError::exception(format!(
            "Invalid highlight name: '{}'",
            name.to_string_lossy()
        )));
    };
    Ok(deprecated_hl_dict(&definition, rgb))
}

/// Canonical 16-color terminal palette mirroring Vim's `cterm-colors` name
/// table; names resolve case-insensitively to the terminal color index, and a
/// bare integer is accepted as a 256-color index.
const CTERM_COLORS: &[(&str, i64)] = &[
    ("Black", 0),
    ("DarkBlue", 1),
    ("DarkGreen", 2),
    ("DarkCyan", 3),
    ("DarkRed", 4),
    ("DarkMagenta", 5),
    ("Brown", 6),
    ("DarkYellow", 6),
    ("LightGray", 7),
    ("LightGrey", 7),
    ("Gray", 7),
    ("Grey", 7),
    ("DarkGray", 8),
    ("DarkGrey", 8),
    ("Blue", 9),
    ("Green", 10),
    ("Cyan", 11),
    ("Red", 12),
    ("Magenta", 13),
    ("Yellow", 14),
    ("White", 15),
];

fn cterm_color(value: &str) -> Option<i64> {
    if let Ok(index) = value.parse::<i64>() {
        return Some(index);
    }
    CTERM_COLORS
        .iter()
        .find_map(|(name, index)| name.eq_ignore_ascii_case(value).then_some(*index))
}

/// Resolves a `:hi` GUI color spelling (`#rrggbb` or named) to its RGB integer
/// via the canonical [`crate::ui::nvim_get_color_by_name`] color table.
fn rgb_color(value: &str) -> Option<i64> {
    let color = crate::ui::nvim_get_color_by_name(OxStr::from(value)).ok()?;
    (color >= 0).then_some(color)
}

/// Parses a comma-separated `gui=`/`cterm=` attribute list and pushes the
/// enabled flags. `standout` stays distinct from `reverse`: the deprecated API
/// reports what the user set, unlike the merged render attributes.
fn push_hl_flags(entries: &mut Vec<(OxStr, Object)>, raw: Option<&String>) {
    let Some(raw) = raw else {
        return;
    };
    for attr in raw.split(',') {
        let attr = attr.trim();
        if attr.eq_ignore_ascii_case("NONE") || attr.is_empty() {
            continue;
        }
        let key = match attr.to_ascii_lowercase().as_str() {
            "bold" => "bold",
            "italic" => "italic",
            "underline" => "underline",
            "undercurl" => "undercurl",
            "underdouble" => "underdouble",
            "underdotted" => "underdotted",
            "underdashed" => "underdashed",
            "strikethrough" => "strikethrough",
            "reverse" | "inverse" => "reverse",
            "standout" => "standout",
            "altfont" => "altfont",
            "nocombine" => "nocombine",
            _ => continue,
        };
        entries.push((OxStr::from(key), Object::Boolean(true)));
    }
}

/// Builds the deprecated `nvim_get_hl_by_*` dictionary from a raw `:hi`
/// definition: RGB integer colors plus `gui=` flags when `rgb` is true, cterm
/// color indices plus `cterm=` flags otherwise.
fn deprecated_hl_dict(definition: &HighlightDefinition, rgb: bool) -> Dict {
    let mut entries = Vec::new();
    if rgb {
        if let Some(color) = definition.get("guifg").and_then(|raw| rgb_color(raw)) {
            entries.push((OxStr::from("foreground"), Object::Integer(color)));
        }
        if let Some(color) = definition.get("guibg").and_then(|raw| rgb_color(raw)) {
            entries.push((OxStr::from("background"), Object::Integer(color)));
        }
        if let Some(color) = definition.get("guisp").and_then(|raw| rgb_color(raw)) {
            entries.push((OxStr::from("special"), Object::Integer(color)));
        }
        push_hl_flags(&mut entries, definition.get("gui"));
    } else {
        if let Some(color) = definition.get("ctermfg").and_then(|raw| cterm_color(raw)) {
            entries.push((OxStr::from("foreground"), Object::Integer(color)));
        }
        if let Some(color) = definition.get("ctermbg").and_then(|raw| cterm_color(raw)) {
            entries.push((OxStr::from("background"), Object::Integer(color)));
        }
        push_hl_flags(&mut entries, definition.get("cterm"));
    }
    Dict(entries)
}

fn hl_by_name<'a>(
    editor: &'a Editor,
    name: &OxStr,
) -> Option<(&'a String, &'a HighlightDefinition)> {
    let needle = std::str::from_utf8(name.as_bytes()).ok()?;
    editor
        .highlights()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(needle))
}

fn hl_by_id(editor: &Editor, hl_id: i64) -> Option<(&String, &HighlightDefinition)> {
    if hl_id <= 0 {
        return None;
    }
    let index = usize::try_from(hl_id - 1).ok()?;
    editor.highlights().iter().nth(index)
}

fn api_type_name(value: &Object) -> &'static str {
    match value {
        Object::Nil => "nil",
        Object::Boolean(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Float(_) => "Float",
        Object::String(_) => "String",
        Object::Array(_) => "Array",
        Object::Dict(_) => "Dict",
        Object::LuaRef(_) => "Function",
        Object::Buffer(_) => "Buffer",
        Object::Window(_) => "Window",
        Object::Tabpage(_) => "Tabpage",
    }
}

/// Decodes one `nvim_call_atomic` item without copying its method or arguments.
///
/// # Errors
///
/// Returns a validation error when the item is not a two-item array containing
/// a string method name and an argument array.
pub fn decode_atomic_call(call: &Object) -> Result<(&OxStr, &[Object]), ApiError> {
    let Object::Array(call) = call else {
        return Err(ApiError::validation(format!(
            "Invalid 'calls' item: expected Array, got {}",
            api_type_name(call)
        )));
    };
    let [name, args] = call.as_slice() else {
        return Err(ApiError::validation(
            "Invalid 'calls' item: expected 2-item Array",
        ));
    };
    let Object::String(name) = name else {
        return Err(ApiError::validation(format!(
            "Invalid 'name': expected String, got {}",
            api_type_name(name)
        )));
    };
    let Object::Array(args) = args else {
        return Err(ApiError::validation(format!(
            "Invalid call args: expected Array, got {}",
            api_type_name(args)
        )));
    };
    Ok((name, args))
}

#[api(since = 1, deprecated_since = 12)]
pub fn nvim_call_atomic(session: &ApiSession, calls: Vec<Object>) -> Result<Vec<Object>, ApiError> {
    let registry = crate::core().map_err(|error| ApiError::exception(error.to_string()))?;
    let mut results = Vec::with_capacity(calls.len());
    for (index, call) in calls.into_iter().enumerate() {
        let (name, args) = decode_atomic_call(&call)?;
        let name = std::str::from_utf8(name.as_bytes())
            .map_err(|_| ApiError::validation("call name must be UTF-8"))?;
        let result = match registry.get(name) {
            Some((_, dispatch)) => dispatch(session, args),
            None => Err(ApiError::exception(Registry::invalid_method_message(name))),
        };
        match result {
            Ok(value) => results.push(value),
            Err(error) => {
                return Ok(vec![
                    Object::Array(results),
                    Object::Array(vec![
                        Object::Integer(i64::try_from(index).unwrap_or(i64::MAX)),
                        Object::Integer(error.error_type()),
                        Object::String(OxStr::from(error.message())),
                    ]),
                ]);
            }
        }
    }
    Ok(vec![Object::Array(results), Object::Nil])
}

#[api(since = 1, deprecated_since = 13)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "deprecated RPC entry keeps its `Result` return shape"
)]
pub fn nvim_out_write(session: &ApiSession, str: OxStr) -> Result<(), ApiError> {
    session.with_editor_mut(|editor| {
        editor.push_message(Message {
            kind: MessageKind::Echo,
            content: Object::String(str),
            history: false,
            leading_newline: true,
        });
    });
    Ok(())
}

#[api(since = 1, deprecated_since = 13)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "deprecated RPC entry keeps its `Result` return shape"
)]
pub fn nvim_err_write(session: &ApiSession, str: OxStr) -> Result<(), ApiError> {
    session.with_editor_mut(|editor| {
        editor.push_message(Message {
            kind: MessageKind::Error,
            content: Object::String(str),
            history: false,
            leading_newline: true,
        });
    });
    Ok(())
}

#[api(since = 7, deprecated_since = 13)]
#[expect(
    unused_variables,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "deprecated RPC entry keeps the upstream `log_level` and `opts` arguments and its `Result` return shape"
)]
pub fn nvim_notify(
    session: &ApiSession,
    msg: OxStr,
    log_level: i64,
    opts: Dict,
) -> Result<Object, ApiError> {
    session.with_editor_mut(|editor| {
        editor.push_message(Message {
            kind: MessageKind::Echo,
            content: Object::String(msg),
            history: true,
            leading_newline: true,
        });
    });
    Ok(Object::Nil)
}

#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_line_count(session: &ApiSession, buffer: BufHandle) -> Result<i64, ApiError> {
    crate::buffer::nvim_buf_line_count(session, buffer)
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_insert(
    session: &ApiSession,
    buffer: BufHandle,
    lnum: i64,
    lines: Vec<OxStr>,
) -> Result<(), ApiError> {
    crate::buffer::nvim_buf_set_lines(session, buffer, lnum, lnum, true, lines)
}

fn legacy_index(index: i64) -> i64 {
    if index < 0 {
        index.saturating_sub(1)
    } else {
        index
    }
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_get_line(
    session: &ApiSession,
    buffer: BufHandle,
    index: i64,
) -> Result<OxStr, ApiError> {
    crate::buffer::nvim_buf_get_lines(
        session,
        buffer,
        legacy_index(index),
        legacy_index(index).saturating_add(1),
        true,
    )?
    .into_iter()
    .next()
    .ok_or_else(|| ApiError::validation("line index out of bounds"))
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_set_line(
    session: &ApiSession,
    buffer: BufHandle,
    index: i64,
    line: OxStr,
) -> Result<(), ApiError> {
    let index = legacy_index(index);
    crate::buffer::nvim_buf_set_lines(
        session,
        buffer,
        index,
        index.saturating_add(1),
        true,
        vec![line],
    )
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_del_line(
    session: &ApiSession,
    buffer: BufHandle,
    index: i64,
) -> Result<(), ApiError> {
    let index = legacy_index(index);
    crate::buffer::nvim_buf_set_lines(
        session,
        buffer,
        index,
        index.saturating_add(1),
        true,
        Vec::new(),
    )
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_get_line_slice(
    session: &ApiSession,
    buffer: BufHandle,
    start: i64,
    end: i64,
    include_start: bool,
    include_end: bool,
) -> Result<Vec<OxStr>, ApiError> {
    crate::buffer::nvim_buf_get_lines(
        session,
        buffer,
        legacy_index(start).saturating_add(i64::from(!include_start)),
        legacy_index(end).saturating_add(i64::from(include_end)),
        false,
    )
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_set_line_slice(
    session: &ApiSession,
    buffer: BufHandle,
    start: i64,
    end: i64,
    include_start: bool,
    include_end: bool,
    replacement: Vec<OxStr>,
) -> Result<(), ApiError> {
    crate::buffer::nvim_buf_set_lines(
        session,
        buffer,
        legacy_index(start).saturating_add(i64::from(!include_start)),
        legacy_index(end).saturating_add(i64::from(include_end)),
        false,
        replacement,
    )
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_set_var(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
    value: Object,
) -> Result<Object, ApiError> {
    let old = crate::buffer::nvim_buf_get_var(session, buffer, name.clone()).unwrap_or(Object::Nil);
    crate::buffer::nvim_buf_set_var(session, buffer, name, value)?;
    Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn buffer_del_var(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let old = crate::buffer::nvim_buf_get_var(session, buffer, name.clone()).unwrap_or(Object::Nil);
    crate::buffer::nvim_buf_del_var(session, buffer, name)?;
    Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn window_set_var(
    session: &ApiSession,
    window: WinHandle,
    name: OxStr,
    value: Object,
) -> Result<Object, ApiError> {
    let old = crate::window::nvim_win_get_var(session, window, name.clone()).unwrap_or(Object::Nil);
    crate::window::nvim_win_set_var(session, window, name, value)?;
    Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn window_del_var(
    session: &ApiSession,
    window: WinHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let old = crate::window::nvim_win_get_var(session, window, name.clone()).unwrap_or(Object::Nil);
    crate::window::nvim_win_del_var(session, window, name)?;
    Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn tabpage_set_var(
    session: &ApiSession,
    tabpage: TabHandle,
    name: OxStr,
    value: Object,
) -> Result<Object, ApiError> {
    let old =
        crate::tabpage::nvim_tabpage_get_var(session, tabpage, name.clone()).unwrap_or(Object::Nil);
    crate::tabpage::nvim_tabpage_set_var(session, tabpage, name, value)?;
    Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn tabpage_del_var(
    session: &ApiSession,
    tabpage: TabHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let old =
        crate::tabpage::nvim_tabpage_get_var(session, tabpage, name.clone()).unwrap_or(Object::Nil);
    crate::tabpage::nvim_tabpage_del_var(session, tabpage, name)?;
    Ok(old)
}

#[api(since = 0, deprecated_since = 1)]
pub fn vim_set_var(session: &ApiSession, name: OxStr, value: Object) -> Result<Object, ApiError> {
    session.with_editor_mut(|editor| {
        let old = editor.gvars().get(&name).cloned().unwrap_or(Object::Nil);
        editor.gvars_mut().insert(name, value);
        Ok(old)
    })
}

#[api(since = 0, deprecated_since = 1)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "deprecated RPC entry keeps its owned `name` argument"
)]
pub fn vim_del_var(session: &ApiSession, name: OxStr) -> Result<Object, ApiError> {
    session.with_editor_mut(|editor| {
        let index = editor
            .gvars()
            .0
            .iter()
            .position(|(key, _)| key == &name)
            .ok_or_else(|| {
                ApiError::validation(format!("Key not found: {}", name.to_string_lossy()))
            })?;
        Ok(editor.gvars_mut().0.remove(index).1)
    })
}

#[api(since = 7, deprecated_since = 11)]
pub fn nvim_get_option_info(_session: &ApiSession, name: OxStr) -> Result<Dict, ApiError> {
    let name_text = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("option name must be UTF-8"))?;
    let metadata = ox_editor::option_metadata(name_text).ok_or_else(|| {
        ApiError::validation(format!("Invalid option (not found): '{name_text}'"))
    })?;
    let scope = metadata
        .scopes
        .first()
        .copied()
        .unwrap_or(ox_editor::OptionScope::Global);
    Ok(Dict(vec![
        (OxStr::from("name"), Object::String(name)),
        (
            OxStr::from("scope"),
            Object::String(OxStr::from(match scope {
                ox_editor::OptionScope::Global => "global",
                ox_editor::OptionScope::Buffer => "buf",
                ox_editor::OptionScope::Window => "win",
                ox_editor::OptionScope::Tab => "tab",
            })),
        ),
        (
            OxStr::from("global_local"),
            Object::Boolean(metadata.scopes.len() > 1),
        ),
    ]))
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(
        buffer_line_count__API_META(),
        buffer_line_count__API_DISPATCH,
    )?;
    registry.register(buffer_insert__API_META(), buffer_insert__API_DISPATCH)?;
    registry.register(buffer_get_line__API_META(), buffer_get_line__API_DISPATCH)?;
    registry.register(buffer_set_line__API_META(), buffer_set_line__API_DISPATCH)?;
    registry.register(buffer_del_line__API_META(), buffer_del_line__API_DISPATCH)?;
    registry.register(
        buffer_get_line_slice__API_META(),
        buffer_get_line_slice__API_DISPATCH,
    )?;
    registry.register(
        buffer_set_line_slice__API_META(),
        buffer_set_line_slice__API_DISPATCH,
    )?;
    registry.register(buffer_set_var__API_META(), buffer_set_var__API_DISPATCH)?;
    registry.register(buffer_del_var__API_META(), buffer_del_var__API_DISPATCH)?;
    registry.register(window_set_var__API_META(), window_set_var__API_DISPATCH)?;
    registry.register(window_del_var__API_META(), window_del_var__API_DISPATCH)?;
    registry.register(tabpage_set_var__API_META(), tabpage_set_var__API_DISPATCH)?;
    registry.register(tabpage_del_var__API_META(), tabpage_del_var__API_DISPATCH)?;
    registry.register(vim_set_var__API_META(), vim_set_var__API_DISPATCH)?;
    registry.register(vim_del_var__API_META(), vim_del_var__API_DISPATCH)?;
    registry.register(vim_eval__API_META(), vim_eval__API_DISPATCH)?;
    registry.register(
        nvim_get_option_info__API_META(),
        nvim_get_option_info__API_DISPATCH,
    )?;
    registry.register(nvim_exec__API_META(), nvim_exec__API_DISPATCH)?;
    registry.register(
        nvim_command_output__API_META(),
        nvim_command_output__API_DISPATCH,
    )?;
    registry.register(nvim_execute_lua__API_META(), nvim_execute_lua__API_DISPATCH)?;
    registry.register(
        nvim_buf_get_number__API_META(),
        nvim_buf_get_number__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_clear_highlight__API_META(),
        nvim_buf_clear_highlight__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_add_highlight__API_META(),
        nvim_buf_add_highlight__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_set_virtual_text__API_META(),
        nvim_buf_set_virtual_text__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_hl_by_id__API_META(),
        nvim_get_hl_by_id__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_hl_by_name__API_META(),
        nvim_get_hl_by_name__API_DISPATCH,
    )?;
    registry.register(nvim_call_atomic__API_META(), nvim_call_atomic__API_DISPATCH)?;
    registry.register(nvim_out_write__API_META(), nvim_out_write__API_DISPATCH)?;
    registry.register(nvim_err_write__API_META(), nvim_err_write__API_DISPATCH)?;
    registry.register(nvim_notify__API_META(), nvim_notify__API_DISPATCH)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use ox_editor::{Editor, Geometry};
    use ox_text::Buffer;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[expect(clippy::unwrap_used, reason = "test builds a known-good editor")]
    fn session_with_lines(lines: &[&str]) -> (ApiSession, BufHandle) {
        let mut editor = Editor::new();
        let lines = lines
            .iter()
            .map(|line| line.as_bytes().to_vec())
            .collect::<Vec<_>>();
        let buffer = editor
            .create_buffer_with(Buffer::from_lines(&lines, false).unwrap(), true)
            .unwrap();
        let _ = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        (ApiSession::new(Rc::new(RefCell::new(editor))), buffer)
    }

    #[expect(
        clippy::unwrap_used,
        reason = "test asserts the deprecated success path"
    )]
    #[test]
    fn negative_col_end_converts_to_next_line_zero_column() {
        let (session, buffer) = session_with_lines(&["hello", "world"]);
        let ns = nvim_buf_add_highlight(&session, buffer, -1, OxStr::from("Question"), 0, 0, -1)
            .unwrap();
        assert!(ns > 0);
    }
}
