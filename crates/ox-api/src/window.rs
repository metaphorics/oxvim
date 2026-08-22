use ox_editor::{
    Anchor, Border, BorderText, BufferRelease, Editor, Margins, OptionValue, RelativeTo,
    TextAlignment, WinConfig,
};
use ox_text::Position;

use crate::{
    ApiError, BufHandle, Dict, LuaRef, Object, OxStr, Registry, RegistryError, TabHandle,
    WinHandle, api,
};

fn exception(error: impl std::fmt::Display) -> ApiError {
    ApiError::exception(error.to_string())
}

fn invalid(field: &str, message: impl std::fmt::Display) -> ApiError {
    ApiError::validation(format!("Invalid 'config.{field}': {message}"))
}

fn key<'a>(dict: &'a Dict, name: &str) -> Option<&'a Object> {
    dict.iter()
        .find(|(candidate, _)| candidate.as_bytes() == name.as_bytes())
        .map(|(_, value)| value)
}

fn resolve_window(editor: &Editor, window: WinHandle) -> Result<WinHandle, ApiError> {
    let resolved = if window.is_current() {
        editor
            .current_window()
            .ok_or_else(|| ApiError::exception("No current window"))?
    } else {
        window
    };
    editor.window(resolved).map_err(exception)?;
    Ok(resolved)
}

fn resolve_buffer(editor: &Editor, buffer: BufHandle) -> Result<BufHandle, ApiError> {
    if buffer.is_current() {
        return editor
            .current_buffer()
            .ok_or_else(|| ApiError::exception("No current buffer"));
    }
    editor.buffer(buffer).map_err(exception)?;
    Ok(buffer)
}

fn window_tabpage(editor: &Editor, window: WinHandle) -> Result<TabHandle, ApiError> {
    editor.window_tabpage(window).map_err(exception)
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
        Some(Object::Nil) if required => Err(invalid(name, "field is required")),
        Some(Object::Nil) => Ok(None),
        Some(_) => Err(invalid(name, "expected Integer")),
        None if required => Err(invalid(name, "field is required")),
        None => Ok(None),
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
    let value = match key(dict, name) {
        Some(Object::Float(value)) => Some(*value),
        Some(Object::Integer(value)) => Some(*value as f64),
        Some(Object::Nil) if required => return Err(invalid(name, "field is required")),
        Some(Object::Nil) => None,
        Some(_) => return Err(invalid(name, "expected Float or Integer")),
        None if required => return Err(invalid(name, "field is required")),
        None => None,
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

fn parse_anchor(value: Option<String>, default: Anchor) -> Result<Anchor, ApiError> {
    match value.as_deref() {
        None => Ok(default),
        Some("NW") => Ok(Anchor::NorthWest),
        Some("NE") => Ok(Anchor::NorthEast),
        Some("SW") => Ok(Anchor::SouthWest),
        Some("SE") => Ok(Anchor::SouthEast),
        Some(value) => Err(invalid("anchor", format!("invalid value: {value}"))),
    }
}

fn parse_relative(
    editor: &Editor,
    dict: &Dict,
    default: Option<RelativeTo>,
) -> Result<RelativeTo, ApiError> {
    let relative = string(dict, "relative")?;
    let effective = relative.as_deref().or_else(|| match default {
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
                    _ => resolve_window(editor, WinHandle::CURRENT)?,
                },
                Some(Object::Window(window)) => resolve_window(editor, *window)?,
                Some(Object::Integer(window)) => WinHandle::try_from(*window)
                    .map_err(|error| invalid("win", error))
                    .and_then(|window| resolve_window(editor, window))?,
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
            let (Object::String(character), Object::String(_highlight)) = (&items[0], &items[1]) else {
                return Err(invalid("border", "tuple items must be [character, highlight] strings"));
            };
            String::from_utf8(character.0.clone())
                .map_err(|_| invalid("border", "characters must be valid UTF-8"))
        }
        _ => Err(invalid("border", "array items must be strings or highlight tuples")),
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

fn parse_alignment(dict: &Dict, name: &str, default: TextAlignment) -> Result<TextAlignment, ApiError> {
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
        Object::String(value) => String::from_utf8(value.0.clone())
            .map_err(|_| invalid(name, "must be valid UTF-8"))?,
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
        return Err(invalid(
            "margins",
            "expected [top, right, bottom, left]",
        ));
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
        b"relative", b"win", b"anchor", b"row", b"col", b"width", b"height",
        b"zindex", b"border", b"title", b"title_pos", b"footer", b"footer_pos",
        b"margins", b"style", b"split", b"focusable", b"external", b"bufpos",
        b"hide", b"noautocmd",
    ];
    for (name, _) in dict.iter() {
        if !SUPPORTED.iter().any(|supported| *supported == name.as_bytes()) {
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
        None | Some("") | Some("minimal") => {}
        Some(value) => return Err(invalid("style", format!("invalid value: {value}"))),
    }
    boolean(dict, "focusable")?;
    boolean(dict, "hide")?;
    boolean(dict, "noautocmd")?;
    Ok(())
}

/// `external` needs the UI layer to display a top-level window; there is no
/// such layer in this editor, so report a typed NotImplemented error.
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
        return Err(invalid("bufpos", "expected [line, column] array of length 2"));
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
    editor: &Editor,
    dict: &Dict,
    current: Option<&WinConfig>,
) -> Result<WinConfig, ApiError> {
    reject_unsupported_keys(dict)?;
    reject_external(dict)?;
    validate_float_flags(dict)?;
    let relative = parse_relative(editor, dict, current.map(|config| config.relative))?;
    let anchor = parse_anchor(
        string(dict, "anchor")?,
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

fn config_to_dict(config: Option<&WinConfig>) -> Dict {
    let Some(config) = config else {
        return Dict(vec![(OxStr::from("relative"), Object::String(OxStr::from("")))]);
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
    let border = match &config.border {
        Border::None => Object::String(OxStr::from("none")),
        Border::Single => Object::String(OxStr::from("single")),
        Border::Double => Object::String(OxStr::from("double")),
        Border::Rounded => Object::String(OxStr::from("rounded")),
        Border::Solid => Object::String(OxStr::from("solid")),
        Border::Shadow => Object::String(OxStr::from("shadow")),
        Border::Custom(parts) => Object::Array(
            parts
                .iter()
                .map(|part| Object::String(OxStr::from(part.as_str())))
                .collect(),
        ),
    };
    let mut result = Dict(vec![
        (OxStr::from("relative"), Object::String(OxStr::from(relative))),
        (OxStr::from("anchor"), Object::String(OxStr::from(anchor))),
        (OxStr::from("row"), Object::Float(config.row)),
        (OxStr::from("col"), Object::Float(config.col)),
        (OxStr::from("width"), Object::Integer(config.width as i64)),
        (OxStr::from("height"), Object::Integer(config.height as i64)),
        (OxStr::from("zindex"), Object::Integer(i64::from(config.zindex))),
        (OxStr::from("border"), border),
        (
            OxStr::from("margins"),
            Object::Array(vec![
                Object::Integer(config.margins.top as i64),
                Object::Integer(config.margins.right as i64),
                Object::Integer(config.margins.bottom as i64),
                Object::Integer(config.margins.left as i64),
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
    result
}

fn set_dimension(
    editor: &mut Editor,
    window: WinHandle,
    width: Option<usize>,
    height: Option<usize>,
) -> Result<(), ApiError> {
    let window = resolve_window(editor, window)?;
    if let Some(width) = width {
        editor.set_window_width(window, width).map_err(exception)?;
    }
    if let Some(height) = height {
        editor.set_window_height(window, height).map_err(exception)?;
    }
    Ok(())
}

#[api(since = 1, method)]
pub fn nvim_win_get_buf(editor: &mut Editor, win: WinHandle) -> Result<BufHandle, ApiError> {
    let win = resolve_window(editor, win)?;
    Ok(editor.window(win).map_err(exception)?.buffer)
}

#[api(since = 5, textlock, method)]
pub fn nvim_win_set_buf(
    editor: &mut Editor,
    win: WinHandle,
    buf: BufHandle,
) -> Result<(), ApiError> {
    let win = resolve_window(editor, win)?;
    let buf = resolve_buffer(editor, buf)?;
    editor
        .set_window_buffer(win, buf, BufferRelease::KeepLoaded)
        .map_err(exception)
}

#[api(since = 1, method)]
pub fn nvim_win_get_cursor(editor: &mut Editor, win: WinHandle) -> Result<Vec<i64>, ApiError> {
    let win = resolve_window(editor, win)?;
    let cursor = editor.window(win).map_err(exception)?.cursor;
    Ok(vec![cursor.lnum as i64, cursor.col as i64])
}

#[api(since = 1, method)]
pub fn nvim_win_set_cursor(
    editor: &mut Editor,
    win: WinHandle,
    pos: Vec<i64>,
) -> Result<(), ApiError> {
    if pos.len() != 2 {
        return Err(ApiError::validation("Cursor position must have exactly two items"));
    }
    let win = resolve_window(editor, win)?;
    let buffer = editor.window(win).map_err(exception)?.buffer;
    let text = editor.buffer(buffer).map_err(exception)?.text().map_err(exception)?;
    let row = usize::try_from(pos[0])
        .ok()
        .filter(|row| (1..=text.line_count()).contains(row))
        .ok_or_else(|| ApiError::validation("Cursor row outside buffer"))?;
    let line = text.line(row).map_err(exception)?;
    // Source: src/nvim/api/window.c:122-130 and src/nvim/pos_defs.h:17-19.
    // `MAXCOL` is the largest valid cursor column; values above it (or
    // negative) are rejected upstream before the column is silently clamped
    // to the line length (check_cursor_col).
    const MAXCOL: i64 = 0x7fff_ffff;
    if pos[1] < 0 || pos[1] > MAXCOL {
        return Err(ApiError::validation("Invalid cursor column: out of range"));
    }
    let col = (pos[1] as usize).min(line.len());
    editor
        .set_window_cursor(win, Position { lnum: row, col })
        .map_err(exception)
}

#[api(since = 1, method)]
pub fn nvim_win_get_height(editor: &mut Editor, win: WinHandle) -> Result<i64, ApiError> {
    let win = resolve_window(editor, win)?;
    if let Some(config) = editor.window_config(win).map_err(exception)? {
        return i64::try_from(config.height)
            .map_err(|_| ApiError::exception("Window height exceeds API integer range"));
    }
    i64::try_from(editor.window_geometry(win).map_err(exception)?.height)
        .map_err(|_| ApiError::exception("Window height exceeds API integer range"))
}

#[api(since = 1, deprecated_since = 15, method)]
pub fn nvim_win_set_height(
    editor: &mut Editor,
    win: WinHandle,
    height: i64,
) -> Result<(), ApiError> {
    let height = usize::try_from(height)
        .ok()
        .filter(|height| *height > 0)
        .ok_or_else(|| ApiError::validation("Height must be greater than zero"))?;
    set_dimension(editor, win, None, Some(height))
}

#[api(since = 1, method)]
pub fn nvim_win_get_width(editor: &mut Editor, win: WinHandle) -> Result<i64, ApiError> {
    let win = resolve_window(editor, win)?;
    if let Some(config) = editor.window_config(win).map_err(exception)? {
        return i64::try_from(config.width)
            .map_err(|_| ApiError::exception("Window width exceeds API integer range"));
    }
    i64::try_from(editor.window_geometry(win).map_err(exception)?.width)
        .map_err(|_| ApiError::exception("Window width exceeds API integer range"))
}

#[api(since = 1, deprecated_since = 15, method)]
pub fn nvim_win_set_width(
    editor: &mut Editor,
    win: WinHandle,
    width: i64,
) -> Result<(), ApiError> {
    let width = usize::try_from(width)
        .ok()
        .filter(|width| *width > 0)
        .ok_or_else(|| ApiError::validation("Width must be greater than zero"))?;
    set_dimension(editor, win, Some(width), None)
}

#[api(since = 1, method)]
pub fn nvim_win_get_position(editor: &mut Editor, win: WinHandle) -> Result<Vec<i64>, ApiError> {
    let win = resolve_window(editor, win)?;
    let geometry = editor.window_geometry(win).map_err(exception)?;
    Ok(vec![geometry.row as i64, geometry.col as i64])
}

#[api(since = 1, method)]
pub fn nvim_win_get_var(
    editor: &mut Editor,
    win: WinHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let win = resolve_window(editor, win)?;
    editor
        .window_variables(win)
        .map_err(exception)?
        .get(&name)
        .cloned()
        .ok_or_else(|| ApiError::exception(format!("Key not found: {}", name.to_string_lossy())))
}

#[api(since = 1, method)]
pub fn nvim_win_set_var(
    editor: &mut Editor,
    win: WinHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let win = resolve_window(editor, win)?;
    editor
        .window_variables_mut(win)
        .map_err(exception)?
        .insert(name, value);
    Ok(())
}

#[api(since = 1, method)]
pub fn nvim_win_del_var(
    editor: &mut Editor,
    win: WinHandle,
    name: OxStr,
) -> Result<(), ApiError> {
    let win = resolve_window(editor, win)?;
    let variables = editor.window_variables_mut(win).map_err(exception)?;
    let Some(index) = variables.iter().position(|(candidate, _)| candidate == &name) else {
        return Err(ApiError::exception(format!(
            "Key not found: {}",
            name.to_string_lossy()
        )));
    };
    variables.0.remove(index);
    Ok(())
}

#[api(since = 1, deprecated_since = 11, method)]
pub fn nvim_win_get_option(
    editor: &mut Editor,
    window: WinHandle,
    name: OxStr,
) -> Result<Object, ApiError> {
    let window = resolve_window(editor, window)?;
    let name = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Option name must be valid UTF-8"))?;
    editor
        .options()
        .get_window(window, name)
        .map(option_to_object)
        .map_err(exception)
}

#[api(since = 1, deprecated_since = 11, method)]
pub fn nvim_win_set_option(
    editor: &mut Editor,
    window: WinHandle,
    name: OxStr,
    value: Object,
) -> Result<(), ApiError> {
    let window = resolve_window(editor, window)?;
    let name = std::str::from_utf8(name.as_bytes())
        .map_err(|_| ApiError::validation("Option name must be valid UTF-8"))?;
    let metadata = ox_editor::OptionStore::metadata(name).map_err(exception)?;
    let value = crate::global::object_to_legacy_option_value(metadata, name, value)?;
    editor
        .options_mut()
        .set_window(window, name, value)
        .map_err(exception)
}

#[api(since = 1, method)]
pub fn nvim_win_get_tabpage(
    editor: &mut Editor,
    win: WinHandle,
) -> Result<TabHandle, ApiError> {
    let win = resolve_window(editor, win)?;
    window_tabpage(editor, win)
}

#[api(since = 1, method)]
pub fn nvim_win_get_number(editor: &mut Editor, win: WinHandle) -> Result<i64, ApiError> {
    let win = resolve_window(editor, win)?;
    let tab = window_tabpage(editor, win)?;
    let windows = editor.tabpage(tab).map_err(exception)?.windows();
    windows
        .iter()
        .position(|candidate| *candidate == win)
        .map(|index| index as i64 + 1)
        .ok_or_else(|| ApiError::exception("Window is not in its owning tabpage"))
}

#[api(since = 1, method)]
pub fn nvim_win_is_valid(editor: &mut Editor, win: WinHandle) -> Result<bool, ApiError> {
    if win.is_current() {
        return Ok(resolve_window(editor, win).is_ok());
    }
    Ok(editor.window(win).is_ok())
}

#[api(since = 7, textlock, method)]
pub fn nvim_win_hide(editor: &mut Editor, win: WinHandle) -> Result<(), ApiError> {
    let win = resolve_window(editor, win)?;
    let tab = window_tabpage(editor, win)?;
    editor.close_window(tab, win, true).map_err(exception)?;
    Ok(())
}

#[api(since = 6, textlock, method)]
pub fn nvim_win_close(
    editor: &mut Editor,
    win: WinHandle,
    force: bool,
) -> Result<(), ApiError> {
    let win = resolve_window(editor, win)?;
    let tab = window_tabpage(editor, win)?;
    // Buffer modified-state is not modeled yet. Closing still follows normal
    // hidden-buffer retention; `force` has no observable distinction until it is.
    let _ = force;
    editor.close_window(tab, win, true).map_err(exception)?;
    Ok(())
}

#[api(since = 7, method)]
pub fn nvim_win_call(
    editor: &mut Editor,
    win: WinHandle,
    _function: LuaRef,
) -> Result<Object, ApiError> {
    resolve_window(editor, win)?;
    Err(ApiError::exception("Not implemented: nvim_win_call"))
}

#[api(since = 10, method)]
pub fn nvim_win_set_hl_ns(
    editor: &mut Editor,
    win: WinHandle,
    ns_id: i64,
) -> Result<(), ApiError> {
    if ns_id < -1 {
        return Err(ApiError::validation(
            "Namespace must be greater than or equal to -1",
        ));
    }
    let win = resolve_window(editor, win)?;
    editor
        .set_window_highlight_namespace(win, ns_id)
        .map_err(exception)
}

#[api(since = 6, textlock)]
pub fn nvim_open_win(
    editor: &mut Editor,
    buf: BufHandle,
    enter: bool,
    config: Dict,
) -> Result<WinHandle, ApiError> {
    let buffer = resolve_buffer(editor, buf)?;
    reject_unsupported_keys(&config)?;
    reject_external(&config)?;
    // A `split` config creates a normal (tiled) split window instead of a
    // floating one (api.txt: "- split:", nvim/api/win_config.c:231-244).
    if key(&config, "split").is_some() {
        validate_float_flags(&config)?;
        return open_split_window(editor, buffer, enter, &config);
    }
    let config = parse_config(editor, &config, None)?;
    let tab = editor
        .current_tabpage()
        .ok_or_else(|| ApiError::exception("no current tabpage"))?;
    let window = editor.open_float(tab, buffer, config).map_err(exception)?;
    if enter {
        editor.set_current_window(window).map_err(exception)?;
    }
    Ok(window)
}

/// Resolves the window `config.win` selects as the split target, defaulting to
/// the current window. The target may live on any tabpage (upstream: "Can be
/// in a different tab page"). Splitting a floating window is rejected
/// (nvim/api/win_config.c: "Cannot split a floating window").
fn parse_split_target(editor: &Editor, config: &Dict) -> Result<WinHandle, ApiError> {
    let target = match key(config, "win") {
        None | Some(Object::Nil) => resolve_window(editor, WinHandle::CURRENT)?,
        Some(Object::Window(window)) => resolve_window(editor, *window)?,
        Some(Object::Integer(window)) => WinHandle::try_from(*window)
            .map_err(|error| invalid("win", error))
            .and_then(|window| resolve_window(editor, window))?,
        Some(_) => return Err(invalid("win", "expected Window")),
    };
    if editor.window_config(target).map_err(exception)?.is_some() {
        return Err(ApiError::exception("Cannot split a floating window"));
    }
    Ok(target)
}

/// Opens a tiled split for a `config.split` request. The target window honors
/// `config.win` (any tabpage) and the four-way direction follows upstream:
/// `left`/`right` split vertically with the new window before/after, and
/// `above`/`below` split horizontally with the new window before/after.
fn open_split_window(
    editor: &mut Editor,
    buffer: BufHandle,
    enter: bool,
    config: &Dict,
) -> Result<WinHandle, ApiError> {
    let direction = parse_config_split(config)?;
    let target = parse_split_target(editor, config)?;
    // `target` may belong to a non-current tabpage; split it there.
    let tab = window_tabpage(editor, target)?;
    let window = match direction {
        SplitDirection::Left => editor.split_left(tab, target, buffer),
        SplitDirection::Right => editor.split_vertical(tab, target, buffer),
        SplitDirection::Above => editor.split_above(tab, target, buffer),
        SplitDirection::Below => editor.split_horizontal(tab, target, buffer),
    }
    .map_err(exception)?;
    let width = positive_size(config, "width", false)?;
    let height = positive_size(config, "height", false)?;
    set_dimension(editor, window, width, height)?;
    if enter {
        editor.set_current_window(window).map_err(exception)?;
    }
    Ok(window)
}

#[api(since = 6, method)]
pub fn nvim_win_set_config(
    editor: &mut Editor,
    win: WinHandle,
    config: Dict,
) -> Result<(), ApiError> {
    let win = resolve_window(editor, win)?;
    let current = editor
        .window_config(win)
        .map_err(exception)?
        .cloned()
        .ok_or_else(|| {
            ApiError::validation(
                "Unsupported window configuration transformation: tiled to floating",
            )
        })?;
    let updated = parse_config(editor, &config, Some(&current))?;
    editor.set_window_config(win, updated).map_err(exception)
}

#[api(since = 6, method)]
pub fn nvim_win_get_config(editor: &mut Editor, win: WinHandle) -> Result<Dict, ApiError> {
    let win = resolve_window(editor, win)?;
    let config = editor.window_config(win).map_err(exception)?;
    Ok(config_to_dict(config))
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_win_get_buf__API_META(), nvim_win_get_buf__API_DISPATCH)?;
    registry.register(nvim_win_set_buf__API_META(), nvim_win_set_buf__API_DISPATCH)?;
    registry.register(nvim_win_get_cursor__API_META(), nvim_win_get_cursor__API_DISPATCH)?;
    registry.register(nvim_win_set_cursor__API_META(), nvim_win_set_cursor__API_DISPATCH)?;
    registry.register(nvim_win_get_height__API_META(), nvim_win_get_height__API_DISPATCH)?;
    registry.register(nvim_win_set_height__API_META(), nvim_win_set_height__API_DISPATCH)?;
    registry.register(nvim_win_get_width__API_META(), nvim_win_get_width__API_DISPATCH)?;
    registry.register(nvim_win_set_width__API_META(), nvim_win_set_width__API_DISPATCH)?;
    registry.register(nvim_win_get_position__API_META(), nvim_win_get_position__API_DISPATCH)?;
    registry.register(nvim_win_get_var__API_META(), nvim_win_get_var__API_DISPATCH)?;
    registry.register(nvim_win_set_var__API_META(), nvim_win_set_var__API_DISPATCH)?;
    registry.register(nvim_win_del_var__API_META(), nvim_win_del_var__API_DISPATCH)?;
    registry.register(nvim_win_get_option__API_META(), nvim_win_get_option__API_DISPATCH)?;
    registry.register(nvim_win_set_option__API_META(), nvim_win_set_option__API_DISPATCH)?;
    registry.register(nvim_win_get_tabpage__API_META(), nvim_win_get_tabpage__API_DISPATCH)?;
    registry.register(nvim_win_get_number__API_META(), nvim_win_get_number__API_DISPATCH)?;
    registry.register(nvim_win_is_valid__API_META(), nvim_win_is_valid__API_DISPATCH)?;
    registry.register(nvim_win_hide__API_META(), nvim_win_hide__API_DISPATCH)?;
    registry.register(nvim_win_close__API_META(), nvim_win_close__API_DISPATCH)?;
    registry.register(nvim_win_call__API_META(), nvim_win_call__API_DISPATCH)?;
    registry.register(nvim_win_set_hl_ns__API_META(), nvim_win_set_hl_ns__API_DISPATCH)?;
    registry.register(nvim_open_win__API_META(), nvim_open_win__API_DISPATCH)?;
    registry.register(nvim_win_set_config__API_META(), nvim_win_set_config__API_DISPATCH)?;
    registry.register(nvim_win_get_config__API_META(), nvim_win_get_config__API_DISPATCH)?;
    Ok(())
}
