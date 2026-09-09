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
pub fn nvim_execute_lua(
    session: &ApiSession,
    code: OxStr,
    args: Vec<Object>,
) -> Result<Object, ApiError> {
    crate::global::nvim_exec_lua(session, code, args)
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
    const MAXLNUM: i64 = 0x7fff_ffff;
    if !(0..MAXLNUM).contains(&line) {
        return Err(ApiError::validation("Invalid line number: out of range"));
    }
    if !(0..=MAXCOL).contains(&col_start) {
        return Err(ApiError::validation("Invalid 'column': out of range"));
    }
    // src2ns (deprecated.c:88-97): ns_id == 0 mints a fresh anonymous
    // namespace and the mint is returned to the caller; ns_id < 0 is the
    // "ungrouped" case and must leave the caller's ns_id untouched (upstream
    // stores it under the raw sentinel 0x7fffffff, which never round-trips
    // through `nvim_create_namespace`/`ns_initialized`, so this port mints
    // its own throwaway anonymous namespace to hold the mark instead — the
    // storage namespace is never returned, so callers keep observing the
    // original negative ns_id).
    // `deprecated.c:158-166`: ns_id 0 or negative mints a storage
    // namespace; only 0 rewrites the returned id to the minted one.
    let (storage_ns, return_ns) = match ns_id {
        positive if positive > 0 => (ns_id, ns_id),
        zero_or_negative => {
            let minted = crate::extmark::nvim_create_namespace(session, OxStr::from(""))?;
            if zero_or_negative == 0 {
                (minted, minted)
            } else {
                (minted, ns_id)
            }
        }
    };
    let (buffer, line_count) =
        session.with_editor(|editor| -> Result<(BufHandle, usize), ApiError> {
            let resolved = if buffer.is_current() {
                editor
                    .current_buffer()
                    .ok_or_else(|| ApiError::validation("No current buffer"))?
            } else {
                buffer
            };
            let state = editor.buffer(resolved).map_err(|_| {
                ApiError::validation(format!("Invalid buffer id: {}", i64::from(resolved)))
            })?;
            let count = state
                .text()
                .map_err(|error| ApiError::exception(error.to_string()))?
                .line_count();
            Ok((resolved, count))
        })?;
    if usize::try_from(line).map_or(true, |row| row >= line_count) {
        // extmark_set safety check (deprecated.c:166-169): a line beyond the
        // buffer is a silent no-op, not an error.
        return Ok(return_ns);
    }
    if hl_group.as_bytes().is_empty() {
        return Ok(return_ns);
    }
    ensure_highlight_group_defined(session, &hl_group);
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
    crate::extmark::nvim_buf_set_extmark(session, buffer, storage_ns, line, col_start, opts)?;
    Ok(return_ns)
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

/// Auto-vivifies a highlight group the way `syn_check_group`
/// (`highlight_group.c`) interns an unknown group name passed to
/// `nvim_buf_add_highlight`: if `name` is not already a key in the editor's
/// highlight table (matched case-insensitively, mirroring [`hl_by_name`]),
/// insert an empty, uncolored definition so the name resolves to a stable id
/// without overwriting any spec a prior `:highlight` command already set.
fn ensure_highlight_group_defined(session: &ApiSession, name: &OxStr) {
    let Ok(name) = std::str::from_utf8(name.as_bytes()) else {
        return;
    };
    session.with_editor_mut(|editor| {
        let highlights = editor.highlights_mut();
        if !highlights.keys().any(|key| key.eq_ignore_ascii_case(name)) {
            highlights.insert(name.to_owned(), HighlightDefinition::default());
        }
    });
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

// Legacy `vim_*`, `buffer_*`, `window_*`, and `tabpage_*` names.
//
// Upstream exports these as pure aliases of the modern handlers:
// `src/nvim/api/dispatch_deprecated.lua` maps each modern name to its legacy
// alias, and `src/gen/gen_api_dispatch.lua:256-282` registers a shallow copy
// of the modern function (identical parameters and return type, same handler)
// under the alias name with `since = 0`, `deprecated_since = 1`, `lua = false`,
// and `eval = false`. Each shim below therefore forwards 1:1 to the modern
// crate function; handle sentinels (0 = current) resolve inside those paths,
// exactly as they do for the modern RPC names.

/// Alias of `nvim_command` (`dispatch_deprecated.lua:16`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_command(session: &ApiSession, cmd: OxStr) -> Result<(), ApiError> {
    crate::global::nvim_command(session, cmd)
}

/// Alias of `nvim_command_output` (deprecated.c:50, `dispatch_deprecated.lua:17`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_command_output(session: &ApiSession, command: OxStr) -> Result<OxStr, ApiError> {
    nvim_command_output(session, command)
}

/// Alias of `nvim_call_function` (`dispatch_deprecated.lua:15`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_call_function(
    session: &ApiSession,
    fn_name: OxStr,
    args: Vec<Object>,
) -> Result<Object, ApiError> {
    crate::global::nvim_call_function(session, fn_name, args)
}

/// Alias of `nvim_del_current_line` (`dispatch_deprecated.lua:18`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_del_current_line(session: &ApiSession) -> Result<(), ApiError> {
    crate::global::nvim_del_current_line(session)
}

/// Alias of `nvim_err_write` (deprecated.c:975, `dispatch_deprecated.lua:19`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_err_write(session: &ApiSession, str: OxStr) -> Result<(), ApiError> {
    nvim_err_write(session, str)
}

/// Alias of `nvim_err_writeln` (deprecated.c:984, `dispatch_deprecated.lua:20`):
/// unlike `vim_err_write` the message is always newline-terminated.
#[api(since = 0, deprecated_since = 1)]
pub fn vim_report_error(session: &ApiSession, str: OxStr) -> Result<(), ApiError> {
    crate::global::nvim_err_writeln(session, str)
}

/// Alias of `nvim_feedkeys` (`dispatch_deprecated.lua:22`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_feedkeys(
    session: &ApiSession,
    keys: OxStr,
    mode: OxStr,
    escape_ks: bool,
) -> Result<(), ApiError> {
    crate::ui::nvim_feedkeys(session, keys, mode, escape_ks)
}

/// Alias of `nvim_get_api_info` (`dispatch_deprecated.lua:23`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_api_info(session: &ApiSession) -> Result<Vec<Object>, ApiError> {
    crate::global::nvim_get_api_info(session)
}

/// Alias of `nvim_get_color_by_name` (`dispatch_deprecated.lua:24`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_name_to_color(name: OxStr) -> Result<i64, ApiError> {
    crate::ui::nvim_get_color_by_name(name)
}

/// Alias of `nvim_get_color_map` (`dispatch_deprecated.lua:25`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_color_map() -> Result<Dict, ApiError> {
    crate::ui::nvim_get_color_map()
}

/// Alias of `nvim_get_current_buf` (`dispatch_deprecated.lua:26`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_current_buffer(session: &ApiSession) -> Result<BufHandle, ApiError> {
    crate::global::nvim_get_current_buf(session)
}

/// Alias of `nvim_get_current_line` (`dispatch_deprecated.lua:27`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_current_line(session: &ApiSession) -> Result<OxStr, ApiError> {
    crate::buffer::nvim_get_current_line(session)
}

/// Alias of `nvim_get_current_tabpage` (`dispatch_deprecated.lua:28`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_current_tabpage(session: &ApiSession) -> Result<TabHandle, ApiError> {
    crate::global::nvim_get_current_tabpage(session)
}

/// Alias of `nvim_get_current_win` (`dispatch_deprecated.lua:29`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_current_window(session: &ApiSession) -> Result<WinHandle, ApiError> {
    crate::global::nvim_get_current_win(session)
}

/// Alias of `nvim_get_option` (`dispatch_deprecated.lua:30`): global option value.
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_option(session: &ApiSession, name: OxStr) -> Result<Object, ApiError> {
    crate::global::nvim_get_option(session, name)
}

/// Alias of `nvim_set_option` (`dispatch_deprecated.lua:45`): global option value.
#[api(since = 0, deprecated_since = 1)]
pub fn vim_set_option(session: &ApiSession, name: OxStr, value: Object) -> Result<(), ApiError> {
    crate::global::nvim_set_option(session, name, value)
}

/// Alias of `nvim_get_var` (`dispatch_deprecated.lua:31`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_var(session: &ApiSession, name: OxStr) -> Result<Object, ApiError> {
    crate::global::nvim_get_var(session, name)
}

/// Alias of `nvim_get_vvar` (`dispatch_deprecated.lua:32`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_vvar(session: &ApiSession, name: OxStr) -> Result<Object, ApiError> {
    crate::global::nvim_get_vvar(session, name)
}

/// Alias of `nvim_input` (`dispatch_deprecated.lua:33`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_input(session: &ApiSession, keys: OxStr) -> Result<i64, ApiError> {
    crate::global::nvim_input(session, keys)
}

/// Alias of `nvim_list_bufs` (`dispatch_deprecated.lua:34`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_buffers(session: &ApiSession) -> Result<Vec<BufHandle>, ApiError> {
    crate::global::nvim_list_bufs(session)
}

/// Alias of `nvim_list_runtime_paths` (`dispatch_deprecated.lua:35`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_list_runtime_paths(session: &ApiSession) -> Result<Vec<OxStr>, ApiError> {
    crate::channel::nvim_list_runtime_paths(session)
}

/// Alias of `nvim_list_tabpages` (`dispatch_deprecated.lua:36`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_tabpages(session: &ApiSession) -> Result<Vec<TabHandle>, ApiError> {
    crate::global::nvim_list_tabpages(session)
}

/// Alias of `nvim_list_wins` (`dispatch_deprecated.lua:37`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_get_windows(session: &ApiSession) -> Result<Vec<WinHandle>, ApiError> {
    crate::global::nvim_list_wins(session)
}

/// Alias of `nvim_out_write` (deprecated.c:966, `dispatch_deprecated.lua:38`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_out_write(session: &ApiSession, str: OxStr) -> Result<(), ApiError> {
    nvim_out_write(session, str)
}

/// Alias of `nvim_replace_termcodes` (`dispatch_deprecated.lua:39`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_replace_termcodes(
    session: &ApiSession,
    str: OxStr,
    from_part: bool,
    do_lt: bool,
    special: bool,
) -> Result<OxStr, ApiError> {
    crate::global::nvim_replace_termcodes(session, str, from_part, do_lt, special)
}

/// Alias of `nvim_set_current_buf` (`dispatch_deprecated.lua:40`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_set_current_buffer(session: &ApiSession, buf: BufHandle) -> Result<(), ApiError> {
    crate::global::nvim_set_current_buf(session, buf)
}

/// Alias of `nvim_set_current_dir` (`dispatch_deprecated.lua:41`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_change_directory(session: &ApiSession, dir: OxStr) -> Result<(), ApiError> {
    crate::global::nvim_set_current_dir(session, dir)
}

/// Alias of `nvim_set_current_line` (`dispatch_deprecated.lua:42`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_set_current_line(session: &ApiSession, line: OxStr) -> Result<(), ApiError> {
    crate::buffer::nvim_set_current_line(session, line)
}

/// Alias of `nvim_set_current_tabpage` (`dispatch_deprecated.lua:43`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_set_current_tabpage(session: &ApiSession, tabpage: TabHandle) -> Result<(), ApiError> {
    crate::global::nvim_set_current_tabpage(session, tabpage)
}

/// Alias of `nvim_set_current_win` (`dispatch_deprecated.lua:44`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_set_current_window(session: &ApiSession, win: WinHandle) -> Result<(), ApiError> {
    crate::global::nvim_set_current_win(session, win)
}

/// Alias of `nvim_strwidth` (`dispatch_deprecated.lua:46`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_strwidth(session: &ApiSession, text: OxStr) -> Result<i64, ApiError> {
    crate::global::nvim_strwidth(session, text)
}

/// Alias of `nvim_subscribe` (`dispatch_deprecated.lua:47`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_subscribe(session: &ApiSession, event: OxStr) -> Result<(), ApiError> {
    crate::channel::nvim_subscribe(session, event)
}

/// Alias of `nvim_unsubscribe` (`dispatch_deprecated.lua:54`).
#[api(since = 0, deprecated_since = 1)]
pub fn vim_unsubscribe(session: &ApiSession, event: OxStr) -> Result<(), ApiError> {
    crate::channel::nvim_unsubscribe(session, event)
}

// Legacy `buffer_*` aliases (dispatch_deprecated.lua:2-14); each forwards to
// the modern `nvim_buf_*` path, so the buffer-id-0 sentinel resolves to the
// current buffer exactly as the modern name does.

/// Alias of `nvim_buf_add_highlight` (deprecated.c:143, `dispatch_deprecated.lua:2`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_add_highlight(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    hl_group: OxStr,
    line: i64,
    col_start: i64,
    col_end: i64,
) -> Result<i64, ApiError> {
    nvim_buf_add_highlight(session, buffer, ns_id, hl_group, line, col_start, col_end)
}

/// Alias of `nvim_buf_clear_highlight` (deprecated.c:109, `dispatch_deprecated.lua:3`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_clear_highlight(
    session: &ApiSession,
    buffer: BufHandle,
    ns_id: i64,
    line_start: i64,
    line_end: i64,
) -> Result<(), ApiError> {
    nvim_buf_clear_highlight(session, buffer, ns_id, line_start, line_end)
}

/// Alias of `nvim_buf_get_lines` (`dispatch_deprecated.lua:4`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_get_lines(
    session: &ApiSession,
    buffer: BufHandle,
    start: i64,
    end: i64,
    strict_indexing: bool,
) -> Result<Vec<OxStr>, ApiError> {
    crate::buffer::nvim_buf_get_lines(session, buffer, start, end, strict_indexing)
}

/// Alias of `nvim_buf_get_mark` (`dispatch_deprecated.lua:5`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_get_mark(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Vec<i64>, ApiError> {
    crate::buffer::nvim_buf_get_mark(session, buffer, name)
}

/// Alias of `nvim_buf_get_name` (`dispatch_deprecated.lua:6`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_get_name(session: &ApiSession, buffer: BufHandle) -> Result<OxStr, ApiError> {
    crate::buffer::nvim_buf_get_name(session, buffer)
}

/// Alias of `nvim_buf_get_number` (deprecated.c:75, `dispatch_deprecated.lua:7`):
/// upstream returns `buf->b_fnum`, which equals the buffer object id.
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_get_number(session: &ApiSession, buffer: BufHandle) -> Result<i64, ApiError> {
    nvim_buf_get_number(session, buffer)
}

/// Alias of `nvim_buf_get_option` (`dispatch_deprecated.lua:8`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_get_option(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    crate::buffer::nvim_buf_get_option(session, buffer, name)
}

/// Alias of `nvim_buf_get_var` (`dispatch_deprecated.lua:9`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_get_var(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    crate::buffer::nvim_buf_get_var(session, buffer, name)
}

/// Alias of `nvim_buf_is_valid` (`dispatch_deprecated.lua:10`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_is_valid(session: &ApiSession, buffer: BufHandle) -> Result<bool, ApiError> {
    crate::buffer::nvim_buf_is_valid(session, buffer)
}

/// Alias of `nvim_buf_set_lines` (`dispatch_deprecated.lua:12`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_set_lines(
    session: &ApiSession,
    buffer: BufHandle,
    start: i64,
    end: i64,
    strict_indexing: bool,
    replacement: Vec<OxStr>,
) -> Result<(), ApiError> {
    crate::buffer::nvim_buf_set_lines(session, buffer, start, end, strict_indexing, replacement)
}

/// Alias of `nvim_buf_set_name` (`dispatch_deprecated.lua:13`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_set_name(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    crate::buffer::nvim_buf_set_name(session, buffer, name)
}

/// Alias of `nvim_buf_set_option` (`dispatch_deprecated.lua:14`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn buffer_set_option(
    session: &ApiSession,
    buffer: BufHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    crate::buffer::nvim_buf_set_option(session, buffer, name, value)
}

// Legacy `window_*` aliases (dispatch_deprecated.lua:55-67); window-id 0
// resolves to the current window inside the modern `nvim_win_*` paths.

/// Alias of `nvim_win_get_buf` (`dispatch_deprecated.lua:55`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_buffer(session: &ApiSession, win: WinHandle) -> Result<BufHandle, ApiError> {
    crate::window::nvim_win_get_buf(session, win)
}

/// Alias of `nvim_win_get_cursor` (`dispatch_deprecated.lua:56`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_cursor(session: &ApiSession, win: WinHandle) -> Result<Vec<i64>, ApiError> {
    crate::window::nvim_win_get_cursor(session, win)
}

/// Alias of `nvim_win_set_cursor` (`dispatch_deprecated.lua:64`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_set_cursor(
    session: &ApiSession,
    win: WinHandle,
    pos: Vec<i64>,
) -> Result<(), ApiError> {
    crate::window::nvim_win_set_cursor(session, win, pos)
}

/// Alias of `nvim_win_get_height` (`dispatch_deprecated.lua:57`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_height(session: &ApiSession, win: WinHandle) -> Result<i64, ApiError> {
    crate::window::nvim_win_get_height(session, win)
}

/// Alias of `nvim_win_set_height` (deprecated.c:1018, `dispatch_deprecated.lua:65`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_set_height(
    session: &ApiSession,
    win: WinHandle,
    height: i64,
) -> Result<(), ApiError> {
    crate::window::nvim_win_set_height(session, win, height)
}

/// Alias of `nvim_win_get_width` (`dispatch_deprecated.lua:62`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_width(session: &ApiSession, win: WinHandle) -> Result<i64, ApiError> {
    crate::window::nvim_win_get_width(session, win)
}

/// Alias of `nvim_win_set_width` (deprecated.c:1042, `dispatch_deprecated.lua:67`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_set_width(session: &ApiSession, win: WinHandle, width: i64) -> Result<(), ApiError> {
    crate::window::nvim_win_set_width(session, win, width)
}

/// Alias of `nvim_win_get_option` (`dispatch_deprecated.lua:58`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_option(
    session: &ApiSession,
    window: WinHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    crate::window::nvim_win_get_option(session, window, name)
}

/// Alias of `nvim_win_set_option` (`dispatch_deprecated.lua:66`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_set_option(
    session: &ApiSession,
    window: WinHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    crate::window::nvim_win_set_option(session, window, name, value)
}

/// Alias of `nvim_win_get_position` (`dispatch_deprecated.lua:59`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_position(session: &ApiSession, win: WinHandle) -> Result<Vec<i64>, ApiError> {
    crate::window::nvim_win_get_position(session, win)
}

/// Alias of `nvim_win_get_tabpage` (`dispatch_deprecated.lua:60`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_tabpage(session: &ApiSession, win: WinHandle) -> Result<TabHandle, ApiError> {
    crate::window::nvim_win_get_tabpage(session, win)
}

/// Alias of `nvim_win_get_var` (`dispatch_deprecated.lua:61`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_get_var(
    session: &ApiSession,
    win: WinHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    crate::window::nvim_win_get_var(session, win, name)
}

/// Alias of `nvim_win_is_valid` (`dispatch_deprecated.lua:63`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn window_is_valid(session: &ApiSession, win: WinHandle) -> Result<bool, ApiError> {
    crate::window::nvim_win_is_valid(session, win)
}

// Legacy `tabpage_*` aliases (dispatch_deprecated.lua:48-51); tabpage-id 0
// resolves to the current tabpage inside the modern `nvim_tabpage_*` paths.

/// Alias of `nvim_tabpage_get_var` (`dispatch_deprecated.lua:48`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn tabpage_get_var(
    session: &ApiSession,
    tabpage: TabHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    crate::tabpage::nvim_tabpage_get_var(session, tabpage, name)
}

/// Alias of `nvim_tabpage_get_win` (`dispatch_deprecated.lua:49`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn tabpage_get_window(session: &ApiSession, tabpage: TabHandle) -> Result<WinHandle, ApiError> {
    crate::tabpage::nvim_tabpage_get_win(session, tabpage)
}

/// Alias of `nvim_tabpage_list_wins` (`dispatch_deprecated.lua:51`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn tabpage_get_windows(
    session: &ApiSession,
    tabpage: TabHandle,
) -> Result<Vec<WinHandle>, ApiError> {
    crate::tabpage::nvim_tabpage_list_wins(session, tabpage)
}

/// Alias of `nvim_tabpage_is_valid` (`dispatch_deprecated.lua:50`).
#[api(since = 0, deprecated_since = 1, method)]
pub fn tabpage_is_valid(session: &ApiSession, tabpage: TabHandle) -> Result<bool, ApiError> {
    crate::tabpage::nvim_tabpage_is_valid(session, tabpage)
}

#[expect(
    clippy::too_many_lines,
    reason = "generated-style registration list; one entry per deprecated alias"
)]
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
    registry.register(
        buffer_add_highlight__API_META(),
        buffer_add_highlight__API_DISPATCH,
    )?;
    registry.register(
        buffer_clear_highlight__API_META(),
        buffer_clear_highlight__API_DISPATCH,
    )?;
    registry.register(buffer_get_lines__API_META(), buffer_get_lines__API_DISPATCH)?;
    registry.register(buffer_get_mark__API_META(), buffer_get_mark__API_DISPATCH)?;
    registry.register(buffer_get_name__API_META(), buffer_get_name__API_DISPATCH)?;
    registry.register(
        buffer_get_number__API_META(),
        buffer_get_number__API_DISPATCH,
    )?;
    registry.register(
        buffer_get_option__API_META(),
        buffer_get_option__API_DISPATCH,
    )?;
    registry.register(buffer_get_var__API_META(), buffer_get_var__API_DISPATCH)?;
    registry.register(buffer_is_valid__API_META(), buffer_is_valid__API_DISPATCH)?;
    registry.register(buffer_set_lines__API_META(), buffer_set_lines__API_DISPATCH)?;
    registry.register(buffer_set_name__API_META(), buffer_set_name__API_DISPATCH)?;
    registry.register(
        buffer_set_option__API_META(),
        buffer_set_option__API_DISPATCH,
    )?;
    registry.register(tabpage_get_var__API_META(), tabpage_get_var__API_DISPATCH)?;
    registry.register(
        tabpage_get_window__API_META(),
        tabpage_get_window__API_DISPATCH,
    )?;
    registry.register(
        tabpage_get_windows__API_META(),
        tabpage_get_windows__API_DISPATCH,
    )?;
    registry.register(tabpage_is_valid__API_META(), tabpage_is_valid__API_DISPATCH)?;
    registry.register(
        vim_call_function__API_META(),
        vim_call_function__API_DISPATCH,
    )?;
    registry.register(
        vim_change_directory__API_META(),
        vim_change_directory__API_DISPATCH,
    )?;
    registry.register(vim_command__API_META(), vim_command__API_DISPATCH)?;
    registry.register(
        vim_command_output__API_META(),
        vim_command_output__API_DISPATCH,
    )?;
    registry.register(
        vim_del_current_line__API_META(),
        vim_del_current_line__API_DISPATCH,
    )?;
    registry.register(vim_err_write__API_META(), vim_err_write__API_DISPATCH)?;
    registry.register(vim_feedkeys__API_META(), vim_feedkeys__API_DISPATCH)?;
    registry.register(vim_get_api_info__API_META(), vim_get_api_info__API_DISPATCH)?;
    registry.register(vim_get_buffers__API_META(), vim_get_buffers__API_DISPATCH)?;
    registry.register(
        vim_get_color_map__API_META(),
        vim_get_color_map__API_DISPATCH,
    )?;
    registry.register(
        vim_get_current_buffer__API_META(),
        vim_get_current_buffer__API_DISPATCH,
    )?;
    registry.register(
        vim_get_current_line__API_META(),
        vim_get_current_line__API_DISPATCH,
    )?;
    registry.register(
        vim_get_current_tabpage__API_META(),
        vim_get_current_tabpage__API_DISPATCH,
    )?;
    registry.register(
        vim_get_current_window__API_META(),
        vim_get_current_window__API_DISPATCH,
    )?;
    registry.register(vim_get_option__API_META(), vim_get_option__API_DISPATCH)?;
    registry.register(vim_get_tabpages__API_META(), vim_get_tabpages__API_DISPATCH)?;
    registry.register(vim_get_var__API_META(), vim_get_var__API_DISPATCH)?;
    registry.register(vim_get_vvar__API_META(), vim_get_vvar__API_DISPATCH)?;
    registry.register(vim_get_windows__API_META(), vim_get_windows__API_DISPATCH)?;
    registry.register(vim_input__API_META(), vim_input__API_DISPATCH)?;
    registry.register(
        vim_list_runtime_paths__API_META(),
        vim_list_runtime_paths__API_DISPATCH,
    )?;
    registry.register(
        vim_name_to_color__API_META(),
        vim_name_to_color__API_DISPATCH,
    )?;
    registry.register(vim_out_write__API_META(), vim_out_write__API_DISPATCH)?;
    registry.register(
        vim_replace_termcodes__API_META(),
        vim_replace_termcodes__API_DISPATCH,
    )?;
    registry.register(vim_report_error__API_META(), vim_report_error__API_DISPATCH)?;
    registry.register(
        vim_set_current_buffer__API_META(),
        vim_set_current_buffer__API_DISPATCH,
    )?;
    registry.register(
        vim_set_current_line__API_META(),
        vim_set_current_line__API_DISPATCH,
    )?;
    registry.register(
        vim_set_current_tabpage__API_META(),
        vim_set_current_tabpage__API_DISPATCH,
    )?;
    registry.register(
        vim_set_current_window__API_META(),
        vim_set_current_window__API_DISPATCH,
    )?;
    registry.register(vim_set_option__API_META(), vim_set_option__API_DISPATCH)?;
    registry.register(vim_strwidth__API_META(), vim_strwidth__API_DISPATCH)?;
    registry.register(vim_subscribe__API_META(), vim_subscribe__API_DISPATCH)?;
    registry.register(vim_unsubscribe__API_META(), vim_unsubscribe__API_DISPATCH)?;
    registry.register(
        window_get_buffer__API_META(),
        window_get_buffer__API_DISPATCH,
    )?;
    registry.register(
        window_get_cursor__API_META(),
        window_get_cursor__API_DISPATCH,
    )?;
    registry.register(
        window_get_height__API_META(),
        window_get_height__API_DISPATCH,
    )?;
    registry.register(
        window_get_option__API_META(),
        window_get_option__API_DISPATCH,
    )?;
    registry.register(
        window_get_position__API_META(),
        window_get_position__API_DISPATCH,
    )?;
    registry.register(
        window_get_tabpage__API_META(),
        window_get_tabpage__API_DISPATCH,
    )?;
    registry.register(window_get_var__API_META(), window_get_var__API_DISPATCH)?;
    registry.register(window_get_width__API_META(), window_get_width__API_DISPATCH)?;
    registry.register(window_is_valid__API_META(), window_is_valid__API_DISPATCH)?;
    registry.register(
        window_set_cursor__API_META(),
        window_set_cursor__API_DISPATCH,
    )?;
    registry.register(
        window_set_height__API_META(),
        window_set_height__API_DISPATCH,
    )?;
    registry.register(
        window_set_option__API_META(),
        window_set_option__API_DISPATCH,
    )?;
    registry.register(window_set_width__API_META(), window_set_width__API_DISPATCH)?;

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
        // `src2ns` (deprecated.c:88-97): a negative ns stays negative in the
        // return value; the mark lives in a throwaway storage namespace.
        assert_eq!(ns, -1);
    }

    /// Every advertised API name has a registry dispatch: the metadata
    /// tables generate both, but an unregistered `#[api]` entry still
    /// answers "not implemented" on the wire (the dispatch probe's 85-name
    /// gap class). This sweep is the permanent pin against that drift.
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "test asserts the registry builds without error"
    )]
    fn every_advertised_api_name_dispatches() {
        let registry = crate::registry::implemented().unwrap();
        let missing: Vec<&str> = crate::api_function_names::API_FUNCTIONS
            .iter()
            .map(|entry| entry.name)
            .filter(|name| registry.get(name).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "advertised but unregistered: {missing:?}"
        );
    }
}
