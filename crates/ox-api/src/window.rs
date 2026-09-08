use std::collections::BTreeMap;

use ox_editor::{
    Anchor, Border, BorderText, BufferRelease, BufferState, Editor, Extmark, ExtmarkPosition,
    ExtmarkVirtualLinesOverflow, ExtmarkVirtualTextPosition, Margins, OptionStore, OptionValue,
    RelativeTo, TextAlignment, VirtualTextChunk, WinConfig,
};
use ox_text::{Buffer, Position};
use unicode_width::UnicodeWidthChar;

use crate::{
    ApiError, BufHandle, Dict, LuaRef, Object, OxStr, Registry, RegistryError, TabHandle,
    WinHandle, api, session::ApiSession,
};

fn exception(error: impl std::fmt::Display) -> ApiError {
    ApiError::exception(error.to_string())
}

fn invalid(field: &str, message: impl std::fmt::Display) -> ApiError {
    ApiError::validation(format!("Invalid 'config.{field}': {message}"))
}

fn api_integer(value: usize, what: &str) -> Result<i64, ApiError> {
    i64::try_from(value).map_err(|_| exception(format!("{what} exceeds API integer range")))
}

fn key<'a>(dict: &'a Dict, name: &str) -> Option<&'a Object> {
    dict.iter()
        .find(|(candidate, _)| candidate.as_bytes() == name.as_bytes())
        .map(|(_, value)| value)
}

fn resolve_window(session: &ApiSession, window: WinHandle) -> Result<WinHandle, ApiError> {
    session.with_editor(|editor| {
        let resolved = if window.is_current() {
            editor
                .current_window()
                .ok_or_else(|| ApiError::exception("No current window"))?
        } else {
            window
        };
        editor.window(resolved).map_err(exception)?;
        Ok(resolved)
    })
}

fn resolve_buffer(session: &ApiSession, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    session.with_editor(|editor| {
        if buffer.is_current() {
            return editor
                .current_buffer()
                .ok_or_else(|| ApiError::exception("No current buffer"));
        }
        editor.buffer(buffer).map_err(exception)?;
        Ok(buffer)
    })
}

fn window_tabpage(session: &ApiSession, window: WinHandle) -> Result<TabHandle, ApiError> {
    session.with_editor(|editor| editor.window_tabpage(window).map_err(exception))
}

fn option_to_object(value: &OptionValue) -> Object {
    match value {
        OptionValue::Boolean(value) => Object::Boolean(*value),
        OptionValue::Number(value) => Object::Integer(*value),
        OptionValue::String(value) => Object::String(OxStr::from(value.as_str())),
    }
}

fn integer(dict: &Dict, name: &str, required: bool) -> Result<Option<i64>, ApiError> {
    match key(dict, name) {
        Some(Object::Integer(value)) => Ok(Some(*value)),
        Some(Object::Nil) | None if required => Err(invalid(name, "field is required")),
        Some(Object::Nil) | None => Ok(None),
        Some(_) => Err(invalid(name, "expected Integer")),
    }
}

fn positive_size(dict: &Dict, name: &str, required: bool) -> Result<Option<usize>, ApiError> {
    let Some(value) = integer(dict, name, required)? else {
        return Ok(None);
    };
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| invalid(name, "must be greater than zero"))
}

fn coordinate(dict: &Dict, name: &str, required: bool) -> Result<Option<f64>, ApiError> {
    const RADIX: i64 = 1_i64 << 32;

    let value = match key(dict, name) {
        Some(Object::Float(value)) => Some(*value),
        Some(Object::Integer(value)) => {
            let high = i32::try_from(value.div_euclid(RADIX))
                .map_err(|_| exception("Integer-to-float conversion invariant violated"))?;
            let low = u32::try_from(value.rem_euclid(RADIX))
                .map_err(|_| exception("Integer-to-float conversion invariant violated"))?;
            Some(f64::from(high).mul_add(4_294_967_296.0, f64::from(low)))
        }
        Some(Object::Nil) | None if required => {
            return Err(invalid(name, "field is required"));
        }
        Some(Object::Nil) | None => None,
        Some(_) => return Err(invalid(name, "expected Float or Integer")),
    };
    if value.is_some_and(|value| !value.is_finite()) {
        return Err(invalid(name, "must be finite"));
    }
    Ok(value)
}

fn string(dict: &Dict, name: &str) -> Result<Option<String>, ApiError> {
    match key(dict, name) {
        Some(Object::String(value)) => String::from_utf8(value.0.clone())
            .map(Some)
            .map_err(|_| invalid(name, "must be valid UTF-8")),
        Some(Object::Nil) | None => Ok(None),
        Some(_) => Err(invalid(name, "expected String")),
    }
}

fn parse_anchor(value: Option<&str>, default: Anchor) -> Result<Anchor, ApiError> {
    match value {
        None => Ok(default),
        Some("NW") => Ok(Anchor::NorthWest),
        Some("NE") => Ok(Anchor::NorthEast),
        Some("SW") => Ok(Anchor::SouthWest),
        Some("SE") => Ok(Anchor::SouthEast),
        Some(value) => Err(invalid("anchor", format!("invalid value: {value}"))),
    }
}

fn parse_relative(
    session: &ApiSession,
    dict: &Dict,
    default: Option<RelativeTo>,
) -> Result<RelativeTo, ApiError> {
    let relative = string(dict, "relative")?;
    let effective = relative.as_deref().or(match default {
        Some(RelativeTo::Editor) => Some("editor"),
        Some(RelativeTo::Cursor) => Some("cursor"),
        Some(RelativeTo::Window(_)) => Some("win"),
        None => None,
    });
    match effective {
        None => Err(invalid("relative", "field is required")),
        Some("editor") => Ok(RelativeTo::Editor),
        Some("cursor") => Ok(RelativeTo::Cursor),
        Some("win") => {
            let target = match key(dict, "win") {
                None | Some(Object::Nil) => match default {
                    Some(RelativeTo::Window(window)) => window,
                    _ => resolve_window(session, WinHandle::CURRENT)?,
                },
                Some(Object::Window(window)) => resolve_window(session, *window)?,
                Some(Object::Integer(window)) => WinHandle::try_from(*window)
                    .map_err(|error| invalid("win", error))
                    .and_then(|window| resolve_window(session, window))?,
                Some(_) => return Err(invalid("win", "expected Window")),
            };
            Ok(RelativeTo::Window(target))
        }
        Some("") => Err(ApiError::validation(
            "Unsupported window configuration transformation: tiled windows are not supported",
        )),
        Some(value) => Err(invalid("relative", format!("invalid value: {value}"))),
    }
}

fn parse_border_piece(value: &Object) -> Result<String, ApiError> {
    match value {
        Object::String(value) => String::from_utf8(value.0.clone())
            .map_err(|_| invalid("border", "characters must be valid UTF-8")),
        // `[character, highlight]` tuples (api.txt: "- border: (string|string[])",
        // "Each border side can specify an optional highlight"). The highlight
        // group is accepted and validated but not used: this editor does not
        // style border cells individually.
        Object::Array(items) if items.len() == 2 => {
            let (Object::String(character), Object::String(_highlight)) = (&items[0], &items[1])
            else {
                return Err(invalid(
                    "border",
                    "tuple items must be [character, highlight] strings",
                ));
            };
            String::from_utf8(character.0.clone())
                .map_err(|_| invalid("border", "characters must be valid UTF-8"))
        }
        _ => Err(invalid(
            "border",
            "array items must be strings or highlight tuples",
        )),
    }
}

fn parse_border(value: Option<&Object>, default: Border) -> Result<Border, ApiError> {
    match value {
        None | Some(Object::Nil) => Ok(default),
        Some(Object::String(value)) => match value.as_bytes() {
            b"" | b"none" => Ok(Border::None),
            b"single" => Ok(Border::Single),
            b"double" => Ok(Border::Double),
            b"rounded" => Ok(Border::Rounded),
            b"solid" => Ok(Border::Solid),
            b"shadow" => Ok(Border::Shadow),
            _ => Err(invalid("border", "invalid named border")),
        },
        Some(Object::Array(values)) => {
            if values.is_empty() || values.len() > 8 || 8 % values.len() != 0 {
                return Err(invalid(
                    "border",
                    "array length must be a non-zero divisor of 8",
                ));
            }
            let pieces = values
                .iter()
                .map(parse_border_piece)
                .collect::<Result<Vec<_>, _>>()?;
            let expanded = std::array::from_fn(|index| pieces[index % pieces.len()].clone());
            Ok(Border::Custom(expanded))
        }
        Some(_) => Err(invalid("border", "expected String or Array")),
    }
}

fn parse_alignment(
    dict: &Dict,
    name: &str,
    default: TextAlignment,
) -> Result<TextAlignment, ApiError> {
    match string(dict, name)?.as_deref() {
        None => Ok(default),
        Some("left") => Ok(TextAlignment::Left),
        Some("center") => Ok(TextAlignment::Center),
        Some("right") => Ok(TextAlignment::Right),
        Some(value) => Err(invalid(name, format!("invalid value: {value}"))),
    }
}

fn parse_border_text(
    dict: &Dict,
    name: &str,
    position_name: &str,
    default: Option<BorderText>,
) -> Result<Option<BorderText>, ApiError> {
    let Some(value) = key(dict, name) else {
        if key(dict, position_name).is_some_and(|value| !matches!(value, Object::Nil)) {
            return Err(invalid(position_name, format!("requires config.{name}")));
        }
        return Ok(default);
    };
    if matches!(value, Object::Nil) {
        return Ok(default);
    }
    let text = match value {
        Object::String(value) => {
            String::from_utf8(value.0.clone()).map_err(|_| invalid(name, "must be valid UTF-8"))?
        }
        Object::Array(chunks) => {
            let mut text = String::new();
            for chunk in chunks {
                match chunk {
                    Object::String(value) => text.push_str(
                        std::str::from_utf8(value.as_bytes())
                            .map_err(|_| invalid(name, "chunks must be valid UTF-8"))?,
                    ),
                    // `[text, highlight]` tuple chunks (api.txt: "- title:
                    // ... List should consist of `[text, highlight]` tuples").
                    // The highlight group is accepted but not used; the editor
                    // has no per-float title/footer highlight model.
                    Object::Array(items) if items.len() == 2 => {
                        let (Object::String(value), Object::String(_highlight)) =
                            (&items[0], &items[1])
                        else {
                            return Err(invalid(
                                name,
                                "tuple chunks must be [text, highlight] strings",
                            ));
                        };
                        text.push_str(
                            std::str::from_utf8(value.as_bytes())
                                .map_err(|_| invalid(name, "chunks must be valid UTF-8"))?,
                        );
                    }
                    _ => {
                        return Err(invalid(
                            name,
                            "array items must be strings or [text, highlight] tuples",
                        ));
                    }
                }
            }
            text
        }
        _ => return Err(invalid(name, "expected String or Array")),
    };
    let default_alignment = default
        .as_ref()
        .map_or(TextAlignment::Left, |text| text.alignment);
    Ok(Some(BorderText {
        text,
        alignment: parse_alignment(dict, position_name, default_alignment)?,
    }))
}

fn parse_margins(value: Option<&Object>, default: Margins) -> Result<Margins, ApiError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if matches!(value, Object::Nil) {
        return Ok(default);
    }
    let Object::Array(values) = value else {
        return Err(invalid("margins", "expected Array"));
    };
    if values.len() != 4 {
        return Err(invalid("margins", "expected [top, right, bottom, left]"));
    }
    let mut parsed = [0_usize; 4];
    for (index, value) in values.iter().enumerate() {
        let Object::Integer(value) = value else {
            return Err(invalid("margins", "items must be non-negative integers"));
        };
        parsed[index] = usize::try_from(*value)
            .map_err(|_| invalid("margins", "items must be non-negative integers"))?;
    }
    Ok(Margins {
        top: parsed[0],
        right: parsed[1],
        bottom: parsed[2],
        left: parsed[3],
    })
}

fn reject_unsupported_keys(dict: &Dict) -> Result<(), ApiError> {
    // The full documented nvim_open_win() config surface (api.txt:3970-4045).
    const SUPPORTED: &[&[u8]] = &[
        b"relative",
        b"win",
        b"anchor",
        b"row",
        b"col",
        b"width",
        b"height",
        b"zindex",
        b"border",
        b"title",
        b"title_pos",
        b"footer",
        b"footer_pos",
        b"margins",
        b"style",
        b"split",
        b"focusable",
        b"external",
        b"bufpos",
        b"hide",
        b"noautocmd",
    ];
    for (name, _) in dict.iter() {
        if !SUPPORTED
            .iter()
            .any(|supported| *supported == name.as_bytes())
        {
            return Err(ApiError::validation(format!(
                "Unsupported window configuration key: {}",
                name.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn boolean(dict: &Dict, name: &str) -> Result<Option<bool>, ApiError> {
    match key(dict, name) {
        Some(Object::Boolean(value)) => Ok(Some(*value)),
        Some(Object::Nil) | None => Ok(None),
        Some(_) => Err(invalid(name, "expected Boolean")),
    }
}

/// Validates the non-positional float config surface that this editor accepts
/// but does not otherwise act on: `style` (only "" and "minimal" are valid),
/// and the `focusable` / `hide` / `noautocmd` booleans.
fn validate_float_flags(dict: &Dict) -> Result<(), ApiError> {
    match string(dict, "style")?.as_deref() {
        None | Some("" | "minimal") => {}
        Some(value) => return Err(invalid("style", format!("invalid value: {value}"))),
    }
    boolean(dict, "focusable")?;
    boolean(dict, "hide")?;
    boolean(dict, "noautocmd")?;
    Ok(())
}

/// `external` needs the UI layer to display a top-level window; there is no
/// such layer in this editor, so report a typed `NotImplemented` error.
fn reject_external(dict: &Dict) -> Result<(), ApiError> {
    if boolean(dict, "external")? == Some(true) {
        return Err(ApiError::exception(
            "Not implemented: external floating windows require a UI layer",
        ));
    }
    Ok(())
}

/// Parses `bufpos` ([line, column], relative to the text of a `relative="win"`
/// window). Returns the tuple; the caller applies its row/col defaults.
fn parse_bufpos(dict: &Dict) -> Result<Option<(i64, i64)>, ApiError> {
    let Some(value) = key(dict, "bufpos") else {
        return Ok(None);
    };
    if matches!(value, Object::Nil) {
        return Ok(None);
    }
    let Object::Array(items) = value else {
        return Err(invalid("bufpos", "expected [line, column] array"));
    };
    // `bufpos` is a two-element [line, column] array (upstream
    // `parse_float_bufpos`); index only after bounding the length so a
    // one-element array yields a typed Validation error, never a panic.
    if items.len() != 2 {
        return Err(invalid(
            "bufpos",
            "expected [line, column] array of length 2",
        ));
    }
    let (Object::Integer(line), Object::Integer(col)) = (&items[0], &items[1]) else {
        return Err(invalid("bufpos", "expected [line, column] integers"));
    };
    Ok(Some((*line, *col)))
}

/// Four-way tiled split direction: `left`/`right` are vertical splits and
/// `above`/`below` are horizontal splits (upstream `kWinSplitLeft` etc.).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitDirection {
    Left,
    Right,
    Above,
    Below,
}

fn parse_config_split(dict: &Dict) -> Result<SplitDirection, ApiError> {
    let Some(direction) = string(dict, "split")? else {
        return Err(invalid("split", "field is required"));
    };
    match direction.as_str() {
        "left" => Ok(SplitDirection::Left),
        "right" => Ok(SplitDirection::Right),
        "above" => Ok(SplitDirection::Above),
        "below" => Ok(SplitDirection::Below),
        value => Err(invalid("split", format!("invalid value: {value}"))),
    }
}

fn parse_config(
    session: &ApiSession,
    dict: &Dict,
    current: Option<&WinConfig>,
) -> Result<WinConfig, ApiError> {
    reject_unsupported_keys(dict)?;
    reject_external(dict)?;
    validate_float_flags(dict)?;
    let relative = parse_relative(session, dict, current.map(|config| config.relative))?;
    let anchor = string(dict, "anchor")?;
    let anchor = parse_anchor(
        anchor.as_deref(),
        current.map_or(Anchor::NorthWest, |config| config.anchor),
    )?;
    // `bufpos` ([line, column]) anchors the float to buffer text of a
    // `relative="win"` window and supplies row/col defaults when those are
    // absent (api.txt: "- bufpos:"). Source: nvim/api/win_config.c:1307-1320.
    let bufpos = parse_bufpos(dict)?.or_else(|| current.and_then(|config| config.bufpos));
    if bufpos.is_some() && !matches!(relative, RelativeTo::Window(_)) {
        return Err(invalid("bufpos", "only valid when relative is 'win'"));
    }
    let mut row = coordinate(dict, "row", false)?;
    let mut col = coordinate(dict, "col", false)?;
    if bufpos.is_some() {
        if row.is_none() {
            row = Some(if matches!(anchor, Anchor::SouthWest | Anchor::SouthEast) {
                0.0
            } else {
                1.0
            });
        }
        if col.is_none() {
            col = Some(0.0);
        }
    }
    let row = row
        .or_else(|| current.map(|config| config.row))
        .ok_or_else(|| invalid("row", "field is required"))?;
    let col = col
        .or_else(|| current.map(|config| config.col))
        .ok_or_else(|| invalid("col", "field is required"))?;
    let width = positive_size(dict, "width", current.is_none())?
        .or_else(|| current.map(|config| config.width))
        .ok_or_else(|| invalid("width", "field is required"))?;
    let height = positive_size(dict, "height", current.is_none())?
        .or_else(|| current.map(|config| config.height))
        .ok_or_else(|| invalid("height", "field is required"))?;
    let zindex = match integer(dict, "zindex", false)? {
        Some(value) => u32::try_from(value)
            .map_err(|_| invalid("zindex", "must be a non-negative 32-bit integer"))?,
        None => current.map_or(50, |config| config.zindex),
    };
    let border = parse_border(
        key(dict, "border"),
        current.map_or(Border::None, |config| config.border.clone()),
    )?;
    let title = parse_border_text(
        dict,
        "title",
        "title_pos",
        current.and_then(|config| config.title.clone()),
    )?;
    let footer = parse_border_text(
        dict,
        "footer",
        "footer_pos",
        current.and_then(|config| config.footer.clone()),
    )?;
    if matches!(border, Border::None) && (title.is_some() || footer.is_some()) {
        return Err(ApiError::validation(
            "Window title or footer requires a border",
        ));
    }
    let margins = parse_margins(
        key(dict, "margins"),
        current.map_or_else(Margins::default, |config| config.margins),
    )?;
    let config = WinConfig {
        relative,
        anchor,
        row,
        col,
        width,
        height,
        zindex,
        border,
        title,
        footer,
        margins,
        bufpos,
    };
    config.validate().map_err(exception)?;
    Ok(config)
}

fn text_alignment(value: TextAlignment) -> &'static str {
    match value {
        TextAlignment::Left => "left",
        TextAlignment::Center => "center",
        TextAlignment::Right => "right",
    }
}

/// Packs eight border characters into an API array.
fn border_chars(chars: [&str; 8]) -> Object {
    Object::Array(
        chars
            .iter()
            .map(|char| Object::String(OxStr::from(*char)))
            .collect(),
    )
}

fn config_to_dict(config: Option<&WinConfig>) -> Result<Dict, ApiError> {
    let Some(config) = config else {
        return Ok(Dict(vec![(
            OxStr::from("relative"),
            Object::String(OxStr::from("")),
        )]));
    };
    let (relative, target) = match config.relative {
        RelativeTo::Editor => ("editor", None),
        RelativeTo::Cursor => ("cursor", None),
        RelativeTo::Window(window) => ("win", Some(window)),
    };
    let anchor = match config.anchor {
        Anchor::NorthWest => "NW",
        Anchor::NorthEast => "NE",
        Anchor::SouthWest => "SW",
        Anchor::SouthEast => "SE",
    };
    // `nvim_win_get_config` reports the eight border characters, not the
    // style name (api/win_config.c:892-908). Tables mirror the `defaults`
    // in `parse_border_style`, ordered top-left, top, top-right, right,
    // bottom-right, bottom, bottom-left, left.
    let border = match &config.border {
        Border::None => Object::String(OxStr::from("none")),
        Border::Single => border_chars(["┌", "─", "┐", "│", "┘", "─", "└", "│"]),
        Border::Double => border_chars(["╔", "═", "╗", "║", "╝", "═", "╚", "║"]),
        Border::Rounded => border_chars(["╭", "─", "╮", "│", "╯", "─", "╰", "│"]),
        Border::Solid => border_chars([" ", " ", " ", " ", " ", " ", " ", " "]),
        Border::Shadow => border_chars(["", "", " ", " ", " ", " ", " ", ""]),
        Border::Custom(parts) => Object::Array(
            parts
                .iter()
                .map(|part| Object::String(OxStr::from(part.as_str())))
                .collect(),
        ),
    };
    let mut result = Dict(vec![
        (
            OxStr::from("relative"),
            Object::String(OxStr::from(relative)),
        ),
        (OxStr::from("anchor"), Object::String(OxStr::from(anchor))),
        (OxStr::from("row"), Object::Float(config.row)),
        (OxStr::from("col"), Object::Float(config.col)),
        (
            OxStr::from("width"),
            Object::Integer(api_integer(config.width, "Window width")?),
        ),
        (
            OxStr::from("height"),
            Object::Integer(api_integer(config.height, "Window height")?),
        ),
        (
            OxStr::from("zindex"),
            Object::Integer(i64::from(config.zindex)),
        ),
        (OxStr::from("border"), border),
        (
            OxStr::from("margins"),
            Object::Array(vec![
                Object::Integer(api_integer(config.margins.top, "Window margin")?),
                Object::Integer(api_integer(config.margins.right, "Window margin")?),
                Object::Integer(api_integer(config.margins.bottom, "Window margin")?),
                Object::Integer(api_integer(config.margins.left, "Window margin")?),
            ]),
        ),
    ]);
    if let Some(target) = target {
        result.insert(OxStr::from("win"), Object::Window(target));
    }
    if let Some((line, col)) = config.bufpos {
        result.insert(
            OxStr::from("bufpos"),
            Object::Array(vec![Object::Integer(line), Object::Integer(col)]),
        );
    }
    if let Some(title) = &config.title {
        result.insert(
            OxStr::from("title"),
            Object::String(OxStr::from(title.text.as_str())),
        );
        result.insert(
            OxStr::from("title_pos"),
            Object::String(OxStr::from(text_alignment(title.alignment))),
        );
    }
    if let Some(footer) = &config.footer {
        result.insert(
            OxStr::from("footer"),
            Object::String(OxStr::from(footer.text.as_str())),
        );
        result.insert(
            OxStr::from("footer_pos"),
            Object::String(OxStr::from(text_alignment(footer.alignment))),
        );
    }
    Ok(result)
}

fn set_dimension(
    session: &ApiSession,
    window: WinHandle,
    width: Option<usize>,
    height: Option<usize>,
) -> Result<(), ApiError> {
    let window = resolve_window(session, window)?;
    session.with_editor_mut(|editor| {
        if let Some(width) = width {
            editor.set_window_width(window, width).map_err(exception)?;
        }
        if let Some(height) = height {
            editor
                .set_window_height(window, height)
                .map_err(exception)?;
        }
        Ok(())
    })
}

#[api(since = 1, method)]
pub fn nvim_win_get_buf(session: &ApiSession, win: WinHandle) -> Result<BufHandle, ApiError> {
    let win = resolve_window(session, win)?;
    session.with_editor(|editor| Ok(editor.window(win).map_err(exception)?.buffer))
}

#[api(since = 5, textlock, method)]
pub fn nvim_win_set_buf(
    session: &ApiSession,
    win: WinHandle,
    buf: BufHandle,
) -> Result<(), ApiError> {
    let win = resolve_window(session, win)?;
    let buf = resolve_buffer(session, buf)?;
    session.with_editor_mut(|editor| {
        editor
            .set_window_buffer(win, buf, BufferRelease::KeepLoaded)
            .map_err(exception)
    })
}

#[api(since = 1, method)]
pub fn nvim_win_get_cursor(session: &ApiSession, win: WinHandle) -> Result<Vec<i64>, ApiError> {
    let win = resolve_window(session, win)?;
    let cursor = session.with_editor(|editor| -> Result<Position, ApiError> {
        Ok(editor.window(win).map_err(exception)?.cursor)
    })?;
    Ok(vec![
        api_integer(cursor.lnum, "Cursor line")?,
        api_integer(cursor.col, "Cursor column")?,
    ])
}

/// Largest valid cursor column (upstream `MAXCOL`, src/nvim/pos_defs.h:17-19).
const MAXCOL: i64 = 0x7fff_ffff;

#[api(since = 1, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded array arguments"
)]
pub fn nvim_win_set_cursor(
    session: &ApiSession,
    win: WinHandle,
    pos: Vec<i64>,
) -> Result<(), ApiError> {
    if pos.len() != 2 {
        return Err(ApiError::validation(
            "Cursor position must have exactly two items",
        ));
    }
    let win = resolve_window(session, win)?;
    let position = session.with_editor(|editor| -> Result<Position, ApiError> {
        let buffer = editor.window(win).map_err(exception)?.buffer;
        let text = editor
            .buffer(buffer)
            .map_err(exception)?
            .text()
            .map_err(exception)?;
        let row = usize::try_from(pos[0])
            .ok()
            .filter(|row| (1..=text.line_count()).contains(row))
            .ok_or_else(|| ApiError::validation("Cursor row outside buffer"))?;
        let line = text.line(row).map_err(exception)?;
        // Source: src/nvim/api/window.c:122-130 and src/nvim/pos_defs.h:17-19.
        // `MAXCOL` is the largest valid cursor column; values above it (or
        // negative) are rejected upstream before the column is silently clamped
        // to the line length (check_cursor_col).
        if pos[1] < 0 || pos[1] > MAXCOL {
            return Err(ApiError::validation("Invalid cursor column: out of range"));
        }
        let col = usize::try_from(pos[1])
            .map_err(|_| exception("Cursor column exceeds addressable range"))?
            .min(line.len());
        Ok(Position { lnum: row, col })
    })?;
    session.with_editor_mut(|editor| editor.set_window_cursor(win, position).map_err(exception))
}

#[api(since = 1, method)]
pub fn nvim_win_get_height(session: &ApiSession, win: WinHandle) -> Result<i64, ApiError> {
    let win = resolve_window(session, win)?;
    session.with_editor(|editor| {
        if let Some(config) = editor.window_config(win).map_err(exception)? {
            return i64::try_from(config.height)
                .map_err(|_| ApiError::exception("Window height exceeds API integer range"));
        }
        i64::try_from(editor.window_geometry(win).map_err(exception)?.height)
            .map_err(|_| ApiError::exception("Window height exceeds API integer range"))
    })
}

#[api(since = 1, deprecated_since = 15, method)]
pub fn nvim_win_set_height(
    session: &ApiSession,
    win: WinHandle,
    height: i64,
) -> Result<(), ApiError> {
    let height = usize::try_from(height)
        .ok()
        .filter(|height| *height > 0)
        .ok_or_else(|| ApiError::validation("Height must be greater than zero"))?;
    set_dimension(session, win, None, Some(height))
}

#[api(since = 1, method)]
pub fn nvim_win_get_width(session: &ApiSession, win: WinHandle) -> Result<i64, ApiError> {
    let win = resolve_window(session, win)?;
    session.with_editor(|editor| {
        if let Some(config) = editor.window_config(win).map_err(exception)? {
            return i64::try_from(config.width)
                .map_err(|_| ApiError::exception("Window width exceeds API integer range"));
        }
        i64::try_from(editor.window_geometry(win).map_err(exception)?.width)
            .map_err(|_| ApiError::exception("Window width exceeds API integer range"))
    })
}

#[api(since = 1, deprecated_since = 15, method)]
pub fn nvim_win_set_width(
    session: &ApiSession,
    win: WinHandle,
    width: i64,
) -> Result<(), ApiError> {
    let width = usize::try_from(width)
        .ok()
        .filter(|width| *width > 0)
        .ok_or_else(|| ApiError::validation("Width must be greater than zero"))?;
    set_dimension(session, win, Some(width), None)
}

#[api(since = 1, method)]
pub fn nvim_win_get_position(session: &ApiSession, win: WinHandle) -> Result<Vec<i64>, ApiError> {
    let win = resolve_window(session, win)?;
    let geometry = session.with_editor(|editor| editor.window_geometry(win).map_err(exception))?;
    Ok(vec![
        api_integer(geometry.row, "Window row")?,
        api_integer(geometry.col, "Window column")?,
    ])
}

#[api(since = 1, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded string arguments"
)]
pub fn nvim_win_get_var(
    session: &ApiSession,
    win: WinHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let win = resolve_window(session, win)?;
    session.with_editor(|editor| {
        editor
            .window_variables(win)
            .map_err(exception)?
            .get(&name)
            .cloned()
            .ok_or_else(|| {
                ApiError::exception(format!("Key not found: {}", name.to_string_lossy()))
            })
    })
}

#[api(since = 1, method)]
pub fn nvim_win_set_var(
    session: &ApiSession,
    win: WinHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let win = resolve_window(session, win)?;
    session.with_editor_mut(|editor| {
        editor
            .window_variables_mut(win)
            .map_err(exception)?
            .insert(name, value);
        Ok(())
    })
}

#[api(since = 1, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded string arguments"
)]
pub fn nvim_win_del_var(session: &ApiSession, win: WinHandle, name: OxStr) -> Result<(), ApiError> {
    let win = resolve_window(session, win)?;
    session.with_editor_mut(|editor| {
        let variables = editor.window_variables_mut(win).map_err(exception)?;
        let Some(index) = variables
            .iter()
            .position(|(candidate, _)| candidate == &name)
        else {
            return Err(ApiError::exception(format!(
                "Key not found: {}",
                name.to_string_lossy()
            )));
        };
        variables.0.remove(index);
        Ok(())
    })
}

#[api(since = 1, deprecated_since = 11, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded string arguments"
)]
pub fn nvim_win_get_option(
    session: &ApiSession,
    window: WinHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let window = resolve_window(session, window)?;
    let name = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Option name must be valid UTF-8"))?;
    session.with_editor(|editor| {
        editor
            .options()
            .get_window(window, name)
            .map(option_to_object)
            .map_err(exception)
    })
}

#[api(since = 1, deprecated_since = 11, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded string arguments"
)]
pub fn nvim_win_set_option(
    session: &ApiSession,
    window: WinHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let window = resolve_window(session, window)?;
    let name = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Option name must be valid UTF-8"))?;
    let metadata = ox_editor::OptionStore::metadata(name).map_err(exception)?;
    let value = crate::global::object_to_legacy_option_value(metadata, name, value)?;
    session.with_editor_mut(|editor| {
        editor
            .options_mut()
            .set_window(window, name, value)
            .map_err(exception)
    })
}

#[api(since = 1, method)]
pub fn nvim_win_get_tabpage(session: &ApiSession, win: WinHandle) -> Result<TabHandle, ApiError> {
    let win = resolve_window(session, win)?;
    window_tabpage(session, win)
}

#[api(since = 1, method)]
pub fn nvim_win_get_number(session: &ApiSession, win: WinHandle) -> Result<i64, ApiError> {
    let win = resolve_window(session, win)?;
    let tab = window_tabpage(session, win)?;
    session.with_editor(|editor| {
        let windows = editor.tabpage(tab).map_err(exception)?.windows();
        let index = windows
            .iter()
            .position(|candidate| *candidate == win)
            .ok_or_else(|| ApiError::exception("Window is not in its owning tabpage"))?;
        let number = api_integer(index, "Window number")?
            .checked_add(1)
            .ok_or_else(|| ApiError::exception("Window number exceeds API integer range"))?;
        Ok(number)
    })
}

#[api(since = 1, method)]
pub fn nvim_win_is_valid(session: &ApiSession, win: WinHandle) -> Result<bool, ApiError> {
    if win.is_current() {
        return Ok(resolve_window(session, win).is_ok());
    }
    session.with_editor(|editor| Ok(editor.window(win).is_ok()))
}

#[api(since = 7, textlock, method)]
pub fn nvim_win_hide(session: &ApiSession, win: WinHandle) -> Result<(), ApiError> {
    let win = resolve_window(session, win)?;
    let tab = window_tabpage(session, win)?;
    session.with_editor_mut(|editor| {
        editor.close_window(tab, win, true).map_err(exception)?;
        Ok(())
    })
}

#[api(since = 6, textlock, method)]
pub fn nvim_win_close(session: &ApiSession, win: WinHandle, force: bool) -> Result<(), ApiError> {
    let win = resolve_window(session, win)?;
    let tab = window_tabpage(session, win)?;
    // Buffer modified-state is not modeled yet. Closing still follows normal
    // hidden-buffer retention; `force` has no observable distinction until it is.
    let _ = force;
    session.with_editor_mut(|editor| {
        editor.close_window(tab, win, true).map_err(exception)?;
        Ok(())
    })
}

#[api(since = 7, method)]
pub fn nvim_win_call(
    session: &ApiSession,
    win: WinHandle,
    function: LuaRef,
) -> Result<Object, ApiError> {
    let win = resolve_window(session, win)?;
    let reference = usize::try_from(function.0)
        .map_err(|_| ApiError::exception("Lua callback reference is out of range"))?;
    // The Lua function runs with `win` current (`nvim/api/window.c:269-286`).
    // The context switch is session-side save/restore: host closures must not
    // hold an editor borrow across user code, so the window swap is one
    // statement on each side of the executor checkout. Entry and restore
    // failures are swallowed so the callback result is never masked, matching
    // the restore contract of `Editor::with_window_context`.
    let caller = session
        .with_editor(Editor::current_window)
        .ok_or_else(|| exception("no current tabpage"))?;
    if caller != win {
        let _ = session.with_editor_mut(|editor| editor.set_current_window(win));
    }
    let outcome = crate::runtime::with_lua_executor(session, |session, executor| {
        executor
            .call_ref(session, reference, Vec::new())
            .map_err(ApiError::exception)
    });
    if session.with_editor(Editor::current_window) != Some(caller)
        && session.with_editor(|editor| editor.window(caller).is_ok())
    {
        let _ = session.with_editor_mut(|editor| editor.set_current_window(caller));
    }
    // The caller's argument reference is consumed exactly once, after every
    // value it produced has been copied out (or the failure recorded).
    crate::runtime::release_lua_callback(session, reference);
    // Array is the internal retstack carrier. The Lua binding expands it and
    // therefore preserves the distinction between no return and one nil.
    Ok(Object::Array(outcome?))
}

#[api(since = 10, method)]
pub fn nvim_win_set_hl_ns(
    session: &ApiSession,
    win: WinHandle,
    ns_id: i64,
) -> Result<(), ApiError> {
    if ns_id < -1 {
        return Err(ApiError::validation(
            "Namespace must be greater than or equal to -1",
        ));
    }
    let win = resolve_window(session, win)?;
    session.with_editor_mut(|editor| {
        editor
            .set_window_highlight_namespace(win, ns_id)
            .map_err(exception)
    })
}

#[api(since = 6, textlock)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded config dictionaries"
)]
pub fn nvim_open_win(
    session: &ApiSession,
    buf: BufHandle,
    enter: bool,
    config: Dict,
) -> Result<WinHandle, ApiError> {
    let buffer = resolve_buffer(session, buf)?;
    reject_unsupported_keys(&config)?;
    reject_external(&config)?;
    // A `split` config creates a normal (tiled) split window instead of a
    // floating one (api.txt: "- split:", nvim/api/win_config.c:231-244).
    if key(&config, "split").is_some() {
        validate_float_flags(&config)?;
        return open_split_window(session, buffer, enter, &config);
    }
    let config = parse_config(session, &config, None)?;
    session.with_editor_mut(|editor| {
        let tab = editor
            .current_tabpage()
            .ok_or_else(|| ApiError::exception("no current tabpage"))?;
        let window = editor.open_float(tab, buffer, config).map_err(exception)?;
        if enter {
            editor.set_current_window(window).map_err(exception)?;
        }
        Ok(window)
    })
}

/// Resolves the window `config.win` selects as the split target, defaulting to
/// the current window. The target may live on any tabpage (upstream: "Can be
/// in a different tab page"). Splitting a floating window is rejected
/// (`nvim/api/win_config.c`: "Cannot split a floating window").
fn parse_split_target(session: &ApiSession, config: &Dict) -> Result<WinHandle, ApiError> {
    let target = match key(config, "win") {
        None | Some(Object::Nil) => resolve_window(session, WinHandle::CURRENT)?,
        Some(Object::Window(window)) => resolve_window(session, *window)?,
        Some(Object::Integer(window)) => WinHandle::try_from(*window)
            .map_err(|error| invalid("win", error))
            .and_then(|window| resolve_window(session, window))?,
        Some(_) => return Err(invalid("win", "expected Window")),
    };
    let floating = session.with_editor(|editor| -> Result<bool, ApiError> {
        Ok(editor.window_config(target).map_err(exception)?.is_some())
    })?;
    if floating {
        return Err(ApiError::exception("Cannot split a floating window"));
    }
    Ok(target)
}

/// Opens a tiled split for a `config.split` request. The target window honors
/// `config.win` (any tabpage) and the four-way direction follows upstream:
/// `left`/`right` split vertically with the new window before/after, and
/// `above`/`below` split horizontally with the new window before/after.
fn open_split_window(
    session: &ApiSession,
    buffer: BufHandle,
    enter: bool,
    config: &Dict,
) -> Result<WinHandle, ApiError> {
    let direction = parse_config_split(config)?;
    let target = parse_split_target(session, config)?;
    // `target` may belong to a non-current tabpage; split it there.
    let tab = window_tabpage(session, target)?;
    let window = session
        .with_editor_mut(|editor| match direction {
            SplitDirection::Left => editor.split_left(tab, target, buffer, enter),
            SplitDirection::Right => editor.split_vertical(tab, target, buffer, enter),
            SplitDirection::Above => editor.split_above(tab, target, buffer, enter),
            SplitDirection::Below => editor.split_horizontal(tab, target, buffer, enter),
        })
        .map_err(exception)?;
    let width = positive_size(config, "width", false)?;
    let height = positive_size(config, "height", false)?;
    set_dimension(session, window, width, height)?;
    if enter {
        session.with_editor_mut(|editor| editor.set_current_window(window).map_err(exception))?;
    }
    Ok(window)
}

#[api(since = 6, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded config dictionaries"
)]
pub fn nvim_win_set_config(
    session: &ApiSession,
    win: WinHandle,
    config: Dict,
) -> Result<(), ApiError> {
    let win = resolve_window(session, win)?;
    let current = session.with_editor(|editor| {
        editor
            .window_config(win)
            .map_err(exception)?
            .cloned()
            .ok_or_else(|| {
                ApiError::validation(
                    "Unsupported window configuration transformation: tiled to floating",
                )
            })
    })?;
    let updated = parse_config(session, &config, Some(&current))?;
    session.with_editor_mut(|editor| editor.set_window_config(win, updated).map_err(exception))
}

#[api(since = 6, method)]
pub fn nvim_win_get_config(session: &ApiSession, win: WinHandle) -> Result<Dict, ApiError> {
    let win = resolve_window(session, win)?;
    session.with_editor(|editor| config_to_dict(editor.window_config(win).map_err(exception)?))
}

// ---------------------------------------------------------------------------
// Window resize (`nvim_win_resize`, api/window.c:557-603) and window text
// height (`nvim_win_text_height`, api/window.c:455-541 with the plines.c core).
// ---------------------------------------------------------------------------

/// Column cap for the line-size walk (`MAXCOL`, pos_defs.h:17-19, used by
/// plines.c:843).
const MAX_COLUMN: i64 = i32::MAX as i64;

/// Cells charged for one undecodable byte (`kInvalidByteCells`, mbyte.c).
const INVALID_BYTE_CELLS: i64 = 4;

/// Rows reported for an unmeasurable line in a zero-width text area
/// (`plines_win_nofold`, plines.c:857).
const ZERO_TEXT_WIDTH_ROWS: i64 = 32000;

/// Display cells per sign in the sign column (`SIGN_WIDTH`, `types_defs.h:59`).
const SIGN_WIDTH: i64 = 2;

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

fn cell_count(cells: usize) -> i64 {
    i64::try_from(cells).unwrap_or(MAX_COLUMN)
}

fn option_number(options: &OptionStore, name: &str, default: i64) -> i64 {
    match options.get_global(name) {
        Ok(OptionValue::Number(value)) => *value,
        _ => default,
    }
}

fn option_string(options: &OptionStore, name: &str) -> String {
    match options.get_global(name) {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    }
}

fn buffer_option_number(options: &OptionStore, buffer: BufHandle, name: &str) -> i64 {
    match options.get_buffer(buffer, name) {
        Ok(OptionValue::Number(value)) => *value,
        _ => 0,
    }
}

fn window_option_number(options: &OptionStore, win: WinHandle, name: &str) -> i64 {
    match options.get_window(win, name) {
        Ok(OptionValue::Number(value)) => *value,
        _ => 0,
    }
}

fn window_option_string(options: &OptionStore, win: WinHandle, name: &str) -> String {
    match options.get_window(win, name) {
        Ok(OptionValue::String(value)) => value.clone(),
        _ => String::new(),
    }
}

fn window_option_flag(options: &OptionStore, win: WinHandle, name: &str) -> bool {
    match options.get_window(win, name) {
        Ok(OptionValue::Boolean(value)) => *value,
        Ok(OptionValue::Number(value)) => *value != 0,
        _ => false,
    }
}

/// `nvim_win_resize` keyset validation (`keydict`, helpers.c:803-898): only
/// "anchor" (String) is accepted, checked in dict order before the function
/// body runs.
fn resize_anchor(opts: &Dict) -> Result<Option<String>, ApiError> {
    let mut anchor = None;
    for (key, value) in opts.iter() {
        if key.as_bytes() != b"anchor" {
            return Err(ApiError::validation(format!(
                "Invalid key: '{}'",
                key.to_string_lossy()
            )));
        }
        let Object::String(value) = value else {
            return Err(ApiError::validation(format!(
                "Invalid 'anchor': expected String, got {}",
                object_type(value)
            )));
        };
        anchor = Some(value.to_string_lossy().into_owned());
    }
    Ok(anchor)
}

/// Whether the window shows a winbar row (`set_winbar_win`, window.c:7344):
/// floating windows require a window-local 'winbar', tiled windows also
/// accept the global fallback. The option store cannot report local-ness, so
/// floats compare the effective value against the global baseline.
fn winbar_shown(options: &ox_editor::OptionStore, win: WinHandle, floating: bool) -> bool {
    let shown = window_option_string(options, win, "winbar");
    if !floating {
        return !shown.is_empty();
    }
    let baseline = options.get_global_baseline("winbar").ok();
    let effective = options.get_window(win, "winbar").ok();
    !shown.is_empty() && baseline != effective
}

#[api(since = 15, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded dict arguments"
)]
pub fn nvim_win_resize(
    session: &ApiSession,
    win: WinHandle,
    width: i64,
    height: i64,
    opts: Dict,
) -> Result<(), ApiError> {
    let anchor = resize_anchor(&opts)?;
    let win = resolve_window(session, win)?;
    // VALIDATE_EXP, api/window.c:566-571: -1 keeps a dimension unchanged.
    if !(height >= 0 || height == -1) {
        return Err(ApiError::validation(
            "Invalid 'height': expected non-negative number or -1",
        ));
    }
    if !(width >= 0 || width == -1) {
        return Err(ApiError::validation(
            "Invalid 'width': expected non-negative number or -1",
        ));
    }
    // VALIDATE_R, api/window.c:573-575.
    if height == -1 && width == -1 {
        return Err(ApiError::validation("Required: 'height' or 'width'"));
    }
    let mut from_top = true;
    let mut from_left = true;
    if let Some(anchor) = anchor {
        let is_height = anchor == "top" || anchor == "bottom";
        let is_width = anchor == "left" || anchor == "right";
        if !(is_height || is_width) {
            return Err(ApiError::validation(format!(
                "Invalid 'anchor': expected \"top\", \"bottom\", \"left\" or \"right\", got \
                 {anchor}"
            )));
        }
        // VALIDATE_CON, api/window.c:589-592: the anchor must match a resized
        // dimension.
        if (is_height && height == -1) || (is_width && width == -1) {
            let other = if is_width { "width" } else { "height" };
            return Err(ApiError::validation(format!(
                "Conflict: '{anchor}' not allowed with '{other}'"
            )));
        }
        from_top = anchor != "bottom";
        from_left = anchor != "right";
    }
    let _ = (from_top, from_left);
    session.with_editor_mut(|editor| {
        let floating = editor.window_config(win).map_err(exception)?.is_some();
        let current = editor.current_window() == Some(win);
        if height >= 0 {
            // win_setheight_win (window.c:6242): the current window keeps at
            // least max('winminheight', 1) rows plus the winbar row; other
            // windows 'winminheight' rows plus the winbar row; floats at
            // least one row (window.c:6246).
            let minimum = option_number(editor.options(), "winminheight", 1);
            let minimum = if current { minimum.max(1) } else { minimum };
            let height =
                height.max(minimum + i64::from(winbar_shown(editor.options(), win, false)));
            let height = if floating { height.max(1) } else { height };
            let rows = usize::try_from(height).unwrap_or(usize::MAX);
            editor.set_window_height(win, rows).map_err(exception)?;
        }
        if width >= 0 {
            // win_setwidth_win (window.c:6415-6419): only the current window
            // is clamped to max('winminwidth', 1); floats keep the requested
            // width.
            let width = if current && !floating {
                width
                    .max(option_number(editor.options(), "winminwidth", 1))
                    .max(1)
            } else {
                width
            };
            let columns = usize::try_from(width).unwrap_or(usize::MAX);
            editor.set_window_width(win, columns).map_err(exception)?;
        }
        Ok(())
    })
}

/// `nvim_win_text_height` keyset fields (`Dict(win_text_height)`,
/// `api/keysets_defs.h`), all Integer.
struct TextHeightOpts {
    start_row: Option<i64>,
    end_row: Option<i64>,
    start_vcol: Option<i64>,
    end_vcol: Option<i64>,
    max_height: Option<i64>,
}

impl TextHeightOpts {
    /// `keydict` conversion (helpers.c:803-898): unknown keys and wrong value
    /// types fail in dict order, before the function body runs.
    fn parse(opts: &Dict) -> Result<Self, ApiError> {
        let mut parsed = Self {
            start_row: None,
            end_row: None,
            start_vcol: None,
            end_vcol: None,
            max_height: None,
        };
        for (key, value) in opts.iter() {
            let field = match key.as_bytes() {
                b"start_row" => &mut parsed.start_row,
                b"end_row" => &mut parsed.end_row,
                b"start_vcol" => &mut parsed.start_vcol,
                b"end_vcol" => &mut parsed.end_vcol,
                b"max_height" => &mut parsed.max_height,
                _ => {
                    return Err(ApiError::validation(format!(
                        "Invalid key: '{}'",
                        key.to_string_lossy()
                    )));
                }
            };
            let Object::Integer(value) = value else {
                return Err(ApiError::validation(format!(
                    "Invalid '{}': expected Integer, got {}",
                    key.to_string_lossy(),
                    object_type(value)
                )));
            };
            *field = Some(*value);
        }
        Ok(parsed)
    }
}

/// Window state the text-height walk measures (`win_T` screen fields,
/// plines.c:1020-1024 with the option-derived gutter widths).
#[expect(
    clippy::struct_excessive_bools,
    reason = "API option shape mirrors the independent win_T fields"
)]
struct WindowTextContext {
    line_count: usize,
    view_width: i64,
    col_off: i64,
    col_off2: i64,
    wrap: bool,
    list_eol: bool,
    use_tabstop: bool,
    tabstop: i64,
    showbreak_cells: i64,
    conceallevel: i64,
    cursor_row: usize,
    cursor_conceals: bool,
    foldenable: bool,
    marks: Vec<Extmark>,
}

fn digits(mut value: i64) -> i64 {
    // number_width's do-while digit count (drawscreen.c:2617-2621): 0 has
    // one digit.
    let mut count = 0;
    loop {
        value /= 10;
        count += 1;
        if value <= 0 {
            break;
        }
    }
    count
}

fn first_digit(value: &str) -> i64 {
    value
        .chars()
        .find_map(|character| character.to_digit(10))
        .map_or(0, i64::from)
}

fn string_cells(value: &str) -> i64 {
    value
        .chars()
        .map(|character| cell_count(UnicodeWidthChar::width(character).unwrap_or(1).max(1)))
        .sum()
}

/// 'signcolumn' bounds (optionstr.c set-time parsing): (minimum, maximum)
/// dedicated columns. "number" draws signs inside the number column and
/// contributes no dedicated columns; without numbers it degrades to "auto".
fn signcolumn_bounds(value: &str, numbers: bool) -> (i64, i64) {
    if let Some(rest) = value.strip_prefix("yes:") {
        let width = first_digit(rest);
        return (width, width);
    }
    if value == "yes" {
        return (1, 1);
    }
    if value == "no" {
        return (0, 0);
    }
    if value.starts_with("number") && numbers {
        return (0, 0);
    }
    if let Some(rest) = value.strip_prefix("auto:") {
        let rest = rest.trim_matches(|character| character == '[' || character == ']');
        return match rest.split_once('-') {
            Some((low, high)) => (first_digit(low), first_digit(high)),
            None => (0, first_digit(rest)),
        };
    }
    (0, 1)
}

/// Fold column width from 'foldcolumn' (`win_fdccol_count`, window.c:834-845):
/// "auto" tracks the deepest nesting, a fixed value counts as-is.
fn foldcolumn_columns(value: &str, deepest: i64) -> i64 {
    if let Some(rest) = value.strip_prefix("auto") {
        let requested = rest
            .strip_prefix(':')
            .and_then(|digits| digits.chars().next())
            .and_then(|digit| digit.to_digit(10))
            .map_or(1, i64::from);
        requested.min(deepest.max(0))
    } else {
        first_digit(value)
    }
}

/// `number_width` (drawscreen.c:2594-2636): the 'number'/'relativenumber'
/// column width, from the line count (or view height for relative-only),
/// 'numberwidth', and the "number" sign column.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "mirrors the independent win_T fields number_width reads"
)]
fn number_columns(
    line_count: usize,
    view_height: i64,
    number: bool,
    relative: bool,
    numberwidth: i64,
    minsc_number: bool,
    has_sign_marks: bool,
) -> i64 {
    let shown = if relative && !number {
        // The cursor line shows "0"; width tracks the view height.
        view_height
    } else {
        cell_count(line_count)
    };
    let mut width = digits(shown);
    width = width.max(numberwidth - 1);
    if width < 2 && has_sign_marks && minsc_number {
        width = 2;
    }
    width
}

/// `w_scwidth` (drawscreen.c:1169-1194): the largest per-row sign count
/// clamped into the 'signcolumn' bounds.
fn sign_column_width(marks: &[Extmark], bounds: (i64, i64)) -> i64 {
    let (min, max) = bounds;
    let mut per_row: BTreeMap<usize, i64> = BTreeMap::new();
    for mark in marks {
        if !mark.invalid && mark.placement.attributes.has_sign() {
            *per_row.entry(mark.position().row).or_insert(0) += 1;
        }
    }
    let needed = per_row.values().copied().max().unwrap_or(0);
    min.max(max.min(needed))
}

/// `getDeepestNesting` (fold.c:1453-1470): the deepest active fold level.
fn deepest_nesting(state: &BufferState) -> i64 {
    state
        .folds
        .folds()
        .iter()
        .map(|fold| cell_count(fold.depth))
        .max()
        .unwrap_or(0)
}

fn text_context(
    editor: &Editor,
    win: WinHandle,
    text: &Buffer,
    marks: Vec<Extmark>,
) -> Result<WindowTextContext, ApiError> {
    let state = editor.window(win).map_err(exception)?;
    let buffer = state.buffer;
    let cursor_row = state.cursor.lnum;
    let line_count = text.line_count();
    let options = editor.options();
    let float_dims = editor
        .window_config(win)
        .map_err(exception)?
        .map(|config| (cell_count(config.width), cell_count(config.height)));
    let (view_width, view_height) = if let Some(dims) = float_dims {
        dims
    } else {
        let geometry = editor.window_geometry(win).map_err(exception)?;
        let height = editor.window_text_height(win).map_err(exception)?;
        (cell_count(geometry.width), cell_count(height))
    };
    let number = window_option_flag(options, win, "number");
    let relative = window_option_flag(options, win, "relativenumber");
    let statuscolumn = window_option_string(options, win, "statuscolumn");
    let numbers = number || relative || !statuscolumn.is_empty();
    let list = window_option_flag(options, win, "list");
    let listchars = window_option_string(options, win, "listchars");
    let signcolumn = window_option_string(options, win, "signcolumn");
    let has_sign_marks = marks.iter().any(|mark| {
        !mark.invalid
            && (mark.placement.attributes.sign_text.is_some()
                || mark.placement.attributes.sign_name.is_some())
    });
    let minsc_number = signcolumn.starts_with("number") && numbers;
    let number_width = if numbers {
        number_columns(
            line_count,
            view_height,
            number,
            relative,
            window_option_number(options, win, "numberwidth").max(1),
            minsc_number,
            has_sign_marks,
        )
    } else {
        0
    };
    let sign_width = sign_column_width(&marks, signcolumn_bounds(&signcolumn, numbers));
    let fold_width = foldcolumn_columns(
        &window_option_string(options, win, "foldcolumn"),
        deepest_nesting(editor.buffer(buffer).map_err(exception)?),
    );
    // win_col_off (move.c:812-820): numbers plus one cell for the "eol" list
    // char position, fold column, and sign columns.
    let col_off = if numbers {
        number_width + i64::from(statuscolumn.is_empty())
    } else {
        0
    } + fold_width
        + sign_width * SIGN_WIDTH;
    // win_col_off2 (move.c:822-831): numbers repeat on wrapped rows only
    // when 'cpoptions' contains "n" (kCpoNumcol).
    let col_off2 = if numbers && option_string(options, "cpoptions").contains('n') {
        number_width + i64::from(statuscolumn.is_empty())
    } else {
        0
    };
    Ok(WindowTextContext {
        line_count,
        view_width,
        col_off,
        col_off2,
        wrap: window_option_flag(options, win, "wrap"),
        list_eol: list && listchars.contains("eol:"),
        use_tabstop: !list || listchars.contains("tab:"),
        tabstop: buffer_option_number(options, buffer, "tabstop").max(1),
        showbreak_cells: string_cells(&option_string(options, "showbreak")),
        conceallevel: window_option_number(options, win, "conceallevel"),
        cursor_row,
        cursor_conceals: window_option_string(options, win, "concealcursor").contains('n'),
        foldenable: window_option_flag(options, win, "foldenable"),
        marks,
    })
}

/// `hasFolding` (fold.c:156-263): the outermost closed fold covering the
/// 1-based line, gated on 'foldenable' (`hasAnyFolding`, fold.c:147-152).
/// Returns the (first, last) 1-based inclusive fold range.
fn fold_covering(
    state: &BufferState,
    ctx: &WindowTextContext,
    lnum: usize,
) -> Option<(usize, usize)> {
    if !ctx.foldenable {
        return None;
    }
    let (first, last) = state.folds.closed_rows_at(lnum - 1)?;
    Some((first + 1, (last + 1).min(ctx.line_count)))
}

/// Whether an extmark decorates a zero-based row: it starts on the row or its
/// range spans past the row start (`marktree_itr_get_overlap` plus the
/// same-row sweep, decoration.c:898-909).
fn mark_covers_row(mark: &Extmark, row: usize) -> bool {
    if mark.position().row == row {
        return true;
    }
    let Some(end) = mark.placement.end else {
        return false;
    };
    end.position.row > row || (end.position.row == row && end.position.column > 0)
}

/// `decor_conceal_line` (decoration.c:878-912): whether a zero-based buffer
/// row is wholly concealed by an extmark with `conceal_lines`. Requires
/// 'conceallevel' >= 2; the cursor row stays visible unless 'concealcursor'
/// covers the current (Normal) mode.
fn conceal_line(ctx: &WindowTextContext, row: usize) -> bool {
    if ctx.conceallevel < 2 {
        return false;
    }
    if row + 1 == ctx.cursor_row && !ctx.cursor_conceals {
        return false;
    }
    ctx.marks.iter().any(|mark| {
        !mark.invalid
            && mark.placement.attributes.conceal_lines.is_some()
            && mark_covers_row(mark, row)
    })
}

/// Display width of inline virtual text (`DecorVirtText.width`), summed per
/// chunk character; tabs count one cell.
fn virtual_text_width(chunks: &[VirtualTextChunk]) -> i64 {
    chunks
        .iter()
        .map(|chunk| {
            chunk
                .text
                .chars()
                .map(|character| match character {
                    '\t' => 1,
                    _ => cell_count(UnicodeWidthChar::width(character).unwrap_or(1).max(1)),
                })
                .sum::<i64>()
        })
        .sum()
}

/// Inline virtual text anchored on a zero-based row as (byte column, width)
/// pairs sorted by column (`CharsizeArg.virt_row` plus
/// `inline_virt_text_width`, plines.c:89-158).
fn inline_marks(ctx: &WindowTextContext, row: usize) -> Vec<(usize, i64)> {
    let mut marks: Vec<(usize, i64)> = ctx
        .marks
        .iter()
        .filter(|mark| {
            !mark.invalid
                && mark.position().row == row
                && mark.placement.attributes.virtual_text_position
                    == ExtmarkVirtualTextPosition::Inline
                && !mark.placement.attributes.virtual_text.is_empty()
        })
        .map(|mark| {
            (
                mark.position().column,
                virtual_text_width(&mark.placement.attributes.virtual_text),
            )
        })
        .collect();
    marks.sort_unstable_by_key(|(column, _)| *column);
    marks
}

/// Decodes one UTF-8 character, mirroring `utf_ptr2StrCharInfo` plus
/// `utfc_next` (mbyte.c): invalid bytes yield `None` and advance one byte,
/// which `charsize_*` charges `kInvalidByteCells`.
fn decode_cell(line: &[u8], index: usize) -> (Option<char>, usize) {
    let lead = line[index];
    if lead < 0x80 {
        return (Some(char::from(lead)), 1);
    }
    let length = match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return (None, 1),
    };
    if index + length > line.len() {
        return (None, 1);
    }
    match std::str::from_utf8(&line[index..index + length]) {
        Ok(text) => match text.chars().next() {
            Some(character) => (Some(character), length),
            None => (None, 1),
        },
        Err(_) => (None, 1),
    }
}

/// Whether virtual column `vcol` sits in the rightmost column
/// (`in_win_border`, plines.c:463-485).
fn in_win_border(ctx: &WindowTextContext, vcol: i64) -> bool {
    if ctx.view_width == 0 {
        return false;
    }
    let width1 = ctx.view_width - ctx.col_off;
    if vcol < width1 - 1 {
        return false;
    }
    if vcol == width1 - 1 {
        return true;
    }
    let width2 = width1 + ctx.col_off2;
    if width2 <= 0 {
        return false;
    }
    (vcol - width1) % width2 == width2 - 1
}

/// Cells of one character (`charsize_fast_impl`, plines.c:401-431, matching
/// the `charsize_regular` base at plines.c:183-196): tabs pad to the tabstop,
/// control characters show as two cells, undecodable bytes as
/// `kInvalidByteCells`, and a double-width character at the wrap border adds
/// the ">" marker cell.
fn char_cells(ctx: &WindowTextContext, character: Option<char>, vcol: i64) -> i64 {
    match character {
        Some('\t') if ctx.use_tabstop => ctx.tabstop - vcol.rem_euclid(ctx.tabstop),
        None => INVALID_BYTE_CELLS,
        Some(control) if (control as u32) < 0x20 || control as u32 == 0x7f => 2,
        Some(character) => {
            let cells = UnicodeWidthChar::width(character).unwrap_or(1).max(1);
            if cells == 2 && (character as u32) >= 0x80 && ctx.wrap && in_win_border(ctx, vcol) {
                3
            } else {
                cell_count(cells)
            }
        }
    }
}

/// Total display width of a buffer line including anchored inline virtual
/// text (`linesize_fast`/`linesize_regular`, plines.c:495-560), capped at
/// `MAX_COLUMN`.
fn line_cells(ctx: &WindowTextContext, line: &[u8], inline: &[(usize, i64)]) -> i64 {
    let mut vcol = 0i64;
    let mut index = 0usize;
    let mut mark_index = 0usize;
    while index < line.len() {
        // Inline virtual text at this byte column (plines.c:198-231): a tab
        // re-pads from the position after the inserted text.
        while mark_index < inline.len() {
            let (column, width) = inline[mark_index];
            if column > index {
                break;
            }
            if column == index {
                vcol += width;
            }
            mark_index += 1;
        }
        let (character, length) = decode_cell(line, index);
        vcol += char_cells(ctx, character, vcol);
        if vcol > MAX_COLUMN {
            return MAX_COLUMN;
        }
        index += length;
    }
    // Inline virtual text at end-of-line (plines.c:512-517).
    while mark_index < inline.len() {
        let (column, width) = inline[mark_index];
        if column > line.len() {
            break;
        }
        if column == line.len() {
            vcol += width;
        }
        mark_index += 1;
    }
    vcol
}

/// `linetabsize_eol` (plines.c:83-87): line width plus the 'listchars' "eol"
/// cell in list mode.
fn linetabsize_eol(text: &Buffer, ctx: &WindowTextContext, lnum: usize) -> i64 {
    let line = text.line(lnum).unwrap_or_default();
    let width = line_cells(ctx, &line, &inline_marks(ctx, lnum - 1));
    width + i64::from(ctx.list_eol)
}

/// `plines_win_nofold` (plines.c:832-866): rows a line occupies ignoring
/// folds and filler lines.
fn plines_win_nofold(text: &Buffer, ctx: &WindowTextContext, lnum: usize) -> i64 {
    let line = text.line(lnum).unwrap_or_default();
    let inline = inline_marks(ctx, lnum - 1);
    // Empty line quick path (plines.c:837-839).
    if line.is_empty() && inline.is_empty() {
        return 1;
    }
    let mut col = line_cells(ctx, &line, &inline);
    if ctx.list_eol {
        col += 1;
    }
    // Add the gutter offset (plines.c:854-858).
    let width = ctx.view_width - ctx.col_off;
    if width <= 0 {
        return ZERO_TEXT_WIDTH_ROWS;
    }
    if col <= width {
        return 1;
    }
    let rest = col - width;
    let width = width + ctx.col_off2;
    // Each continuation row repeats the 'showbreak' cells
    // (charsize_regular, plines.c:284-327).
    let width = (width - ctx.showbreak_cells).max(1);
    ((rest + width - 1) / width + 1).min(MAX_COLUMN)
}

/// `plines_win_nofill` (plines.c:804-828): rows a buffer line occupies
/// excluding filler lines above.
fn plines_win_nofill(
    state: &BufferState,
    text: &Buffer,
    ctx: &WindowTextContext,
    lnum: usize,
) -> i64 {
    if conceal_line(ctx, lnum - 1) {
        return 0;
    }
    if !ctx.wrap || ctx.view_width == 0 {
        return 1;
    }
    // Folded lines count like an empty line (plines.c:818-821).
    if fold_covering(state, ctx, lnum).is_some() {
        return 1;
    }
    plines_win_nofold(text, ctx, lnum)
}

/// `decor_virt_line_wrap` (decoration.c:1134-1136).
fn virtual_lines_wrap(ctx: &WindowTextContext, overflow: ExtmarkVirtualLinesOverflow) -> bool {
    match overflow {
        ExtmarkVirtualLinesOverflow::Wrap => true,
        ExtmarkVirtualLinesOverflow::Auto => ctx.wrap,
        ExtmarkVirtualLinesOverflow::Trunc | ExtmarkVirtualLinesOverflow::Scroll => false,
    }
}

/// `decor_virt_line_rows` (decoration.c:1141-1188): rows one virtual line
/// occupies, wrapping its chunk text at the window edge.
fn virt_line_rows(ctx: &WindowTextContext, line: &[VirtualTextChunk], leftcol: bool) -> i64 {
    // `kVLLeftcol` draws in the left column, skipping the gutter
    // (decoration.c:1152).
    let row_width = ctx.view_width - if leftcol { 0 } else { ctx.col_off };
    if row_width <= 0 {
        return 1;
    }
    let mut rows = 1i64;
    let mut row_cells = 0i64;
    let mut vcol = 0i64;
    for chunk in line {
        for character in chunk.text.chars() {
            let cells = match character {
                '\t' => (ctx.tabstop - vcol.rem_euclid(ctx.tabstop)).max(1),
                control if (control as u32) < 0x20 || control as u32 == 0x7f => 2,
                character => cell_count(UnicodeWidthChar::width(character).unwrap_or(1).max(1)),
            };
            if row_cells + cells > row_width {
                rows += 1;
                row_cells = 0;
            }
            row_cells += cells;
            vcol += cells;
        }
    }
    rows
}

/// `win_get_fill` (plines.c:785-788) without diff filler: the port keeps
/// diff windows equal width, so `diff_check_fill` is always zero. Counts the
/// virtual lines drawn just above buffer line `lnum`
/// (`decor_virt_lines`, decoration.c:1192-1256, `apply_folds`), including the
/// filler below the last buffer line for `lnum == line_count + 1`.
fn win_get_fill(state: &BufferState, ctx: &WindowTextContext, lnum: usize) -> i64 {
    // Marks starting on rows [lnum - 2, lnum - 1] 0-based
    // (decoration.c:1203: MAX(start_row - 1, 0)).
    let first_row = lnum.saturating_sub(2);
    let last_row = lnum - 1;
    let mut count = 0;
    for mark in &ctx.marks {
        if mark.invalid {
            continue;
        }
        let attributes = &mark.placement.attributes;
        if attributes.virtual_lines.is_empty() {
            continue;
        }
        let row = mark.position().row;
        if !(first_row..=last_row).contains(&row) {
            continue;
        }
        let draw_row = row + usize::from(!attributes.virt_lines_above);
        if draw_row != lnum - 1 {
            continue;
        }
        // apply_folds skips virtual lines inside folds or on concealed rows
        // (decoration.c:1222-1223).
        if fold_covering(state, ctx, row + 1).is_some() || conceal_line(ctx, row) {
            continue;
        }
        if virtual_lines_wrap(ctx, attributes.virt_lines_overflow) {
            for line in &attributes.virtual_lines {
                count += virt_line_rows(ctx, line, attributes.virt_lines_leftcol);
            }
        } else {
            count += cell_count(attributes.virtual_lines.len());
        }
    }
    count
}

/// `win_text_height` (plines.c:1016-1083): screen rows occupied by the
/// 1-based inclusive `start_lnum..=end_lnum` range. `end_lnum`/`end_vcol`
/// are in/out exactly like upstream.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the upstream win_text_height in/out parameter set"
)]
fn text_height(
    state: &BufferState,
    text: &Buffer,
    ctx: &WindowTextContext,
    start_lnum: usize,
    start_vcol: i64,
    end_lnum: &mut usize,
    end_vcol: &mut i64,
    fill: &mut i64,
    max: i64,
) -> i64 {
    let raw_width1 = ctx.view_width - ctx.col_off;
    let raw_width2 = raw_width1 + ctx.col_off2;
    let width1 = raw_width1.max(0);
    let width2 = raw_width2.max(0);
    let mut height_sum_fill = 0;
    let mut height_cur_nofill = 0;
    let mut height_sum_nofill = 0;
    let mut lnum = start_lnum;
    let mut cur_lnum = start_lnum;
    let mut cur_folded = false;

    if start_vcol >= 0 {
        let mut lnum_next = lnum;
        if let Some((first, last)) = fold_covering(state, ctx, lnum) {
            cur_folded = true;
            lnum = first;
            lnum_next = last;
        }
        height_cur_nofill = plines_win_nofill(state, text, ctx, lnum);
        height_sum_nofill += height_cur_nofill;
        let row_off = if start_vcol < width1 || width2 <= 0 {
            0
        } else {
            1 + (start_vcol - width1) / width2
        };
        height_sum_nofill -= row_off.min(height_cur_nofill);
        lnum = lnum_next + 1;
    }

    while lnum <= *end_lnum && height_sum_nofill + height_sum_fill < max {
        let mut lnum_next = lnum;
        if let Some((first, last)) = fold_covering(state, ctx, lnum) {
            cur_folded = true;
            lnum = first;
            lnum_next = last;
        } else {
            cur_folded = false;
        }
        height_sum_fill += win_get_fill(state, ctx, lnum);
        height_cur_nofill = plines_win_nofill(state, text, ctx, lnum);
        height_sum_nofill += height_cur_nofill;
        cur_lnum = lnum;
        lnum = lnum_next + 1;
    }

    let mut vcol_end = *end_vcol;
    let use_vcol = vcol_end >= 0 && lnum > *end_lnum;
    if use_vcol {
        height_sum_nofill -= height_cur_nofill;
        let row_off = if vcol_end == 0 {
            0
        } else if vcol_end <= width1 || width2 <= 0 {
            1
        } else {
            1 + (vcol_end - width1 + width2 - 1) / width2
        };
        height_sum_nofill += row_off.min(height_cur_nofill);
    }

    if cur_folded {
        vcol_end = 0;
    } else {
        let cap = if use_vcol { vcol_end } else { i64::MAX };
        vcol_end = cap.min(linetabsize_eol(text, ctx, cur_lnum));
    }

    let overflow = height_sum_nofill + height_sum_fill - max;
    if overflow > 0 && width2 > 0 && vcol_end > width2 {
        vcol_end -= (vcol_end - width1) % width2 + (overflow - 1) * width2;
    }

    *end_lnum = cur_lnum;
    *end_vcol = vcol_end;
    *fill = height_sum_fill;
    height_sum_fill + height_sum_nofill
}

/// `normalize_index` (helpers.c:450-468): a 0-based index, negatives counting
/// from the bottom, clamped into range with the out-of-bounds flag set when
/// clamping happened.
fn normalize_index(index: i64, line_count: usize) -> (usize, bool) {
    let max_index = cell_count(line_count) - 1;
    let mut index = if index < 0 {
        max_index.saturating_add(index).saturating_add(1)
    } else {
        index
    };
    let mut oob = false;
    if index > max_index {
        oob = true;
        index = max_index;
    } else if index < 0 {
        oob = true;
        index = 0;
    }
    (usize::try_from(index).unwrap_or(0), oob)
}

/// Number of screen lines a range of text takes in a window
/// (`nvim_win_text_height`, api/window.c:455-541).
#[api(since = 12, method)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "RPC dispatch owns decoded dict arguments"
)]
pub fn nvim_win_text_height(
    session: &ApiSession,
    win: WinHandle,
    opts: Dict,
) -> Result<Dict, ApiError> {
    let opts = TextHeightOpts::parse(&opts)?;
    let win = resolve_window(session, win)?;
    session.with_editor(|editor| {
        let window = editor.window(win).map_err(exception)?;
        let buffer = window.buffer;
        let state = editor.buffer(buffer).map_err(exception)?;
        let text = state.text().map_err(exception)?;
        let line_count = text.line_count();
        let marks = state.extmarks.query_all(
            ExtmarkPosition::new(0, 0),
            ExtmarkPosition::new(usize::MAX, usize::MAX),
            None,
        );
        let ctx = text_context(editor, win, text, marks)?;

        let mut start_lnum = 1;
        let mut end_lnum = line_count;
        let mut oob = false;
        if let Some(index) = opts.start_row {
            let (row, out) = normalize_index(index, line_count);
            start_lnum = row + 1;
            oob |= out;
        }
        if let Some(index) = opts.end_row {
            let (row, out) = normalize_index(index, line_count);
            end_lnum = row + 1;
            oob |= out;
        }
        // VALIDATE, api/window.c:483-488.
        if oob {
            return Err(ApiError::validation("Line index out of bounds"));
        }
        // VALIDATE, api/window.c:486-488.
        if start_lnum > end_lnum {
            return Err(ApiError::validation("'start_row' is higher than 'end_row'"));
        }
        let mut start_vcol = -1;
        let mut end_vcol = -1;
        if let Some(value) = opts.start_vcol {
            // VALIDATE, api/window.c:491-494.
            if opts.start_row.is_none() {
                return Err(ApiError::validation(
                    "'start_vcol' specified without 'start_row'",
                ));
            }
            start_vcol = value;
            // VALIDATE_RANGE, api/window.c:496-498.
            if !(0..=MAX_COLUMN).contains(&start_vcol) {
                return Err(ApiError::validation("Invalid 'start_vcol': out of range"));
            }
        }
        if let Some(value) = opts.end_vcol {
            // VALIDATE, api/window.c:502-505.
            if opts.end_row.is_none() {
                return Err(ApiError::validation(
                    "'end_vcol' specified without 'end_row'",
                ));
            }
            end_vcol = value;
            // VALIDATE_RANGE, api/window.c:507-509.
            if !(0..=MAX_COLUMN).contains(&end_vcol) {
                return Err(ApiError::validation("Invalid 'end_vcol': out of range"));
            }
        }
        let mut max = i64::MAX;
        if let Some(value) = opts.max_height {
            // VALIDATE_RANGE, api/window.c:514-516.
            if value <= 0 {
                return Err(ApiError::validation("Invalid 'max_height': out of range"));
            }
            max = value;
        }
        // VALIDATE, api/window.c:520-524.
        if start_lnum == end_lnum && start_vcol >= 0 && end_vcol >= 0 && start_vcol > end_vcol {
            return Err(ApiError::validation(
                "'start_vcol' is higher than 'end_vcol'",
            ));
        }

        let mut fill = 0;
        let mut end_row = end_lnum;
        let mut end_column = end_vcol;
        let mut all = text_height(
            state,
            text,
            &ctx,
            start_lnum,
            start_vcol,
            &mut end_row,
            &mut end_column,
            &mut fill,
            max,
        );
        // Filler below the last buffer line counts only when "end_row" is
        // omitted (api/window.c:528-532).
        if opts.end_row.is_none() {
            let end_fill = win_get_fill(state, &ctx, line_count + 1);
            fill += end_fill;
            all += end_fill;
        }
        Ok(Dict(vec![
            (OxStr::from("all"), Object::Integer(all)),
            (OxStr::from("fill"), Object::Integer(fill)),
            (
                OxStr::from("end_row"),
                Object::Integer(api_integer(end_row - 1, "end_row")?),
            ),
            (OxStr::from("end_vcol"), Object::Integer(end_column)),
        ]))
    })
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_win_get_buf__API_META(), nvim_win_get_buf__API_DISPATCH)?;
    registry.register(nvim_win_set_buf__API_META(), nvim_win_set_buf__API_DISPATCH)?;
    registry.register(
        nvim_win_get_cursor__API_META(),
        nvim_win_get_cursor__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_set_cursor__API_META(),
        nvim_win_set_cursor__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_get_height__API_META(),
        nvim_win_get_height__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_set_height__API_META(),
        nvim_win_set_height__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_get_width__API_META(),
        nvim_win_get_width__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_set_width__API_META(),
        nvim_win_set_width__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_get_position__API_META(),
        nvim_win_get_position__API_DISPATCH,
    )?;
    registry.register(nvim_win_get_var__API_META(), nvim_win_get_var__API_DISPATCH)?;
    registry.register(nvim_win_set_var__API_META(), nvim_win_set_var__API_DISPATCH)?;
    registry.register(nvim_win_del_var__API_META(), nvim_win_del_var__API_DISPATCH)?;
    registry.register(
        nvim_win_get_option__API_META(),
        nvim_win_get_option__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_set_option__API_META(),
        nvim_win_set_option__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_get_tabpage__API_META(),
        nvim_win_get_tabpage__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_get_number__API_META(),
        nvim_win_get_number__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_is_valid__API_META(),
        nvim_win_is_valid__API_DISPATCH,
    )?;
    registry.register(nvim_win_hide__API_META(), nvim_win_hide__API_DISPATCH)?;
    registry.register(nvim_win_close__API_META(), nvim_win_close__API_DISPATCH)?;
    registry.register(nvim_win_call__API_META(), nvim_win_call__API_DISPATCH)?;
    registry.register(
        nvim_win_set_hl_ns__API_META(),
        nvim_win_set_hl_ns__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_get_config__API_META(),
        nvim_win_get_config__API_DISPATCH,
    )?;
    registry.register(
        nvim_win_text_height__API_META(),
        nvim_win_text_height__API_DISPATCH,
    )?;
    registry.register(nvim_win_resize__API_META(), nvim_win_resize__API_DISPATCH)?;
    registry.register(nvim_open_win__API_META(), nvim_open_win__API_DISPATCH)?;
    registry.register(
        nvim_win_set_config__API_META(),
        nvim_win_set_config__API_DISPATCH,
    )?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "focused unit tests assert exact upstream messages; unwraps and panics are the local test idiom, mirroring crates/ox-api/src/tests.rs"
)]
mod tests {

    use std::cell::RefCell;
    use std::rc::Rc;

    use ox_editor::fold;
    use ox_text::Buffer;

    use super::*;
    use crate::ApiSession;

    fn session_with(
        lines: &[&str],
        width: usize,
        height: usize,
    ) -> (ApiSession, BufHandle, WinHandle) {
        let mut editor = Editor::new();
        let content = lines
            .iter()
            .map(|line| line.as_bytes().to_vec())
            .collect::<Vec<_>>();
        let buffer = editor
            .create_buffer_with(Buffer::from_lines(&content, false).unwrap(), true)
            .unwrap();
        let tab = editor
            .create_tabpage(
                buffer,
                ox_editor::Geometry::new(0, 0, width, height).unwrap(),
            )
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        (
            ApiSession::new(Rc::new(RefCell::new(editor))),
            buffer,
            window,
        )
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        Dict(
            entries
                .iter()
                .map(|(key, value)| (OxStr::from(*key), value.clone()))
                .collect(),
        )
    }

    fn integer(entry: &Dict, key: &str) -> i64 {
        match entry
            .iter()
            .find(|(name, _)| name.as_bytes() == key.as_bytes())
            .map(|(_, value)| value)
        {
            Some(Object::Integer(value)) => *value,
            other => panic!("'{key}' is not an integer: {other:?}"),
        }
    }

    fn set_extmark(session: &ApiSession, buffer: BufHandle, line: i64, opts: Dict) {
        // `nvim_buf_set_extmark` rejects `ns_id` 0 like upstream; the
        // default namespace is whatever `nvim_create_namespace("")` holds.
        let ns = crate::extmark::nvim_create_namespace(session, OxStr::from(&b""[..])).unwrap();
        crate::extmark::nvim_buf_set_extmark(session, buffer, ns, line, 0, opts).unwrap();
    }

    fn set_window_number(session: &ApiSession, win: WinHandle, name: &str, value: i64) {
        session
            .with_editor_mut(|editor| {
                editor
                    .options_mut()
                    .set_window(win, name, OptionValue::Number(value))
            })
            .unwrap();
    }

    // -- nvim_win_resize ----------------------------------------------------

    #[test]
    fn resize_requires_a_dimension() {
        let (session, _buffer, window) = session_with(&["a"], 80, 24);
        let error = nvim_win_resize(&session, window, -1, -1, dict(&[])).unwrap_err();
        assert_eq!(error.to_string(), "Required: 'height' or 'width'");
    }

    #[test]
    fn resize_rejects_negative_dimensions() {
        let (session, _buffer, window) = session_with(&["a"], 80, 24);
        let error = nvim_win_resize(&session, window, -1, -2, dict(&[])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid 'height': expected non-negative number or -1"
        );
        let error = nvim_win_resize(&session, window, -2, -1, dict(&[])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid 'width': expected non-negative number or -1"
        );
    }

    #[test]
    fn resize_validates_anchor() {
        let (session, _buffer, window) = session_with(&["a"], 80, 24);
        let error = nvim_win_resize(
            &session,
            window,
            10,
            -1,
            dict(&[("anchor", Object::String(OxStr::from("middle")))]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid 'anchor': expected \"top\", \"bottom\", \"left\" or \"right\", got middle"
        );
        let error = nvim_win_resize(
            &session,
            window,
            -1,
            10,
            dict(&[("anchor", Object::String(OxStr::from("left")))]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Conflict: 'left' not allowed with 'width'"
        );
        let error = nvim_win_resize(
            &session,
            window,
            10,
            -1,
            dict(&[("anchor", Object::String(OxStr::from("top")))]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Conflict: 'top' not allowed with 'height'"
        );
        let error = nvim_win_resize(
            &session,
            window,
            10,
            -1,
            dict(&[("anchor", Object::Integer(1))]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid 'anchor': expected String, got Integer"
        );
        let error =
            nvim_win_resize(&session, window, 10, -1, dict(&[("bogus", Object::Nil)])).unwrap_err();
        assert_eq!(error.to_string(), "Invalid key: 'bogus'");
    }

    #[test]
    fn resize_resizes_a_floating_window() {
        let (session, buffer, _window) = session_with(&["a"], 80, 24);
        let float = nvim_open_win(
            &session,
            buffer,
            true,
            dict(&[
                ("relative", Object::String(OxStr::from("editor"))),
                ("row", Object::Float(2.0)),
                ("col", Object::Float(4.0)),
                ("width", Object::Integer(20)),
                ("height", Object::Integer(5)),
            ]),
        )
        .unwrap();
        nvim_win_resize(&session, float, 30, 7, dict(&[])).unwrap();
        assert_eq!(nvim_win_get_width(&session, float).unwrap(), 30);
        assert_eq!(nvim_win_get_height(&session, float).unwrap(), 7);
    }

    #[test]
    fn resize_resizes_a_tiled_split() {
        let (session, buffer, window) = session_with(&["a"], 80, 24);
        let other = session.with_editor_mut(|editor| {
            let tab = editor.window_tabpage(window).unwrap();
            editor.split_vertical(tab, window, buffer, false).unwrap()
        });
        nvim_win_resize(&session, window, 30, -1, dict(&[])).unwrap();
        assert_eq!(nvim_win_get_width(&session, window).unwrap(), 30);
        assert_eq!(nvim_win_get_width(&session, other).unwrap(), 50);
    }

    #[test]
    fn resize_clamps_height_to_winminheight() {
        // The clamp is a split negotiation: a lone window fills the frame
        // and cannot shrink (nothing can take the space), so the clamp
        // needs a sibling (win_setheight_win -> frame_setheight,
        // window.c:6238-6254).
        let (session, buffer, window) = session_with(&["a"], 80, 24);
        session
            .with_editor_mut(|editor| {
                let tab = editor.window_tabpage(window).unwrap();
                editor.split_horizontal(tab, window, buffer, false).unwrap();
                editor
                    .options_mut()
                    .set_global("winminheight", OptionValue::Number(5))
            })
            .unwrap();
        nvim_win_resize(&session, window, -1, 2, dict(&[])).unwrap();
        assert_eq!(nvim_win_get_height(&session, window).unwrap(), 5);
    }

    // -- nvim_win_text_height ------------------------------------------------

    #[test]
    fn text_height_counts_plain_lines() {
        let (session, _buffer, window) = session_with(&["abc", "de", ""], 80, 24);
        let result = nvim_win_text_height(&session, window, dict(&[])).unwrap();
        assert_eq!(integer(&result, "all"), 3);
        assert_eq!(integer(&result, "fill"), 0);
        assert_eq!(integer(&result, "end_row"), 2);
        // linetabsize_eol of the last (empty) line: no 'list' "eol" cell.
        assert_eq!(integer(&result, "end_vcol"), 0);
    }

    #[test]
    fn text_height_counts_wrapped_lines() {
        let (session, _buffer, window) = session_with(&["aaaaaaaaaaaa"], 5, 24);
        let result = nvim_win_text_height(&session, window, dict(&[])).unwrap();
        assert_eq!(integer(&result, "all"), 3);
        assert_eq!(integer(&result, "end_row"), 0);
        assert_eq!(integer(&result, "end_vcol"), 12);
    }

    #[test]
    fn text_height_honors_row_range_and_negative_index() {
        let (session, _buffer, window) = session_with(&["a", "b", "c", "d"], 80, 24);
        let result = nvim_win_text_height(
            &session,
            window,
            dict(&[
                ("start_row", Object::Integer(1)),
                ("end_row", Object::Integer(2)),
            ]),
        )
        .unwrap();
        assert_eq!(integer(&result, "all"), 2);
        assert_eq!(integer(&result, "end_row"), 2);
        let result = nvim_win_text_height(
            &session,
            window,
            dict(&[("start_row", Object::Integer(-2))]),
        )
        .unwrap();
        assert_eq!(integer(&result, "all"), 2);
        assert_eq!(integer(&result, "end_row"), 3);
    }

    #[test]
    fn text_height_validates_arguments() {
        let (session, _buffer, window) = session_with(&["a", "b"], 80, 24);
        let error =
            nvim_win_text_height(&session, window, dict(&[("start_row", Object::Integer(9))]))
                .unwrap_err();
        assert_eq!(error.to_string(), "Line index out of bounds");
        let error = nvim_win_text_height(
            &session,
            window,
            dict(&[
                ("start_row", Object::Integer(1)),
                ("end_row", Object::Integer(0)),
            ]),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "'start_row' is higher than 'end_row'");
        let error = nvim_win_text_height(
            &session,
            window,
            dict(&[("start_vcol", Object::Integer(0))]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "'start_vcol' specified without 'start_row'"
        );
        let error =
            nvim_win_text_height(&session, window, dict(&[("end_vcol", Object::Integer(0))]))
                .unwrap_err();
        assert_eq!(error.to_string(), "'end_vcol' specified without 'end_row'");
        let error = nvim_win_text_height(
            &session,
            window,
            dict(&[
                ("start_row", Object::Integer(0)),
                ("start_vcol", Object::Integer(-1)),
            ]),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Invalid 'start_vcol': out of range");
        let error = nvim_win_text_height(
            &session,
            window,
            dict(&[("max_height", Object::Integer(0))]),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Invalid 'max_height': out of range");
        let error = nvim_win_text_height(
            &session,
            window,
            dict(&[
                ("start_row", Object::Integer(0)),
                ("end_row", Object::Integer(0)),
                ("start_vcol", Object::Integer(3)),
                ("end_vcol", Object::Integer(1)),
            ]),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "'start_vcol' is higher than 'end_vcol'");
        let error = nvim_win_text_height(&session, window, dict(&[("bogus", Object::Integer(1))]))
            .unwrap_err();
        assert_eq!(error.to_string(), "Invalid key: 'bogus'");
        let error = nvim_win_text_height(
            &session,
            window,
            dict(&[("end_row", Object::String(OxStr::from("x")))]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid 'end_row': expected Integer, got String"
        );
    }

    #[test]
    fn text_height_limits_with_max_height() {
        let (session, _buffer, window) = session_with(&["a", "b", "c"], 80, 24);
        let result = nvim_win_text_height(
            &session,
            window,
            dict(&[("max_height", Object::Integer(2))]),
        )
        .unwrap();
        assert_eq!(integer(&result, "all"), 2);
        assert_eq!(integer(&result, "end_row"), 1);
        assert_eq!(integer(&result, "end_vcol"), 1);
    }

    #[test]
    fn text_height_counts_closed_folds() {
        let (session, buffer, window) = session_with(&["a", "b", "c", "d", "e"], 80, 24);
        session
            .with_editor_mut(|editor| {
                editor
                    .options_mut()
                    .set_window(window, "foldenable", OptionValue::Boolean(true))
            })
            .unwrap();
        session
            .with_editor_mut(|editor| {
                editor
                    .buffer_mut(buffer)
                    .unwrap()
                    .folds
                    // `FoldRange` is half-open: end position row 3 folds
                    // rows 0-2 (buffer lines 1-3).
                    .create_manual(fold::Position::new(0, 0), fold::Position::new(3, 0))
            })
            .unwrap();
        let result = nvim_win_text_height(&session, window, dict(&[])).unwrap();
        // One row for the closed fold plus rows for lines 4-5.
        assert_eq!(integer(&result, "all"), 3);
        assert_eq!(integer(&result, "end_row"), 4);
        // The height is reached on the last (unfolded) line, so end_vcol is
        // its display width (api/window.c:451-452).
        assert_eq!(integer(&result, "end_vcol"), 1);
    }

    #[test]
    fn text_height_counts_virtual_lines() {
        let (session, buffer, window) = session_with(&["a", "b"], 80, 24);
        set_extmark(
            &session,
            buffer,
            0,
            dict(&[(
                "virt_lines",
                Object::Array(vec![Object::Array(vec![Object::Array(vec![
                    Object::String(OxStr::from("x")),
                ])])]),
            )]),
        );
        let result = nvim_win_text_height(&session, window, dict(&[])).unwrap();
        assert_eq!(integer(&result, "all"), 3);
        assert_eq!(integer(&result, "fill"), 1);
        // With "end_row" set, filler below the last buffer line is excluded.
        let result =
            nvim_win_text_height(&session, window, dict(&[("end_row", Object::Integer(1))]))
                .unwrap();
        assert_eq!(integer(&result, "all"), 3);
        assert_eq!(integer(&result, "fill"), 1);
    }

    #[test]
    fn text_height_skips_concealed_lines() {
        let (session, buffer, window) = session_with(&["a", "b", "c"], 80, 24);
        set_window_number(&session, window, "conceallevel", 2);
        set_extmark(
            &session,
            buffer,
            1,
            dict(&[("conceal_lines", Object::String(OxStr::from("X")))]),
        );
        let result = nvim_win_text_height(&session, window, dict(&[])).unwrap();
        assert_eq!(integer(&result, "all"), 2);
        assert_eq!(integer(&result, "end_row"), 2);
    }

    #[test]
    fn text_height_measures_vcol_range() {
        let (session, _buffer, window) = session_with(&["abcdefghij"], 5, 24);
        let result = nvim_win_text_height(
            &session,
            window,
            dict(&[
                ("start_row", Object::Integer(0)),
                ("start_vcol", Object::Integer(5)),
                ("end_row", Object::Integer(0)),
                ("end_vcol", Object::Integer(10)),
            ]),
        )
        .unwrap();
        // Columns 5..10 land on the second screen line of the wrapped row
        // (plines.c:1031-1063).
        assert_eq!(integer(&result, "all"), 1);
        assert_eq!(integer(&result, "end_vcol"), 10);
    }
}
