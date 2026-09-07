//! Mapping registration and listing (`nvim_set_keymap` and its family).
//!
//! Every one of these is `mapping.c` `modify_keymap`/`keymap_array` over the
//! editor's mapping table; the buffer-scoped pair differ only in the
//! [`MapScope`] they work in. `vim.keymap.set()` is written on top of
//! `nvim_set_keymap`, so this is the surface a plugin registers through.

use ox_editor::typeahead::special_notation;
use ox_editor::{Keys, MapModes, MapScope, Mapping, MappingAction, MappingError};

use crate::session::ApiSession;
use crate::{ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, api};

/// `nvim_set_keymap` runs `replace_termcodes` over the left-hand side, which
/// substitutes `<Leader>` from `g:mapleader`. Global variables do not reach
/// this layer, so both leaders keep their documented default of a backslash.
const LEADER: &str = "\\";

fn exception(error: impl std::fmt::Display) -> ApiError {
    ApiError::exception(error.to_string())
}

/// mapping.c `modify_keymap`: the mode string is one recognized shortname,
/// optionally `!`, and must be consumed whole. An empty string means `:map`.
fn parse_modes(mode: &OxStr) -> Result<MapModes, ApiError> {
    let text = String::from_utf8_lossy(mode.as_bytes()).into_owned();
    let invalid = || ApiError::validation(format!("Invalid mode shortname: \"{text}\""));
    match text.as_str() {
        "" => Ok(MapModes::MAP),
        "!" | "!a" => Ok(MapModes::MAP_BANG),
        "n" | "v" | "x" | "s" | "o" | "i" | "c" | "l" | "t" => {
            Ok(MapModes::from_mode_string(&text))
        }
        "ia" => Ok(MapModes::from_mode_string("i")),
        "ca" => Ok(MapModes::from_mode_string("c")),
        _ => Err(invalid()),
    }
}

fn is_abbrev_mode(mode: &OxStr) -> bool {
    matches!(
        String::from_utf8_lossy(mode.as_bytes()).as_ref(),
        "ia" | "ca" | "!a"
    )
}

/// The options `Dict(keymap)` accepts, rejected by name like every other API
/// keyset so a typo is reported rather than ignored.
#[expect(
    clippy::struct_excessive_bools,
    reason = "Neovim keymap options are independent RPC flags"
)]
struct KeymapOptions {
    noremap: bool,
    nowait: bool,
    silent: bool,
    script: bool,
    expr: bool,
    unique: bool,
    replace_keycodes: bool,
    desc: Option<String>,
    callback: Option<i32>,
}

fn parse_options(opts: &Dict) -> Result<KeymapOptions, ApiError> {
    let mut parsed = KeymapOptions {
        noremap: false,
        nowait: false,
        silent: false,
        script: false,
        expr: false,
        unique: false,
        replace_keycodes: false,
        desc: None,
        callback: None,
    };
    for (key, value) in opts.iter() {
        let flag = |value: &Object| match value {
            Object::Boolean(value) => Ok(*value),
            Object::Integer(value) => Ok(*value != 0),
            Object::Nil => Ok(false),
            _ => Err(ApiError::validation(format!(
                "Invalid '{}': expected Boolean",
                key.to_string_lossy()
            ))),
        };
        match key.as_bytes() {
            b"noremap" => parsed.noremap = flag(value)?,
            b"nowait" => parsed.nowait = flag(value)?,
            b"silent" => parsed.silent = flag(value)?,
            b"script" => parsed.script = flag(value)?,
            b"expr" => parsed.expr = flag(value)?,
            b"unique" => parsed.unique = flag(value)?,
            b"replace_keycodes" => parsed.replace_keycodes = flag(value)?,
            b"desc" => match value {
                Object::String(text) => parsed.desc = Some(text.to_string_lossy().into_owned()),
                Object::Nil => parsed.desc = None,
                _ => return Err(ApiError::validation("Invalid 'desc': expected String")),
            },
            b"callback" => match value {
                Object::LuaRef(reference) => parsed.callback = Some(*reference),
                Object::Nil => parsed.callback = None,
                _ => {
                    return Err(ApiError::validation(
                        "Invalid 'callback': expected Function",
                    ));
                }
            },
            other => {
                return Err(ApiError::validation(format!(
                    "invalid key: {}",
                    String::from_utf8_lossy(other)
                )));
            }
        }
    }
    if parsed.replace_keycodes && !parsed.expr {
        return Err(ApiError::validation(
            "\"replace_keycodes\" requires \"expr\"",
        ));
    }
    Ok(parsed)
}

fn map_error(error: &MappingError) -> ApiError {
    ApiError::exception(error.to_string())
}

/// mapping.c `modify_keymap` with `MAPTYPE_MAP`, shared by the global and
/// buffer-local entry points.
fn set_keymap(
    session: &ApiSession,
    scope: MapScope,
    mode: &OxStr,
    lhs: &OxStr,
    rhs: &OxStr,
    opts: &Dict,
) -> Result<(), ApiError> {
    let modes = parse_modes(mode)?;
    let options = parse_options(opts)?;
    let lhs_text = String::from_utf8_lossy(lhs.as_bytes()).into_owned();
    let lhs = Keys::parse_notation(&lhs_text, LEADER, LEADER);
    if lhs.is_empty() {
        return Err(ApiError::validation("Invalid (empty) LHS"));
    }
    if options.unique
        && session.with_editor(|editor| editor.mappings().conflicts(&lhs, modes, scope))
    {
        return Err(ApiError::exception(format!(
            "E227: Mapping already exists for {lhs_text}"
        )));
    }
    let rhs_text = String::from_utf8_lossy(rhs.as_bytes()).into_owned();
    let action = match (options.callback, options.expr) {
        (Some(reference), _) => MappingAction::Callback(u64::from(reference.unsigned_abs())),
        // `<expr>` keeps the right-hand side as an expression to re-evaluate;
        // every other form is decoded as key notation now.
        (None, true) => MappingAction::Expr(rhs_text.clone()),
        (None, false) => MappingAction::parse_rhs(&rhs_text, LEADER, LEADER)
            .map_err(|error| map_error(&error))?,
    };
    if is_abbrev_mode(mode) {
        return session
            .with_editor_mut(|editor| {
                editor.mappings_mut().abbreviate(
                    &lhs_text,
                    action,
                    scope,
                    !(options.noremap || options.script),
                )
            })
            .map_err(|error| map_error(&error));
    }
    let mut mapping_options = ox_editor::MappingOptions {
        modes,
        scope,
        description: options.desc,
        orig_rhs: rhs_text,
        script_context: ox_editor::script::SourceContext::default(),
        ..ox_editor::MappingOptions::default()
    };
    let remap = !(options.noremap || options.script);
    mapping_options.flags.set(ox_editor::MapFlags::REMAP, remap);
    mapping_options
        .flags
        .set(ox_editor::MapFlags::NOWAIT, options.nowait);
    mapping_options
        .flags
        .set(ox_editor::MapFlags::SILENT, options.silent);
    mapping_options
        .flags
        .set(ox_editor::MapFlags::SCRIPT, options.script);
    session
        .with_editor_mut(|editor| editor.mappings_mut().map(lhs, action, mapping_options))
        .map_err(|error| map_error(&error))
}

/// mapping.c `modify_keymap` with `MAPTYPE_UNMAP`: removing a mapping that is
/// not there is `E31`, not a silent success.
fn del_keymap(
    session: &ApiSession,
    scope: MapScope,
    mode: &OxStr,
    lhs: &OxStr,
) -> Result<(), ApiError> {
    let lhs_text = String::from_utf8_lossy(lhs.as_bytes()).into_owned();
    if is_abbrev_mode(mode) {
        if !session.with_editor_mut(|editor| editor.mappings_mut().unabbreviate(&lhs_text, scope)) {
            return Err(ApiError::exception("E31: No such mapping"));
        }
        return Ok(());
    }
    let modes = parse_modes(mode)?;
    let lhs = Keys::parse_notation(&lhs_text, LEADER, LEADER);
    if lhs.is_empty() {
        return Err(ApiError::validation("Invalid (empty) LHS"));
    }
    if session.with_editor_mut(|editor| editor.mappings_mut().unmap(&lhs, modes, scope)) == 0 {
        return Err(ApiError::exception("E31: No such mapping"));
    }
    Ok(())
}

/// mapping.c `keymap_array`: every mapping in one scope whose modes overlap,
/// in `:map`'s listing order, each rendered by `mapblock_fill_dict`.
fn keymap_array(
    session: &ApiSession,
    scope: MapScope,
    mode: &OxStr,
) -> Result<Vec<Object>, ApiError> {
    let modes = parse_modes(mode)?;
    let (buffer, want_local) = match scope {
        MapScope::Global => (None, false),
        MapScope::Buffer(buffer) => (Some(buffer), true),
    };
    Ok(session.with_editor(|editor| {
        editor
            .mappings()
            .matching(b"", modes, buffer)
            .into_iter()
            .filter(|(_, local)| *local == want_local)
            .map(|(mapping, local)| Object::Dict(fill_dict(mapping, local, buffer)))
            .collect()
    }))
}

/// mapping.c `mapblock_fill_dict` with `compatible` false, which is the shape
/// `nvim_get_keymap` returns: the `maparg()` keys plus `buf`, with `rhs`
/// replaced by `callback` when the right-hand side is a host function.
///
/// `lhsrawalt` is absent: upstream emits it only for a mapping that key
/// simplification produced an alternative form for, and this port keeps no
/// simplified alternates. `sid` and `lnum` report the empty script context an
/// API-created mapping carries here.
fn fill_dict(mapping: &Mapping, local: bool, buffer: Option<BufHandle>) -> Dict {
    let options = &mapping.options;
    let context = options.script_context;
    let mut entries = vec![
        (
            OxStr::from("lhs"),
            Object::String(OxStr::from(
                special_notation(mapping.lhs.as_bytes(), true, false).as_str(),
            )),
        ),
        (
            OxStr::from("lhsraw"),
            Object::String(OxStr(mapping.lhs.as_bytes().to_vec())),
        ),
        (
            OxStr::from("noremap"),
            Object::Integer(i64::from(
                !options.flags.contains(ox_editor::MapFlags::REMAP),
            )),
        ),
        (
            OxStr::from("script"),
            Object::Integer(i64::from(
                options.flags.contains(ox_editor::MapFlags::SCRIPT),
            )),
        ),
        (
            OxStr::from("expr"),
            Object::Integer(i64::from(matches!(mapping.action, MappingAction::Expr(_)))),
        ),
        (
            OxStr::from("silent"),
            Object::Integer(i64::from(
                options.flags.contains(ox_editor::MapFlags::SILENT),
            )),
        ),
        (
            OxStr::from("sid"),
            Object::Integer(i64::try_from(context.sid).unwrap_or(0)),
        ),
        (OxStr::from("scriptversion"), Object::Integer(1)),
        (
            OxStr::from("lnum"),
            Object::Integer(i64::try_from(context.lnum).unwrap_or(0)),
        ),
        (OxStr::from("buffer"), Object::Integer(i64::from(local))),
        (
            OxStr::from("buf"),
            Object::Integer(buffer.filter(|_| local).map_or(0, i64::from)),
        ),
        (
            OxStr::from("nowait"),
            Object::Integer(i64::from(
                options.flags.contains(ox_editor::MapFlags::NOWAIT),
            )),
        ),
        (OxStr::from("replace_keycodes"), Object::Integer(0)),
        (
            OxStr::from("mode"),
            Object::String(OxStr::from(options.modes.to_chars().as_str())),
        ),
        (OxStr::from("abbr"), Object::Integer(0)),
        (
            OxStr::from("mode_bits"),
            Object::Integer(i64::from(options.modes.bits())),
        ),
    ];
    match &mapping.action {
        // A host callback has no key string at all, so upstream reports the
        // reference in place of `rhs`.
        MappingAction::Callback(reference) => entries.push((
            OxStr::from("callback"),
            Object::LuaRef(i32::try_from(*reference).unwrap_or(0)),
        )),
        _ => entries.push((
            OxStr::from("rhs"),
            Object::String(OxStr::from(options.orig_rhs.as_str())),
        )),
    }
    if let Some(description) = &options.description {
        entries.push((
            OxStr::from("desc"),
            Object::String(OxStr::from(description.as_str())),
        ));
    }
    Dict(entries)
}

fn resolve_buffer(session: &ApiSession, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    session.with_editor(|editor| {
        if buffer.is_current() {
            return editor
                .current_buffer()
                .ok_or_else(|| exception("No current buffer"));
        }
        editor.buffer(buffer).map_err(exception)?;
        Ok(buffer)
    })
}

#[api(since = 6)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded keymap arguments"
)]
pub fn nvim_set_keymap(
    session: &ApiSession,
    mode: OxStr,
    lhs: OxStr,
    rhs: OxStr,
    opts: Dict,
) -> Result<(), ApiError> {
    set_keymap(session, MapScope::Global, &mode, &lhs, &rhs, &opts)
}

#[api(since = 6)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded keymap arguments"
)]
pub fn nvim_del_keymap(session: &ApiSession, mode: OxStr, lhs: OxStr) -> Result<(), ApiError> {
    del_keymap(session, MapScope::Global, &mode, &lhs)
}

#[api(since = 3)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded keymap arguments"
)]
pub fn nvim_get_keymap(session: &ApiSession, mode: OxStr) -> Result<Vec<Object>, ApiError> {
    keymap_array(session, MapScope::Global, &mode)
}

#[api(since = 6)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded keymap arguments"
)]
pub fn nvim_buf_set_keymap(
    session: &ApiSession,
    buffer: BufHandle,
    mode: OxStr,
    lhs: OxStr,
    rhs: OxStr,
    opts: Dict,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    set_keymap(session, MapScope::Buffer(buffer), &mode, &lhs, &rhs, &opts)
}

#[api(since = 6)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded keymap arguments"
)]
pub fn nvim_buf_del_keymap(
    session: &ApiSession,
    buffer: BufHandle,
    mode: OxStr,
    lhs: OxStr,
) -> Result<(), ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    del_keymap(session, MapScope::Buffer(buffer), &mode, &lhs)
}

#[api(since = 3)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC dispatcher owns decoded keymap arguments"
)]
pub fn nvim_buf_get_keymap(
    session: &ApiSession,
    buffer: BufHandle,
    mode: OxStr,
) -> Result<Vec<Object>, ApiError> {
    let buffer = resolve_buffer(session, buffer)?;
    keymap_array(session, MapScope::Buffer(buffer), &mode)
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_set_keymap__API_META(), nvim_set_keymap__API_DISPATCH)?;
    registry.register(nvim_del_keymap__API_META(), nvim_del_keymap__API_DISPATCH)?;
    registry.register(nvim_get_keymap__API_META(), nvim_get_keymap__API_DISPATCH)?;
    registry.register(
        nvim_buf_set_keymap__API_META(),
        nvim_buf_set_keymap__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_del_keymap__API_META(),
        nvim_buf_del_keymap__API_DISPATCH,
    )?;
    registry.register(
        nvim_buf_get_keymap__API_META(),
        nvim_buf_get_keymap__API_DISPATCH,
    )?;
    Ok(())
}
