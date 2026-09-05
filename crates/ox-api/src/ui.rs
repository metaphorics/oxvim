//! UI attachment, highlight, input, paste, and terminal APIs.

#![allow(non_snake_case)]
use std::collections::BTreeMap;

use ox_editor::{Editor, Geometry, Keys, NullExprEval, RegisterContent, Remap, TypeaheadFlags};
use ox_text::Position;
use ox_types::WinHandle;
use ox_ui::{Highlight, HlAttrs, HlDef, HlState, UiOptions};

use crate::runtime::ChannelInfo;
use crate::session::{ApiSession, SessionState};
use crate::{ApiError, BufHandle, Dict, Object, OxStr, Registry, RegistryError, api};

const CHANNEL_ID: u64 = 1;

fn dimension(value: i64, name: &str) -> Result<usize, ApiError> {
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| ApiError::validation(format!("{name} must be positive")))
}
fn resize_current_tabpage(
    session: &ApiSession,
    width: usize,
    height: usize,
) -> Result<(), ApiError> {
    let geometry = Geometry::new(0, 0, width, height)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    session.with_editor_mut(|editor| {
        editor
            .resize_tabpage(ox_types::TabHandle::CURRENT, geometry)
            .map_err(|error| ApiError::exception(error.to_string()))
    })
}

fn ui_dict(id: u64, channel: &ox_ui::UiChannel) -> Dict {
    let (width, height) = channel.size();
    let opts = channel.options();
    Dict(vec![
        (
            OxStr::from("chan"),
            Object::Integer(i64::try_from(id).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("width"),
            Object::Integer(i64::try_from(width).unwrap_or(i64::MAX)),
        ),
        (
            OxStr::from("height"),
            Object::Integer(i64::try_from(height).unwrap_or(i64::MAX)),
        ),
        (OxStr::from("rgb"), Object::Boolean(true)),
        (
            OxStr::from("ext_linegrid"),
            Object::Boolean(opts.ext_linegrid),
        ),
        (
            OxStr::from("ext_multigrid"),
            Object::Boolean(opts.ext_multigrid),
        ),
        (
            OxStr::from("ext_messages"),
            Object::Boolean(opts.ext_messages),
        ),
        (
            OxStr::from("ext_cmdline"),
            Object::Boolean(opts.ext_cmdline),
        ),
        (
            OxStr::from("ext_popupmenu"),
            Object::Boolean(opts.ext_popupmenu),
        ),
        (
            OxStr::from("ext_hlstate"),
            Object::Boolean(opts.ext_hlstate),
        ),
        (
            OxStr::from("ext_termcolors"),
            Object::Boolean(opts.ext_termcolors),
        ),
    ])
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires handlers to return typed API errors"
)]
#[api(since = 4)]
pub fn nvim_list_uis(session: &ApiSession) -> Result<Vec<Dict>, ApiError> {
    Ok(session.with_state(|state| {
        state
            .ui_channels
            .iter()
            .map(|(id, channel)| ui_dict(*id, channel))
            .collect()
    }))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes UI options as an owned Dictionary"
)]
#[api(since = 1)]
pub fn nvim_ui_attach(
    session: &ApiSession,
    width: i64,
    height: i64,
    options: Dict,
) -> Result<(), ApiError> {
    let width = dimension(width, "width")?;
    let height = dimension(height, "height")?;
    session.with_state_mut(|state| {
        state
            .ui_channels
            .attach(CHANNEL_ID, width, height, UiOptions::from_dict(&options))
            .map_err(|error| ApiError::exception(error.to_string()))
    })?;
    if let Err(error) = resize_current_tabpage(session, width, height) {
        session.with_state_mut(|state| {
            let _ = state.ui_channels.detach(CHANNEL_ID);
        });
        return Err(error);
    }
    Ok(())
}

#[api(since = 1)]
pub fn nvim_ui_detach(session: &ApiSession) -> Result<(), ApiError> {
    session.with_state_mut(|state| {
        state
            .ui_channels
            .detach(CHANNEL_ID)
            .map(|_| ())
            .map_err(|error| ApiError::exception(error.to_string()))
    })
}

#[api(since = 1)]
pub fn nvim_ui_try_resize(session: &ApiSession, width: i64, height: i64) -> Result<(), ApiError> {
    let width = dimension(width, "width")?;
    let height = dimension(height, "height")?;
    resize_current_tabpage(session, width, height)?;
    session.with_state_mut(|state| {
        state
            .ui_channels
            .try_resize(CHANNEL_ID, width, height)
            .map_err(|error| ApiError::exception(error.to_string()))
    })
}

/// Upstream cterm 256-color palette (`color_names` + `color_numbers_256`).
const CTERM_COLOR_NAMES: &[(&str, i64)] = &[
    ("Black", 0),
    ("DarkBlue", 4),
    ("DarkGreen", 2),
    ("DarkCyan", 6),
    ("DarkRed", 1),
    ("DarkMagenta", 5),
    ("Brown", 130),
    ("DarkYellow", 3),
    ("Gray", 248),
    ("Grey", 248),
    ("LightGray", 7),
    ("LightGrey", 7),
    ("DarkGray", 242),
    ("DarkGrey", 242),
    ("Blue", 12),
    ("LightBlue", 81),
    ("Green", 10),
    ("LightGreen", 121),
    ("Cyan", 14),
    ("LightCyan", 159),
    ("Red", 9),
    ("LightRed", 224),
    ("Magenta", 13),
    ("LightMagenta", 225),
    ("Yellow", 11),
    ("LightYellow", 229),
    ("White", 15),
    ("NONE", -1),
];

fn cterm_color(value: &OxStr) -> Option<i64> {
    CTERM_COLOR_NAMES
        .iter()
        .find(|(entry, _)| entry.as_bytes().eq_ignore_ascii_case(value.as_bytes()))
        .map(|(_, index)| *index)
}

fn type_name(value: &Object) -> &'static str {
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

fn invalid_arg_type(key: &str, expected: &str, value: &Object) -> ApiError {
    ApiError::validation(format!(
        "Invalid '{key}': expected {expected}, got {}",
        type_name(value)
    ))
}

fn invalid_color(name: &OxStr) -> ApiError {
    ApiError::validation(format!(
        "Invalid highlight color: '{}'",
        name.to_string_lossy()
    ))
}

/// Parses a color value like upstream `object_to_color`.
fn object_to_color(value: &Object, key: &str, rgb: bool) -> Result<Option<u32>, ApiError> {
    match value {
        Object::Nil => Ok(None),
        Object::Integer(value) => {
            if *value < 0 {
                Ok(None)
            } else if rgb {
                u32::try_from(*value)
                    .ok()
                    .filter(|v| *v <= 0xFF_FFFF)
                    .map(Some)
                    .ok_or_else(|| ApiError::validation(format!("Invalid '{key}': out of range")))
            } else {
                u8::try_from(*value)
                    .ok()
                    .map(u32::from)
                    .map(Some)
                    .ok_or_else(|| {
                        ApiError::validation(format!("Invalid highlight color: '{value}'"))
                    })
            }
        }
        Object::String(name) => {
            if name.as_bytes().is_empty() || name.as_bytes().eq_ignore_ascii_case(b"NONE") {
                return Ok(None);
            }
            if rgb {
                named_color(name)
                    .map(Some)
                    .ok_or_else(|| invalid_color(name))
            } else {
                cterm_color(name)
                    .filter(|index| *index >= 0)
                    .and_then(|index| u32::try_from(index).ok())
                    .map(Some)
                    .ok_or_else(|| invalid_color(name))
            }
        }
        _ => Err(invalid_arg_type(key, "String or Integer", value)),
    }
}

/// Canonical table of named colors and their RGB values, mirroring upstream
/// Neovim's `color_name_table` (`src/nvim/highlight_group.c`) entry for entry:
/// same names in the same case-insensitive order and identical RGB values.
///
/// [`nvim_get_color_map`] exposes these names verbatim — the mixed-case
/// spellings are the canonical keys functional tests index with, e.g.
/// `Screen.colors.Blue1` — while [`named_color`] resolves any casing.
/// Do not add local spellings or aliases: extend from upstream only.
#[expect(
    clippy::unreadable_literal,
    reason = "the canonical RGB table keeps fixed six-digit color notation"
)]
const COLOR_TABLE: &[(&str, u32)] = &[
    ("AliceBlue", 0xf0f8ff),
    ("AntiqueWhite", 0xfaebd7),
    ("AntiqueWhite1", 0xffefdb),
    ("AntiqueWhite2", 0xeedfcc),
    ("AntiqueWhite3", 0xcdc0b0),
    ("AntiqueWhite4", 0x8b8378),
    ("Aqua", 0x00ffff),
    ("Aquamarine", 0x7fffd4),
    ("Aquamarine1", 0x7fffd4),
    ("Aquamarine2", 0x76eec6),
    ("Aquamarine3", 0x66cdaa),
    ("Aquamarine4", 0x458b74),
    ("Azure", 0xf0ffff),
    ("Azure1", 0xf0ffff),
    ("Azure2", 0xe0eeee),
    ("Azure3", 0xc1cdcd),
    ("Azure4", 0x838b8b),
    ("Beige", 0xf5f5dc),
    ("Bisque", 0xffe4c4),
    ("Bisque1", 0xffe4c4),
    ("Bisque2", 0xeed5b7),
    ("Bisque3", 0xcdb79e),
    ("Bisque4", 0x8b7d6b),
    ("Black", 0x000000),
    ("BlanchedAlmond", 0xffebcd),
    ("Blue", 0x0000ff),
    ("Blue1", 0x0000ff),
    ("Blue2", 0x0000ee),
    ("Blue3", 0x0000cd),
    ("Blue4", 0x00008b),
    ("BlueViolet", 0x8a2be2),
    ("Brown", 0xa52a2a),
    ("Brown1", 0xff4040),
    ("Brown2", 0xee3b3b),
    ("Brown3", 0xcd3333),
    ("Brown4", 0x8b2323),
    ("BurlyWood", 0xdeb887),
    ("Burlywood1", 0xffd39b),
    ("Burlywood2", 0xeec591),
    ("Burlywood3", 0xcdaa7d),
    ("Burlywood4", 0x8b7355),
    ("CadetBlue", 0x5f9ea0),
    ("CadetBlue1", 0x98f5ff),
    ("CadetBlue2", 0x8ee5ee),
    ("CadetBlue3", 0x7ac5cd),
    ("CadetBlue4", 0x53868b),
    ("ChartReuse", 0x7fff00),
    ("Chartreuse1", 0x7fff00),
    ("Chartreuse2", 0x76ee00),
    ("Chartreuse3", 0x66cd00),
    ("Chartreuse4", 0x458b00),
    ("Chocolate", 0xd2691e),
    ("Chocolate1", 0xff7f24),
    ("Chocolate2", 0xee7621),
    ("Chocolate3", 0xcd661d),
    ("Chocolate4", 0x8b4513),
    ("Coral", 0xff7f50),
    ("Coral1", 0xff7256),
    ("Coral2", 0xee6a50),
    ("Coral3", 0xcd5b45),
    ("Coral4", 0x8b3e2f),
    ("CornFlowerBlue", 0x6495ed),
    ("Cornsilk", 0xfff8dc),
    ("Cornsilk1", 0xfff8dc),
    ("Cornsilk2", 0xeee8cd),
    ("Cornsilk3", 0xcdc8b1),
    ("Cornsilk4", 0x8b8878),
    ("Crimson", 0xdc143c),
    ("Cyan", 0x00ffff),
    ("Cyan1", 0x00ffff),
    ("Cyan2", 0x00eeee),
    ("Cyan3", 0x00cdcd),
    ("Cyan4", 0x008b8b),
    ("DarkBlue", 0x00008b),
    ("DarkCyan", 0x008b8b),
    ("DarkGoldenrod", 0xb8860b),
    ("DarkGoldenrod1", 0xffb90f),
    ("DarkGoldenrod2", 0xeead0e),
    ("DarkGoldenrod3", 0xcd950c),
    ("DarkGoldenrod4", 0x8b6508),
    ("DarkGray", 0xa9a9a9),
    ("DarkGreen", 0x006400),
    ("DarkGrey", 0xa9a9a9),
    ("DarkKhaki", 0xbdb76b),
    ("DarkMagenta", 0x8b008b),
    ("DarkOliveGreen", 0x556b2f),
    ("DarkOliveGreen1", 0xcaff70),
    ("DarkOliveGreen2", 0xbcee68),
    ("DarkOliveGreen3", 0xa2cd5a),
    ("DarkOliveGreen4", 0x6e8b3d),
    ("DarkOrange", 0xff8c00),
    ("DarkOrange1", 0xff7f00),
    ("DarkOrange2", 0xee7600),
    ("DarkOrange3", 0xcd6600),
    ("DarkOrange4", 0x8b4500),
    ("DarkOrchid", 0x9932cc),
    ("DarkOrchid1", 0xbf3eff),
    ("DarkOrchid2", 0xb23aee),
    ("DarkOrchid3", 0x9a32cd),
    ("DarkOrchid4", 0x68228b),
    ("DarkRed", 0x8b0000),
    ("DarkSalmon", 0xe9967a),
    ("DarkSeaGreen", 0x8fbc8f),
    ("DarkSeaGreen1", 0xc1ffc1),
    ("DarkSeaGreen2", 0xb4eeb4),
    ("DarkSeaGreen3", 0x9bcd9b),
    ("DarkSeaGreen4", 0x698b69),
    ("DarkSlateBlue", 0x483d8b),
    ("DarkSlateGray", 0x2f4f4f),
    ("DarkSlateGray1", 0x97ffff),
    ("DarkSlateGray2", 0x8deeee),
    ("DarkSlateGray3", 0x79cdcd),
    ("DarkSlateGray4", 0x528b8b),
    ("DarkSlateGrey", 0x2f4f4f),
    ("DarkTurquoise", 0x00ced1),
    ("DarkViolet", 0x9400d3),
    ("DarkYellow", 0xbbbb00),
    ("DeepPink", 0xff1493),
    ("DeepPink1", 0xff1493),
    ("DeepPink2", 0xee1289),
    ("DeepPink3", 0xcd1076),
    ("DeepPink4", 0x8b0a50),
    ("DeepSkyBlue", 0x00bfff),
    ("DeepSkyBlue1", 0x00bfff),
    ("DeepSkyBlue2", 0x00b2ee),
    ("DeepSkyBlue3", 0x009acd),
    ("DeepSkyBlue4", 0x00688b),
    ("DimGray", 0x696969),
    ("DimGrey", 0x696969),
    ("DodgerBlue", 0x1e90ff),
    ("DodgerBlue1", 0x1e90ff),
    ("DodgerBlue2", 0x1c86ee),
    ("DodgerBlue3", 0x1874cd),
    ("DodgerBlue4", 0x104e8b),
    ("Firebrick", 0xb22222),
    ("Firebrick1", 0xff3030),
    ("Firebrick2", 0xee2c2c),
    ("Firebrick3", 0xcd2626),
    ("Firebrick4", 0x8b1a1a),
    ("FloralWhite", 0xfffaf0),
    ("ForestGreen", 0x228b22),
    ("Fuchsia", 0xff00ff),
    ("Gainsboro", 0xdcdcdc),
    ("GhostWhite", 0xf8f8ff),
    ("Gold", 0xffd700),
    ("Gold1", 0xffd700),
    ("Gold2", 0xeec900),
    ("Gold3", 0xcdad00),
    ("Gold4", 0x8b7500),
    ("Goldenrod", 0xdaa520),
    ("Goldenrod1", 0xffc125),
    ("Goldenrod2", 0xeeb422),
    ("Goldenrod3", 0xcd9b1d),
    ("Goldenrod4", 0x8b6914),
    ("Gray", 0x808080),
    ("Gray0", 0x000000),
    ("Gray1", 0x030303),
    ("Gray10", 0x1a1a1a),
    ("Gray100", 0xffffff),
    ("Gray11", 0x1c1c1c),
    ("Gray12", 0x1f1f1f),
    ("Gray13", 0x212121),
    ("Gray14", 0x242424),
    ("Gray15", 0x262626),
    ("Gray16", 0x292929),
    ("Gray17", 0x2b2b2b),
    ("Gray18", 0x2e2e2e),
    ("Gray19", 0x303030),
    ("Gray2", 0x050505),
    ("Gray20", 0x333333),
    ("Gray21", 0x363636),
    ("Gray22", 0x383838),
    ("Gray23", 0x3b3b3b),
    ("Gray24", 0x3d3d3d),
    ("Gray25", 0x404040),
    ("Gray26", 0x424242),
    ("Gray27", 0x454545),
    ("Gray28", 0x474747),
    ("Gray29", 0x4a4a4a),
    ("Gray3", 0x080808),
    ("Gray30", 0x4d4d4d),
    ("Gray31", 0x4f4f4f),
    ("Gray32", 0x525252),
    ("Gray33", 0x545454),
    ("Gray34", 0x575757),
    ("Gray35", 0x595959),
    ("Gray36", 0x5c5c5c),
    ("Gray37", 0x5e5e5e),
    ("Gray38", 0x616161),
    ("Gray39", 0x636363),
    ("Gray4", 0x0a0a0a),
    ("Gray40", 0x666666),
    ("Gray41", 0x696969),
    ("Gray42", 0x6b6b6b),
    ("Gray43", 0x6e6e6e),
    ("Gray44", 0x707070),
    ("Gray45", 0x737373),
    ("Gray46", 0x757575),
    ("Gray47", 0x787878),
    ("Gray48", 0x7a7a7a),
    ("Gray49", 0x7d7d7d),
    ("Gray5", 0x0d0d0d),
    ("Gray50", 0x7f7f7f),
    ("Gray51", 0x828282),
    ("Gray52", 0x858585),
    ("Gray53", 0x878787),
    ("Gray54", 0x8a8a8a),
    ("Gray55", 0x8c8c8c),
    ("Gray56", 0x8f8f8f),
    ("Gray57", 0x919191),
    ("Gray58", 0x949494),
    ("Gray59", 0x969696),
    ("Gray6", 0x0f0f0f),
    ("Gray60", 0x999999),
    ("Gray61", 0x9c9c9c),
    ("Gray62", 0x9e9e9e),
    ("Gray63", 0xa1a1a1),
    ("Gray64", 0xa3a3a3),
    ("Gray65", 0xa6a6a6),
    ("Gray66", 0xa8a8a8),
    ("Gray67", 0xababab),
    ("Gray68", 0xadadad),
    ("Gray69", 0xb0b0b0),
    ("Gray7", 0x121212),
    ("Gray70", 0xb3b3b3),
    ("Gray71", 0xb5b5b5),
    ("Gray72", 0xb8b8b8),
    ("Gray73", 0xbababa),
    ("Gray74", 0xbdbdbd),
    ("Gray75", 0xbfbfbf),
    ("Gray76", 0xc2c2c2),
    ("Gray77", 0xc4c4c4),
    ("Gray78", 0xc7c7c7),
    ("Gray79", 0xc9c9c9),
    ("Gray8", 0x141414),
    ("Gray80", 0xcccccc),
    ("Gray81", 0xcfcfcf),
    ("Gray82", 0xd1d1d1),
    ("Gray83", 0xd4d4d4),
    ("Gray84", 0xd6d6d6),
    ("Gray85", 0xd9d9d9),
    ("Gray86", 0xdbdbdb),
    ("Gray87", 0xdedede),
    ("Gray88", 0xe0e0e0),
    ("Gray89", 0xe3e3e3),
    ("Gray9", 0x171717),
    ("Gray90", 0xe5e5e5),
    ("Gray91", 0xe8e8e8),
    ("Gray92", 0xebebeb),
    ("Gray93", 0xededed),
    ("Gray94", 0xf0f0f0),
    ("Gray95", 0xf2f2f2),
    ("Gray96", 0xf5f5f5),
    ("Gray97", 0xf7f7f7),
    ("Gray98", 0xfafafa),
    ("Gray99", 0xfcfcfc),
    ("Green", 0x008000),
    ("Green1", 0x00ff00),
    ("Green2", 0x00ee00),
    ("Green3", 0x00cd00),
    ("Green4", 0x008b00),
    ("GreenYellow", 0xadff2f),
    ("Grey", 0x808080),
    ("Grey0", 0x000000),
    ("Grey1", 0x030303),
    ("Grey10", 0x1a1a1a),
    ("Grey100", 0xffffff),
    ("Grey11", 0x1c1c1c),
    ("Grey12", 0x1f1f1f),
    ("Grey13", 0x212121),
    ("Grey14", 0x242424),
    ("Grey15", 0x262626),
    ("Grey16", 0x292929),
    ("Grey17", 0x2b2b2b),
    ("Grey18", 0x2e2e2e),
    ("Grey19", 0x303030),
    ("Grey2", 0x050505),
    ("Grey20", 0x333333),
    ("Grey21", 0x363636),
    ("Grey22", 0x383838),
    ("Grey23", 0x3b3b3b),
    ("Grey24", 0x3d3d3d),
    ("Grey25", 0x404040),
    ("Grey26", 0x424242),
    ("Grey27", 0x454545),
    ("Grey28", 0x474747),
    ("Grey29", 0x4a4a4a),
    ("Grey3", 0x080808),
    ("Grey30", 0x4d4d4d),
    ("Grey31", 0x4f4f4f),
    ("Grey32", 0x525252),
    ("Grey33", 0x545454),
    ("Grey34", 0x575757),
    ("Grey35", 0x595959),
    ("Grey36", 0x5c5c5c),
    ("Grey37", 0x5e5e5e),
    ("Grey38", 0x616161),
    ("Grey39", 0x636363),
    ("Grey4", 0x0a0a0a),
    ("Grey40", 0x666666),
    ("Grey41", 0x696969),
    ("Grey42", 0x6b6b6b),
    ("Grey43", 0x6e6e6e),
    ("Grey44", 0x707070),
    ("Grey45", 0x737373),
    ("Grey46", 0x757575),
    ("Grey47", 0x787878),
    ("Grey48", 0x7a7a7a),
    ("Grey49", 0x7d7d7d),
    ("Grey5", 0x0d0d0d),
    ("Grey50", 0x7f7f7f),
    ("Grey51", 0x828282),
    ("Grey52", 0x858585),
    ("Grey53", 0x878787),
    ("Grey54", 0x8a8a8a),
    ("Grey55", 0x8c8c8c),
    ("Grey56", 0x8f8f8f),
    ("Grey57", 0x919191),
    ("Grey58", 0x949494),
    ("Grey59", 0x969696),
    ("Grey6", 0x0f0f0f),
    ("Grey60", 0x999999),
    ("Grey61", 0x9c9c9c),
    ("Grey62", 0x9e9e9e),
    ("Grey63", 0xa1a1a1),
    ("Grey64", 0xa3a3a3),
    ("Grey65", 0xa6a6a6),
    ("Grey66", 0xa8a8a8),
    ("Grey67", 0xababab),
    ("Grey68", 0xadadad),
    ("Grey69", 0xb0b0b0),
    ("Grey7", 0x121212),
    ("Grey70", 0xb3b3b3),
    ("Grey71", 0xb5b5b5),
    ("Grey72", 0xb8b8b8),
    ("Grey73", 0xbababa),
    ("Grey74", 0xbdbdbd),
    ("Grey75", 0xbfbfbf),
    ("Grey76", 0xc2c2c2),
    ("Grey77", 0xc4c4c4),
    ("Grey78", 0xc7c7c7),
    ("Grey79", 0xc9c9c9),
    ("Grey8", 0x141414),
    ("Grey80", 0xcccccc),
    ("Grey81", 0xcfcfcf),
    ("Grey82", 0xd1d1d1),
    ("Grey83", 0xd4d4d4),
    ("Grey84", 0xd6d6d6),
    ("Grey85", 0xd9d9d9),
    ("Grey86", 0xdbdbdb),
    ("Grey87", 0xdedede),
    ("Grey88", 0xe0e0e0),
    ("Grey89", 0xe3e3e3),
    ("Grey9", 0x171717),
    ("Grey90", 0xe5e5e5),
    ("Grey91", 0xe8e8e8),
    ("Grey92", 0xebebeb),
    ("Grey93", 0xededed),
    ("Grey94", 0xf0f0f0),
    ("Grey95", 0xf2f2f2),
    ("Grey96", 0xf5f5f5),
    ("Grey97", 0xf7f7f7),
    ("Grey98", 0xfafafa),
    ("Grey99", 0xfcfcfc),
    ("Honeydew", 0xf0fff0),
    ("Honeydew1", 0xf0fff0),
    ("Honeydew2", 0xe0eee0),
    ("Honeydew3", 0xc1cdc1),
    ("Honeydew4", 0x838b83),
    ("HotPink", 0xff69b4),
    ("HotPink1", 0xff6eb4),
    ("HotPink2", 0xee6aa7),
    ("HotPink3", 0xcd6090),
    ("HotPink4", 0x8b3a62),
    ("IndianRed", 0xcd5c5c),
    ("IndianRed1", 0xff6a6a),
    ("IndianRed2", 0xee6363),
    ("IndianRed3", 0xcd5555),
    ("IndianRed4", 0x8b3a3a),
    ("Indigo", 0x4b0082),
    ("Ivory", 0xfffff0),
    ("Ivory1", 0xfffff0),
    ("Ivory2", 0xeeeee0),
    ("Ivory3", 0xcdcdc1),
    ("Ivory4", 0x8b8b83),
    ("Khaki", 0xf0e68c),
    ("Khaki1", 0xfff68f),
    ("Khaki2", 0xeee685),
    ("Khaki3", 0xcdc673),
    ("Khaki4", 0x8b864e),
    ("Lavender", 0xe6e6fa),
    ("LavenderBlush", 0xfff0f5),
    ("LavenderBlush1", 0xfff0f5),
    ("LavenderBlush2", 0xeee0e5),
    ("LavenderBlush3", 0xcdc1c5),
    ("LavenderBlush4", 0x8b8386),
    ("LawnGreen", 0x7cfc00),
    ("LemonChiffon", 0xfffacd),
    ("LemonChiffon1", 0xfffacd),
    ("LemonChiffon2", 0xeee9bf),
    ("LemonChiffon3", 0xcdc9a5),
    ("LemonChiffon4", 0x8b8970),
    ("LightBlue", 0xadd8e6),
    ("LightBlue1", 0xbfefff),
    ("LightBlue2", 0xb2dfee),
    ("LightBlue3", 0x9ac0cd),
    ("LightBlue4", 0x68838b),
    ("LightCoral", 0xf08080),
    ("LightCyan", 0xe0ffff),
    ("LightCyan1", 0xe0ffff),
    ("LightCyan2", 0xd1eeee),
    ("LightCyan3", 0xb4cdcd),
    ("LightCyan4", 0x7a8b8b),
    ("LightGoldenrod", 0xeedd82),
    ("LightGoldenrod1", 0xffec8b),
    ("LightGoldenrod2", 0xeedc82),
    ("LightGoldenrod3", 0xcdbe70),
    ("LightGoldenrod4", 0x8b814c),
    ("LightGoldenrodYellow", 0xfafad2),
    ("LightGray", 0xd3d3d3),
    ("LightGreen", 0x90ee90),
    ("LightGrey", 0xd3d3d3),
    ("LightMagenta", 0xffbbff),
    ("LightPink", 0xffb6c1),
    ("LightPink1", 0xffaeb9),
    ("LightPink2", 0xeea2ad),
    ("LightPink3", 0xcd8c95),
    ("LightPink4", 0x8b5f65),
    ("LightRed", 0xffbbbb),
    ("LightSalmon", 0xffa07a),
    ("LightSalmon1", 0xffa07a),
    ("LightSalmon2", 0xee9572),
    ("LightSalmon3", 0xcd8162),
    ("LightSalmon4", 0x8b5742),
    ("LightSeaGreen", 0x20b2aa),
    ("LightSkyBlue", 0x87cefa),
    ("LightSkyBlue1", 0xb0e2ff),
    ("LightSkyBlue2", 0xa4d3ee),
    ("LightSkyBlue3", 0x8db6cd),
    ("LightSkyBlue4", 0x607b8b),
    ("LightSlateBlue", 0x8470ff),
    ("LightSlateGray", 0x778899),
    ("LightSlateGrey", 0x778899),
    ("LightSteelBlue", 0xb0c4de),
    ("LightSteelBlue1", 0xcae1ff),
    ("LightSteelBlue2", 0xbcd2ee),
    ("LightSteelBlue3", 0xa2b5cd),
    ("LightSteelBlue4", 0x6e7b8b),
    ("LightYellow", 0xffffe0),
    ("LightYellow1", 0xffffe0),
    ("LightYellow2", 0xeeeed1),
    ("LightYellow3", 0xcdcdb4),
    ("LightYellow4", 0x8b8b7a),
    ("Lime", 0x00ff00),
    ("LimeGreen", 0x32cd32),
    ("Linen", 0xfaf0e6),
    ("Magenta", 0xff00ff),
    ("Magenta1", 0xff00ff),
    ("Magenta2", 0xee00ee),
    ("Magenta3", 0xcd00cd),
    ("Magenta4", 0x8b008b),
    ("Maroon", 0x800000),
    ("Maroon1", 0xff34b3),
    ("Maroon2", 0xee30a7),
    ("Maroon3", 0xcd2990),
    ("Maroon4", 0x8b1c62),
    ("MediumAquamarine", 0x66cdaa),
    ("MediumBlue", 0x0000cd),
    ("MediumOrchid", 0xba55d3),
    ("MediumOrchid1", 0xe066ff),
    ("MediumOrchid2", 0xd15fee),
    ("MediumOrchid3", 0xb452cd),
    ("MediumOrchid4", 0x7a378b),
    ("MediumPurple", 0x9370db),
    ("MediumPurple1", 0xab82ff),
    ("MediumPurple2", 0x9f79ee),
    ("MediumPurple3", 0x8968cd),
    ("MediumPurple4", 0x5d478b),
    ("MediumSeaGreen", 0x3cb371),
    ("MediumSlateBlue", 0x7b68ee),
    ("MediumSpringGreen", 0x00fa9a),
    ("MediumTurquoise", 0x48d1cc),
    ("MediumVioletRed", 0xc71585),
    ("MidnightBlue", 0x191970),
    ("MintCream", 0xf5fffa),
    ("MistyRose", 0xffe4e1),
    ("MistyRose1", 0xffe4e1),
    ("MistyRose2", 0xeed5d2),
    ("MistyRose3", 0xcdb7b5),
    ("MistyRose4", 0x8b7d7b),
    ("Moccasin", 0xffe4b5),
    ("NavajoWhite", 0xffdead),
    ("NavajoWhite1", 0xffdead),
    ("NavajoWhite2", 0xeecfa1),
    ("NavajoWhite3", 0xcdb38b),
    ("NavajoWhite4", 0x8b795e),
    ("Navy", 0x000080),
    ("NavyBlue", 0x000080),
    ("NvimDarkBlue", 0x004c73),
    ("NvimDarkCyan", 0x007373),
    ("NvimDarkGray1", 0x07080d),
    ("NvimDarkGray2", 0x14161b),
    ("NvimDarkGray3", 0x2c2e33),
    ("NvimDarkGray4", 0x4f5258),
    ("NvimDarkGreen", 0x005523),
    ("NvimDarkGrey1", 0x07080d),
    ("NvimDarkGrey2", 0x14161b),
    ("NvimDarkGrey3", 0x2c2e33),
    ("NvimDarkGrey4", 0x4f5258),
    ("NvimDarkMagenta", 0x470045),
    ("NvimDarkRed", 0x590008),
    ("NvimDarkYellow", 0x6b5300),
    ("NvimLightBlue", 0xa6dbff),
    ("NvimLightCyan", 0x8cf8f7),
    ("NvimLightGray1", 0xeef1f8),
    ("NvimLightGray2", 0xe0e2ea),
    ("NvimLightGray3", 0xc4c6cd),
    ("NvimLightGray4", 0x9b9ea4),
    ("NvimLightGreen", 0xb3f6c0),
    ("NvimLightGrey1", 0xeef1f8),
    ("NvimLightGrey2", 0xe0e2ea),
    ("NvimLightGrey3", 0xc4c6cd),
    ("NvimLightGrey4", 0x9b9ea4),
    ("NvimLightMagenta", 0xffcaff),
    ("NvimLightRed", 0xffc0b9),
    ("NvimLightYellow", 0xfce094),
    ("OldLace", 0xfdf5e6),
    ("Olive", 0x808000),
    ("OliveDrab", 0x6b8e23),
    ("OliveDrab1", 0xc0ff3e),
    ("OliveDrab2", 0xb3ee3a),
    ("OliveDrab3", 0x9acd32),
    ("OliveDrab4", 0x698b22),
    ("Orange", 0xffa500),
    ("Orange1", 0xffa500),
    ("Orange2", 0xee9a00),
    ("Orange3", 0xcd8500),
    ("Orange4", 0x8b5a00),
    ("OrangeRed", 0xff4500),
    ("OrangeRed1", 0xff4500),
    ("OrangeRed2", 0xee4000),
    ("OrangeRed3", 0xcd3700),
    ("OrangeRed4", 0x8b2500),
    ("Orchid", 0xda70d6),
    ("Orchid1", 0xff83fa),
    ("Orchid2", 0xee7ae9),
    ("Orchid3", 0xcd69c9),
    ("Orchid4", 0x8b4789),
    ("PaleGoldenrod", 0xeee8aa),
    ("PaleGreen", 0x98fb98),
    ("PaleGreen1", 0x9aff9a),
    ("PaleGreen2", 0x90ee90),
    ("PaleGreen3", 0x7ccd7c),
    ("PaleGreen4", 0x548b54),
    ("PaleTurquoise", 0xafeeee),
    ("PaleTurquoise1", 0xbbffff),
    ("PaleTurquoise2", 0xaeeeee),
    ("PaleTurquoise3", 0x96cdcd),
    ("PaleTurquoise4", 0x668b8b),
    ("PaleVioletRed", 0xdb7093),
    ("PaleVioletRed1", 0xff82ab),
    ("PaleVioletRed2", 0xee799f),
    ("PaleVioletRed3", 0xcd6889),
    ("PaleVioletRed4", 0x8b475d),
    ("PapayaWhip", 0xffefd5),
    ("PeachPuff", 0xffdab9),
    ("PeachPuff1", 0xffdab9),
    ("PeachPuff2", 0xeecbad),
    ("PeachPuff3", 0xcdaf95),
    ("PeachPuff4", 0x8b7765),
    ("Peru", 0xcd853f),
    ("Pink", 0xffc0cb),
    ("Pink1", 0xffb5c5),
    ("Pink2", 0xeea9b8),
    ("Pink3", 0xcd919e),
    ("Pink4", 0x8b636c),
    ("Plum", 0xdda0dd),
    ("Plum1", 0xffbbff),
    ("Plum2", 0xeeaeee),
    ("Plum3", 0xcd96cd),
    ("Plum4", 0x8b668b),
    ("PowderBlue", 0xb0e0e6),
    ("Purple", 0x800080),
    ("Purple1", 0x9b30ff),
    ("Purple2", 0x912cee),
    ("Purple3", 0x7d26cd),
    ("Purple4", 0x551a8b),
    ("RebeccaPurple", 0x663399),
    ("Red", 0xff0000),
    ("Red1", 0xff0000),
    ("Red2", 0xee0000),
    ("Red3", 0xcd0000),
    ("Red4", 0x8b0000),
    ("RosyBrown", 0xbc8f8f),
    ("RosyBrown1", 0xffc1c1),
    ("RosyBrown2", 0xeeb4b4),
    ("RosyBrown3", 0xcd9b9b),
    ("RosyBrown4", 0x8b6969),
    ("RoyalBlue", 0x4169e1),
    ("RoyalBlue1", 0x4876ff),
    ("RoyalBlue2", 0x436eee),
    ("RoyalBlue3", 0x3a5fcd),
    ("RoyalBlue4", 0x27408b),
    ("SaddleBrown", 0x8b4513),
    ("Salmon", 0xfa8072),
    ("Salmon1", 0xff8c69),
    ("Salmon2", 0xee8262),
    ("Salmon3", 0xcd7054),
    ("Salmon4", 0x8b4c39),
    ("SandyBrown", 0xf4a460),
    ("SeaGreen", 0x2e8b57),
    ("SeaGreen1", 0x54ff9f),
    ("SeaGreen2", 0x4eee94),
    ("SeaGreen3", 0x43cd80),
    ("SeaGreen4", 0x2e8b57),
    ("SeaShell", 0xfff5ee),
    ("Seashell1", 0xfff5ee),
    ("Seashell2", 0xeee5de),
    ("Seashell3", 0xcdc5bf),
    ("Seashell4", 0x8b8682),
    ("Sienna", 0xa0522d),
    ("Sienna1", 0xff8247),
    ("Sienna2", 0xee7942),
    ("Sienna3", 0xcd6839),
    ("Sienna4", 0x8b4726),
    ("Silver", 0xc0c0c0),
    ("SkyBlue", 0x87ceeb),
    ("SkyBlue1", 0x87ceff),
    ("SkyBlue2", 0x7ec0ee),
    ("SkyBlue3", 0x6ca6cd),
    ("SkyBlue4", 0x4a708b),
    ("SlateBlue", 0x6a5acd),
    ("SlateBlue1", 0x836fff),
    ("SlateBlue2", 0x7a67ee),
    ("SlateBlue3", 0x6959cd),
    ("SlateBlue4", 0x473c8b),
    ("SlateGray", 0x708090),
    ("SlateGray1", 0xc6e2ff),
    ("SlateGray2", 0xb9d3ee),
    ("SlateGray3", 0x9fb6cd),
    ("SlateGray4", 0x6c7b8b),
    ("SlateGrey", 0x708090),
    ("Snow", 0xfffafa),
    ("Snow1", 0xfffafa),
    ("Snow2", 0xeee9e9),
    ("Snow3", 0xcdc9c9),
    ("Snow4", 0x8b8989),
    ("SpringGreen", 0x00ff7f),
    ("SpringGreen1", 0x00ff7f),
    ("SpringGreen2", 0x00ee76),
    ("SpringGreen3", 0x00cd66),
    ("SpringGreen4", 0x008b45),
    ("SteelBlue", 0x4682b4),
    ("SteelBlue1", 0x63b8ff),
    ("SteelBlue2", 0x5cacee),
    ("SteelBlue3", 0x4f94cd),
    ("SteelBlue4", 0x36648b),
    ("Tan", 0xd2b48c),
    ("Tan1", 0xffa54f),
    ("Tan2", 0xee9a49),
    ("Tan3", 0xcd853f),
    ("Tan4", 0x8b5a2b),
    ("Teal", 0x008080),
    ("Thistle", 0xd8bfd8),
    ("Thistle1", 0xffe1ff),
    ("Thistle2", 0xeed2ee),
    ("Thistle3", 0xcdb5cd),
    ("Thistle4", 0x8b7b8b),
    ("Tomato", 0xff6347),
    ("Tomato1", 0xff6347),
    ("Tomato2", 0xee5c42),
    ("Tomato3", 0xcd4f39),
    ("Tomato4", 0x8b3626),
    ("Turquoise", 0x40e0d0),
    ("Turquoise1", 0x00f5ff),
    ("Turquoise2", 0x00e5ee),
    ("Turquoise3", 0x00c5cd),
    ("Turquoise4", 0x00868b),
    ("Violet", 0xee82ee),
    ("VioletRed", 0xd02090),
    ("VioletRed1", 0xff3e96),
    ("VioletRed2", 0xee3a8c),
    ("VioletRed3", 0xcd3278),
    ("VioletRed4", 0x8b2252),
    ("WebGray", 0x808080),
    ("WebGreen", 0x008000),
    ("WebGrey", 0x808080),
    ("WebMaroon", 0x800000),
    ("WebPurple", 0x800080),
    ("Wheat", 0xf5deb3),
    ("Wheat1", 0xffe7ba),
    ("Wheat2", 0xeed8ae),
    ("Wheat3", 0xcdba96),
    ("Wheat4", 0x8b7e66),
    ("White", 0xffffff),
    ("WhiteSmoke", 0xf5f5f5),
    ("X11Gray", 0xbebebe),
    ("X11Green", 0x00ff00),
    ("X11Grey", 0xbebebe),
    ("X11Maroon", 0xb03060),
    ("X11Purple", 0xa020f0),
    ("Yellow", 0xffff00),
    ("Yellow1", 0xffff00),
    ("Yellow2", 0xeeee00),
    ("Yellow3", 0xcdcd00),
    ("Yellow4", 0x8b8b00),
    ("YellowGreen", 0x9acd32),
];

fn named_color(value: &OxStr) -> Option<u32> {
    let name = std::str::from_utf8(value.as_bytes()).ok()?;
    if let Some(hex) = name.strip_prefix('#') {
        return (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten();
    }
    COLOR_TABLE
        .iter()
        .find_map(|(entry, rgb)| entry.eq_ignore_ascii_case(name).then_some(*rgb))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes color names as owned Strings"
)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires handlers to return typed API errors"
)]
#[api(since = 1)]
pub fn nvim_get_color_by_name(name: OxStr) -> Result<i64, ApiError> {
    Ok(named_color(&name).map_or(-1, i64::from))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires handlers to return typed API errors"
)]
#[api(since = 1)]
pub fn nvim_get_color_map() -> Result<Dict, ApiError> {
    Ok(Dict(
        COLOR_TABLE
            .iter()
            .map(|(name, rgb)| (OxStr::from(*name), Object::Integer(i64::from(*rgb))))
            .collect(),
    ))
}

/// Raw highlight options extracted from a `Dict` for canonical conversion.
struct HlRaw<'a> {
    fg: Option<&'a Object>,
    foreground: Option<&'a Object>,
    bg: Option<&'a Object>,
    background: Option<&'a Object>,
    sp: Option<&'a Object>,
    special: Option<&'a Object>,
    blend: Option<&'a Object>,
    cterm: Option<Dict>,
    ctermfg: Option<&'a Object>,
    ctermbg: Option<&'a Object>,
    link: Option<&'a Object>,
    link_global: Option<&'a Object>,
    font: Option<&'a Object>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    undercurl: Option<bool>,
    underdouble: Option<bool>,
    underdotted: Option<bool>,
    underdashed: Option<bool>,
    standout: Option<bool>,
    strikethrough: Option<bool>,
    altfont: Option<bool>,
    dim: Option<bool>,
    blink: Option<bool>,
    conceal: Option<bool>,
    overline: Option<bool>,
    reverse: Option<bool>,
    fg_indexed: Option<bool>,
    bg_indexed: Option<bool>,
    nocombine: Option<bool>,
    default: Option<bool>,
    update: bool,
    force: bool,
    fallback: bool,
}

fn parse_bool(value: &Object, key: &str) -> Result<bool, ApiError> {
    match value {
        Object::Boolean(enabled) => Ok(*enabled),
        _ => Err(invalid_arg_type(key, "Boolean", value)),
    }
}

fn parse_blend(value: &Object) -> Result<u8, ApiError> {
    match value {
        Object::Integer(value) => {
            let value = u8::try_from(*value)
                .map_err(|_| ApiError::validation("Invalid 'blend': out of range".to_string()))?;
            if value <= 100 {
                Ok(value)
            } else {
                Err(ApiError::validation(
                    "Invalid 'blend': out of range".to_string(),
                ))
            }
        }
        _ => Err(invalid_arg_type("blend", "Integer", value)),
    }
}

fn parse_font(value: &Object) -> Result<Option<OxStr>, ApiError> {
    match value {
        Object::Nil => Ok(None),
        Object::String(name) => {
            if name.as_bytes().is_empty() || name.as_bytes().eq_ignore_ascii_case(b"NONE") {
                Ok(None)
            } else {
                Ok(Some(name.clone()))
            }
        }
        _ => Err(invalid_arg_type("font", "String", value)),
    }
}

fn extract_hl_raw(dict: &Dict) -> Result<HlRaw<'_>, ApiError> {
    let mut raw = HlRaw {
        fg: None,
        foreground: None,
        bg: None,
        background: None,
        sp: None,
        special: None,
        blend: None,
        cterm: None,
        ctermfg: None,
        ctermbg: None,
        link: None,
        link_global: None,
        font: None,
        bold: None,
        italic: None,
        underline: None,
        undercurl: None,
        underdouble: None,
        underdotted: None,
        underdashed: None,
        standout: None,
        strikethrough: None,
        altfont: None,
        dim: None,
        blink: None,
        conceal: None,
        overline: None,
        reverse: None,
        fg_indexed: None,
        bg_indexed: None,
        nocombine: None,
        default: None,
        update: false,
        force: false,
        fallback: false,
    };
    for (key, value) in &dict.0 {
        match key.as_bytes() {
            b"fg" => raw.fg = Some(value),
            b"foreground" => raw.foreground = Some(value),
            b"bg" => raw.bg = Some(value),
            b"background" => raw.background = Some(value),
            b"sp" => raw.sp = Some(value),
            b"special" => raw.special = Some(value),
            b"bold" => raw.bold = Some(parse_bool(value, "bold")?),
            b"italic" => raw.italic = Some(parse_bool(value, "italic")?),
            b"underline" => raw.underline = Some(parse_bool(value, "underline")?),
            b"undercurl" => raw.undercurl = Some(parse_bool(value, "undercurl")?),
            b"underdouble" => raw.underdouble = Some(parse_bool(value, "underdouble")?),
            b"underdotted" => raw.underdotted = Some(parse_bool(value, "underdotted")?),
            b"underdashed" => raw.underdashed = Some(parse_bool(value, "underdashed")?),
            b"standout" => raw.standout = Some(parse_bool(value, "standout")?),
            b"strikethrough" => raw.strikethrough = Some(parse_bool(value, "strikethrough")?),
            b"altfont" => raw.altfont = Some(parse_bool(value, "altfont")?),
            b"dim" => raw.dim = Some(parse_bool(value, "dim")?),
            b"blink" => raw.blink = Some(parse_bool(value, "blink")?),
            b"conceal" => raw.conceal = Some(parse_bool(value, "conceal")?),
            b"overline" => raw.overline = Some(parse_bool(value, "overline")?),
            b"reverse" => raw.reverse = Some(parse_bool(value, "reverse")?),
            b"fg_indexed" => raw.fg_indexed = Some(parse_bool(value, "fg_indexed")?),
            b"bg_indexed" => raw.bg_indexed = Some(parse_bool(value, "bg_indexed")?),
            b"nocombine" => raw.nocombine = Some(parse_bool(value, "nocombine")?),
            b"blend" => raw.blend = Some(value),
            b"cterm" => match value {
                Object::Dict(dict) => raw.cterm = Some(dict.clone()),
                // Lua 5.1 has one table type, so an empty `{}` reaches the
                // generic converter as an empty array even in a Dict field.
                Object::Array(items) if items.is_empty() => raw.cterm = Some(Dict(Vec::new())),
                _ => return Err(invalid_arg_type("cterm", "Dictionary", value)),
            },
            b"ctermfg" => raw.ctermfg = Some(value),
            b"ctermbg" => raw.ctermbg = Some(value),
            b"link" => raw.link = Some(value),
            b"link_global" => raw.link_global = Some(value),
            b"font" => raw.font = Some(value),
            b"default" => raw.default = Some(parse_bool(value, "default")?),
            b"update" => raw.update = parse_bool(value, "update")?,
            b"force" => raw.force = parse_bool(value, "force")?,
            b"fallback" => raw.fallback = parse_bool(value, "fallback")?,
            b"url" => return Err(ApiError::validation("Invalid key: 'url'".to_string())),
            _ => {
                return Err(ApiError::validation(format!(
                    "Invalid key: '{}'",
                    key.to_string_lossy()
                )));
            }
        }
    }
    Ok(raw)
}

fn is_underline_style(flag: &str) -> bool {
    matches!(
        flag,
        "underline" | "undercurl" | "underdouble" | "underdotted" | "underdashed"
    )
}

fn current_underline_style(attrs: &HlAttrs) -> Option<&'static str> {
    if attrs.underdashed {
        Some("underdashed")
    } else if attrs.underdotted {
        Some("underdotted")
    } else if attrs.underdouble {
        Some("underdouble")
    } else if attrs.undercurl {
        Some("undercurl")
    } else if attrs.underline {
        Some("underline")
    } else {
        None
    }
}

fn clear_underline_styles(attrs: &mut HlAttrs) {
    attrs.underline = false;
    attrs.undercurl = false;
    attrs.underdouble = false;
    attrs.underdotted = false;
    attrs.underdashed = false;
}

fn set_flag(attrs: &mut HlAttrs, flag: &str, value: Option<bool>) {
    let Some(value) = value else {
        return;
    };
    if is_underline_style(flag) {
        if value {
            clear_underline_styles(attrs);
        } else if current_underline_style(attrs) == Some(flag) {
            clear_underline_styles(attrs);
            return;
        } else {
            return;
        }
    }
    match flag {
        "reverse" => attrs.reverse = value,
        "bold" => attrs.bold = value,
        "italic" => attrs.italic = value,
        "underline" => attrs.underline = value,
        "undercurl" => attrs.undercurl = value,
        "underdouble" => attrs.underdouble = value,
        "underdotted" => attrs.underdotted = value,
        "underdashed" => attrs.underdashed = value,
        "standout" => attrs.standout = value,
        "strikethrough" => attrs.strikethrough = value,
        "altfont" => attrs.altfont = value,
        "dim" => attrs.dim = value,
        "blink" => attrs.blink = value,
        "conceal" => attrs.conceal = value,
        "overline" => attrs.overline = value,
        "fg_indexed" => attrs.fg_indexed = value,
        "bg_indexed" => attrs.bg_indexed = value,
        "nocombine" => attrs.nocombine = value,
        _ => {}
    }
}

fn cterm_flag(cterm: &Dict, flag: &str) -> Result<Option<bool>, ApiError> {
    match cterm
        .0
        .iter()
        .find(|(key, _)| key.as_bytes() == flag.as_bytes())
    {
        Some((_, Object::Boolean(value))) => Ok(Some(*value)),
        Some((_, Object::Nil)) | None => Ok(None),
        Some((_, value)) => Err(invalid_arg_type(flag, "Boolean", value)),
    }
}

fn resolve_link_target(value: &Object, registry: &mut HlState) -> Result<Option<u64>, ApiError> {
    match value {
        Object::Integer(id) if *id > 0 => u64::try_from(*id)
            .map(Some)
            .map_err(|_| ApiError::exception("highlight id out of range")),
        Object::Nil | Object::Integer(_) => Ok(None),
        Object::String(name) => {
            if name.as_bytes().is_empty() || name.as_bytes().eq_ignore_ascii_case(b"NONE") {
                Ok(None)
            } else {
                registry
                    .check_group(name)
                    .map(Some)
                    .map_err(|error| ApiError::exception(error.to_string()))
            }
        }
        _ => Err(ApiError::validation(
            "link must be a String or Integer".to_string(),
        )),
    }
}

fn dict_to_hl_def(raw: &HlRaw, base: Option<&HlDef>) -> Result<HlDef, ApiError> {
    let mut def = HlDef {
        rgb: base.map(|b| b.rgb.clone()).unwrap_or_default(),
        cterm: HlAttrs::default(),
        cterm_fg: base.and_then(|b| b.cterm_fg),
        cterm_bg: base.and_then(|b| b.cterm_bg),
        link: None,
        link_global: false,
        font: base.and_then(|b| b.font.clone()),
        default_flag: base.is_some_and(|b| b.default_flag),
    };
    apply_rgb_flags(&mut def, raw);
    // GUI colors.
    def.rgb.foreground = match raw.fg.or(raw.foreground) {
        Some(value) => object_to_color(value, "fg", true)?,
        None => def.rgb.foreground,
    };
    def.rgb.background = match raw.bg.or(raw.background) {
        Some(value) => object_to_color(value, "bg", true)?,
        None => def.rgb.background,
    };
    def.rgb.special = match raw.sp.or(raw.special) {
        Some(value) => object_to_color(value, "sp", true)?,
        None => def.rgb.special,
    };
    // Blend.
    def.rgb.blend = match raw.blend {
        Some(value) => Some(parse_blend(value)?),
        None => def.rgb.blend,
    };
    // Font.
    def.font = match raw.font {
        Some(value) => parse_font(value)?,
        None => def.font,
    };
    // Default flag.
    def.default_flag = raw
        .default
        .or(base.map(|b| b.default_flag))
        .unwrap_or_default();
    // Cterm colors.
    def.cterm_fg = match raw.ctermfg {
        Some(value) => object_to_color(value, "ctermfg", false)?,
        None => def.cterm_fg,
    };
    def.cterm_bg = match raw.ctermbg {
        Some(value) => object_to_color(value, "ctermbg", false)?,
        None => def.cterm_bg,
    };
    apply_cterm_flags(&mut def, raw)?;
    // Link target is resolved later by the caller with the correct namespace.
    Ok(def)
}

fn apply_rgb_flags(def: &mut HlDef, raw: &HlRaw) {
    for &flag in &[
        "reverse",
        "bold",
        "italic",
        "underline",
        "undercurl",
        "underdouble",
        "underdotted",
        "underdashed",
        "standout",
        "strikethrough",
        "altfont",
        "dim",
        "blink",
        "conceal",
        "overline",
        "fg_indexed",
        "bg_indexed",
        "nocombine",
    ] {
        let value = match flag {
            "reverse" => raw.reverse,
            "bold" => raw.bold,
            "italic" => raw.italic,
            "underline" => raw.underline,
            "undercurl" => raw.undercurl,
            "underdouble" => raw.underdouble,
            "underdotted" => raw.underdotted,
            "underdashed" => raw.underdashed,
            "standout" => raw.standout,
            "strikethrough" => raw.strikethrough,
            "altfont" => raw.altfont,
            "dim" => raw.dim,
            "blink" => raw.blink,
            "conceal" => raw.conceal,
            "overline" => raw.overline,
            "fg_indexed" => raw.fg_indexed,
            "bg_indexed" => raw.bg_indexed,
            "nocombine" => raw.nocombine,
            _ => None,
        };
        set_flag(&mut def.rgb, flag, value);
    }
}

fn apply_cterm_flags(def: &mut HlDef, raw: &HlRaw) -> Result<(), ApiError> {
    if let Some(cterm) = &raw.cterm {
        for &flag in &[
            "bold",
            "standout",
            "underline",
            "undercurl",
            "underdouble",
            "underdotted",
            "underdashed",
            "italic",
            "reverse",
            "altfont",
            "dim",
            "blink",
            "conceal",
            "overline",
            "nocombine",
        ] {
            let value = match flag {
                "bold" => cterm_flag(cterm, "bold")?,
                "standout" => cterm_flag(cterm, "standout")?,
                "underline" => cterm_flag(cterm, "underline")?,
                "undercurl" => cterm_flag(cterm, "undercurl")?,
                "underdouble" => cterm_flag(cterm, "underdouble")?,
                "underdotted" => cterm_flag(cterm, "underdotted")?,
                "underdashed" => cterm_flag(cterm, "underdashed")?,
                "italic" => cterm_flag(cterm, "italic")?,
                "reverse" => cterm_flag(cterm, "reverse")?,
                "altfont" => cterm_flag(cterm, "altfont")?,
                "dim" => cterm_flag(cterm, "dim")?,
                "blink" => cterm_flag(cterm, "blink")?,
                "conceal" => cterm_flag(cterm, "conceal")?,
                "overline" => cterm_flag(cterm, "overline")?,
                "nocombine" => cterm_flag(cterm, "nocombine")?,
                _ => None,
            };
            set_flag(&mut def.cterm, flag, value);
        }
    } else {
        def.cterm = def.rgb.clone();
    }
    Ok(())
}

fn resolve_link_def(def: &HlDef, namespaces: &BTreeMap<i64, HlState>, ns_id: i64) -> HlDef {
    let mut current = def.clone();
    let mut current_ns = ns_id;
    for _ in 0..64 {
        let Some(next) = current.link else {
            return current;
        };
        current_ns = if current.link_global { 0 } else { current_ns };
        let Some(ns) = namespaces.get(&current_ns) else {
            return current;
        };
        let Some(next_def) = ns.group_def(next) else {
            return current;
        };
        if next_def.link.is_none() {
            return next_def.clone();
        }
        current = next_def.clone();
    }
    current
}

fn project_protocol_hl(def: &HlDef) -> Highlight {
    let mut cterm = def.cterm.clone();
    cterm.foreground = def.cterm_fg;
    cterm.background = def.cterm_bg;
    cterm.special = None;
    cterm.url = None;
    cterm.fg_indexed = false;
    cterm.bg_indexed = false;
    Highlight {
        rgb: def.rgb.clone(),
        cterm,
        cterm_explicit: true,
        default_flag: false,
        info: Vec::new(),
    }
}

fn underline_style(attrs: &HlAttrs) -> Option<&'static str> {
    if attrs.underdashed {
        Some("underdashed")
    } else if attrs.underdotted {
        Some("underdotted")
    } else if attrs.underdouble {
        Some("underdouble")
    } else if attrs.undercurl {
        Some("undercurl")
    } else if attrs.underline {
        Some("underline")
    } else {
        None
    }
}

fn push_color(out: &mut Vec<(OxStr, Object)>, name: &'static str, color: Option<u32>) {
    if let Some(color) = color {
        out.push((OxStr::from(name), Object::Integer(i64::from(color))));
    }
}

fn push_flag(out: &mut Vec<(OxStr, Object)>, name: &'static str, enabled: bool) {
    if enabled {
        out.push((OxStr::from(name), Object::Boolean(true)));
    }
}

fn hl_attrs_to_dict(attrs: &HlAttrs, out: &mut Vec<(OxStr, Object)>) {
    for (name, value) in [
        ("reverse", attrs.reverse),
        ("bold", attrs.bold),
        ("italic", attrs.italic),
        ("standout", attrs.standout),
        ("strikethrough", attrs.strikethrough),
        ("altfont", attrs.altfont),
        ("dim", attrs.dim),
        ("blink", attrs.blink),
        ("conceal", attrs.conceal),
        ("overline", attrs.overline),
        ("nocombine", attrs.nocombine),
    ] {
        push_flag(out, name, value);
    }
    if let Some(style) = underline_style(attrs) {
        push_flag(out, style, true);
    }
}

fn hl_def_to_dict(def: &HlDef, link_name: Option<&OxStr>) -> Dict {
    let mut out = Vec::new();
    if def.default_flag {
        push_flag(&mut out, "default", true);
    }
    if let Some(name) = link_name {
        out.push((OxStr::from("link"), Object::String(name.clone())));
    }
    push_color(&mut out, "fg", def.rgb.foreground);
    push_color(&mut out, "bg", def.rgb.background);
    push_color(&mut out, "sp", def.rgb.special);
    hl_attrs_to_dict(&def.rgb, &mut out);
    push_flag(&mut out, "fg_indexed", def.rgb.fg_indexed);
    push_flag(&mut out, "bg_indexed", def.rgb.bg_indexed);
    if let Some(font) = &def.font {
        out.push((OxStr::from("font"), Object::String(font.clone())));
    }
    if let Some(blend) = def.rgb.blend {
        out.push((OxStr::from("blend"), Object::Integer(i64::from(blend))));
    }
    push_color(&mut out, "ctermfg", def.cterm_fg);
    push_color(&mut out, "ctermbg", def.cterm_bg);
    let mut cterm = Vec::new();
    hl_attrs_to_dict(&def.cterm, &mut cterm);
    if !cterm.is_empty() {
        out.push((OxStr::from("cterm"), Object::Dict(Dict(cterm))));
    }
    Dict(out)
}

fn build_group_dict(
    def: &HlDef,
    link_name: Option<&OxStr>,
    namespaces: &BTreeMap<i64, HlState>,
    ns_id: i64,
    follow_link: bool,
) -> Dict {
    if follow_link {
        hl_def_to_dict(def, link_name)
    } else {
        let effective = resolve_link_def(def, namespaces, ns_id);
        hl_def_to_dict(&effective, None)
    }
}

fn hldef_has_settings(def: &HlDef) -> bool {
    def.rgb.foreground.is_some()
        || def.rgb.background.is_some()
        || def.rgb.special.is_some()
        || def.rgb.blend.is_some()
        || def.rgb.url.is_some()
        || def.rgb.bold
        || def.rgb.italic
        || def.rgb.underline
        || def.rgb.undercurl
        || def.rgb.underdouble
        || def.rgb.underdotted
        || def.rgb.underdashed
        || def.rgb.strikethrough
        || def.rgb.reverse
        || def.rgb.standout
        || def.rgb.altfont
        || def.rgb.dim
        || def.rgb.blink
        || def.rgb.conceal
        || def.rgb.overline
        || def.rgb.nocombine
        || def.rgb.fg_indexed
        || def.rgb.bg_indexed
        || def.cterm.bold
        || def.cterm.italic
        || def.cterm.underline
        || def.cterm.undercurl
        || def.cterm.underdouble
        || def.cterm.underdotted
        || def.cterm.underdashed
        || def.cterm.strikethrough
        || def.cterm.reverse
        || def.cterm.standout
        || def.cterm.altfont
        || def.cterm.dim
        || def.cterm.blink
        || def.cterm.conceal
        || def.cterm.overline
        || def.cterm.nocombine
        || def.cterm_fg.is_some()
        || def.cterm_bg.is_some()
        || def.link.is_some()
        || def.font.is_some()
}

/// Rebuilds the active render table from the given namespace's definitions.
fn activate_hl(state: &mut SessionState, ns_id: i64) {
    let active = state.hl_namespaces.entry(ns_id).or_default().clone();
    state.highlights = active;
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes highlight attributes as an owned Dictionary"
)]
#[api(since = 7)]
pub fn nvim_set_hl(
    session: &ApiSession,
    ns_id: i64,
    name: OxStr,
    val: Dict,
) -> Result<(), ApiError> {
    if ns_id < 0 {
        return Err(ApiError::validation("namespace must be non-negative"));
    }
    let raw = extract_hl_raw(&val)?;
    session.with_state_mut(|state| {
        // Canonical group id and existing definition in this namespace.
        let (gid, existing) = {
            let ns = state.hl_namespaces.entry(ns_id).or_default();
            let gid = ns
                .check_group(&name)
                .map_err(|error| ApiError::exception(error.to_string()))?;
            let existing = ns.group_def(gid).cloned();
            (gid, existing)
        };
        if raw.default.unwrap_or(false)
            && !raw.force
            && let Some(existing) = &existing
            && !existing.default_flag
            && hldef_has_settings(existing)
        {
            return Ok(());
        }
        // Resolve the existing definition so `update` inherits resolved colors/flags.
        let resolved_base = if raw.update {
            let namespaces = &state.hl_namespaces;
            existing
                .as_ref()
                .map(|def| resolve_link_def(def, namespaces, ns_id))
        } else {
            None
        };
        // Resolve link targets in the appropriate namespace (global for link_global).
        let link_target = match (raw.link_global, raw.link) {
            (Some(value), _) => {
                let global = state.hl_namespaces.entry(0).or_default();
                resolve_link_target(value, &mut *global)?
            }
            (None, Some(value)) => {
                let ns = state.hl_namespaces.entry(ns_id).or_default();
                resolve_link_target(value, &mut *ns)?
            }
            (None, None) => None,
        };
        let ns = state.hl_namespaces.entry(ns_id).or_default();
        let mut def = dict_to_hl_def(&raw, resolved_base.as_ref())?;
        def.link = link_target;
        def.link_global = raw.link_global.is_some() && link_target.is_some();
        if def.default_flag && !raw.force && existing.as_ref().is_some_and(hldef_has_settings) {
            return Ok(());
        }
        let protocol = project_protocol_hl(&def);
        let protocol_id = if let Some(current_id) = ns.group_id(&name) {
            let _ = ns
                .redefine(current_id, protocol)
                .map_err(|error| ApiError::exception(error.to_string()))?;
            current_id
        } else {
            let (new_id, _) = ns
                .intern(protocol)
                .map_err(|error| ApiError::exception(error.to_string()))?;
            new_id
        };
        ns.set_group(name.clone(), protocol_id)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        ns.set_group_def(gid, def);
        if ns_id == state.current_hl_ns {
            activate_hl(state, ns_id);
        }
        Ok(())
    })?;
    bridge_editor_highlights(session, &name)?;
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes highlight options as an owned Dictionary"
)]
#[api(since = 9)]
pub fn nvim_get_hl(session: &ApiSession, ns_id: i64, opts: Dict) -> Result<Dict, ApiError> {
    if ns_id < 0 {
        return Err(ApiError::validation("namespace must be non-negative"));
    }
    let by_id = match opts.get(&OxStr::from("id")) {
        Some(Object::Integer(id)) => Some(
            u64::try_from(*id).map_err(|_| ApiError::validation("Highlight id out of bounds"))?,
        ),
        Some(value) => return Err(invalid_arg_type("id", "Integer", value)),
        None => None,
    };
    let by_name = match opts.get(&OxStr::from("name")) {
        Some(Object::String(name)) => Some(name.clone()),
        Some(value) => return Err(invalid_arg_type("name", "String", value)),
        None => None,
    };
    let follow_link = match opts.get(&OxStr::from("link")) {
        Some(Object::Boolean(false)) => false,
        None | Some(Object::Boolean(true)) => true,
        Some(value) => return Err(invalid_arg_type("link", "Boolean", value)),
    };
    let create = match opts.get(&OxStr::from("create")) {
        Some(Object::Boolean(true)) => true,
        None | Some(Object::Boolean(false)) => false,
        Some(value) => return Err(invalid_arg_type("create", "Boolean", value)),
    };
    let mut created = false;
    let result = session.with_state_mut(|state| {
        if let Some(id) = by_id {
            hl_group_by_id(state, ns_id, id, follow_link)
        } else if let Some(name) = &by_name {
            let gid = if create {
                created = true;
                let ns = state.hl_namespaces.entry(ns_id).or_default();
                ns.check_group(name)
                    .map_err(|_| ApiError::validation("Highlight id out of bounds"))?
            } else {
                let Some(ns) = state.hl_namespaces.get(&ns_id) else {
                    return Err(ApiError::validation("highlight group not found"));
                };
                match ns.group_by_name(name) {
                    Some(gid) => gid,
                    None => return Err(ApiError::validation("highlight group not found")),
                }
            };
            Ok(hl_group_by_name(state, ns_id, gid, follow_link))
        } else {
            Ok(all_hl_groups(state, ns_id, follow_link))
        }
    });
    if created && let Some(name) = &by_name {
        bridge_editor_highlights(session, name)?;
    }
    result
}

fn hl_group_by_id(
    state: &mut SessionState,
    ns_id: i64,
    id: u64,
    follow_link: bool,
) -> Result<Dict, ApiError> {
    let (def, ns_id_for_link) = {
        let ns = state.hl_namespaces.entry(ns_id).or_default();
        if id == 0 || id > ns.group_count() {
            return Err(ApiError::validation("Highlight id out of bounds"));
        }
        let Some(def) = ns.group_def(id).cloned() else {
            return Ok(Dict(Vec::new()));
        };
        (def, ns_id)
    };
    let namespaces = &state.hl_namespaces;
    let link_name = resolve_link_name(&def, namespaces, ns_id_for_link, follow_link);
    Ok(build_group_dict(
        &def,
        link_name.as_ref(),
        namespaces,
        ns_id_for_link,
        follow_link,
    ))
}

fn hl_group_by_name(state: &mut SessionState, ns_id: i64, gid: u64, follow_link: bool) -> Dict {
    let namespaces = &state.hl_namespaces;
    let Some(ns) = namespaces.get(&ns_id) else {
        return Dict(Vec::new());
    };
    let Some(def) = ns.group_def(gid).cloned() else {
        return Dict(Vec::new());
    };
    let link_name = resolve_link_name(&def, namespaces, ns_id, follow_link);
    build_group_dict(&def, link_name.as_ref(), namespaces, ns_id, follow_link)
}

fn all_hl_groups(state: &mut SessionState, ns_id: i64, follow_link: bool) -> Dict {
    let namespaces = &state.hl_namespaces;
    let Some(ns) = namespaces.get(&ns_id) else {
        return Dict(Vec::new());
    };
    let mut out = Vec::new();
    for (name, _gid, def) in ns.iter_group_defs() {
        let link_name = resolve_link_name(def, namespaces, ns_id, follow_link);
        let dict = build_group_dict(def, link_name.as_ref(), namespaces, ns_id, follow_link);
        out.push((name.clone(), Object::Dict(dict)));
    }
    Dict(out)
}

fn resolve_link_name(
    def: &HlDef,
    namespaces: &BTreeMap<i64, HlState>,
    ns_id: i64,
    follow_link: bool,
) -> Option<OxStr> {
    if !follow_link {
        return None;
    }
    def.link.and_then(|target| {
        let registry = if def.link_global {
            namespaces.get(&0)
        } else {
            namespaces.get(&ns_id)
        };
        registry.and_then(|ns| ns.group_name(target).cloned())
    })
}

#[api(since = 7)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
pub fn nvim_get_hl_id_by_name(session: &ApiSession, name: OxStr) -> Result<i64, ApiError> {
    let id = session.with_state_mut(|state| {
        let ns = state.hl_namespaces.entry(0).or_default();
        ns.check_group(&name)
            .map_err(|error| ApiError::exception(error.to_string()))
    })?;
    bridge_editor_highlights(session, &name)?;
    i64::try_from(id).map_err(|_| ApiError::exception("highlight id out of range"))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the generated dispatcher binds and moves owned RPC arguments"
)]
#[api(since = 12)]
pub fn nvim_get_hl_ns(session: &ApiSession, opts: Dict) -> Result<i64, ApiError> {
    match opts.get(&OxStr::from("winid")) {
        None => Ok(session.with_state(|state| state.current_hl_ns)),
        Some(Object::Integer(winid)) => session.with_editor(|editor| {
            let win = if *winid == 0 {
                editor
                    .current_window()
                    .ok_or_else(|| ApiError::exception("No current window"))?
            } else {
                WinHandle::try_from(*winid)
                    .map_err(|error| ApiError::validation(error.to_string()))?
            };
            editor
                .window_highlight_namespace(win)
                .map_err(|error| ApiError::exception(error.to_string()))
        }),
        Some(value) => Err(invalid_arg_type("winid", "Integer", value)),
    }
}

fn bridge_editor_highlights(session: &ApiSession, name: &OxStr) -> Result<(), ApiError> {
    let name_string = String::from_utf8(name.as_bytes().to_vec())
        .map_err(|_| ApiError::validation("highlight name is not valid UTF-8"))?;
    session.with_editor_mut(|editor| {
        editor.highlights_mut().insert(name_string, BTreeMap::new());
    });
    Ok(())
}

#[api(since = 10)]
pub fn nvim_set_hl_ns(session: &ApiSession, ns_id: i64) -> Result<(), ApiError> {
    if ns_id < 0 {
        return Err(ApiError::validation("namespace must be non-negative"));
    }
    session.with_state_mut(|state| {
        state.current_hl_ns = ns_id;
        activate_hl(state, ns_id);
    });
    Ok(())
}

#[api(since = 10, fast)]
pub fn nvim_set_hl_ns_fast(session: &ApiSession, ns_id: i64) -> Result<(), ApiError> {
    if ns_id < 0 {
        return Err(ApiError::validation("namespace must be non-negative"));
    }
    session.with_state_mut(|state| {
        state.fast_hl_ns = ns_id;
        activate_hl(state, ns_id);
    });
    Ok(())
}

#[api(since = 6)]
pub fn nvim_create_buf(
    session: &ApiSession,
    listed: bool,
    scratch: bool,
) -> Result<BufHandle, ApiError> {
    let _ = scratch;
    session.with_editor_mut(|editor| {
        editor
            .create_buffer(listed)
            .map_err(|error| ApiError::exception(error.to_string()))
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes terminal options as an owned Dict"
)]
#[api(since = 5)]
pub fn nvim_open_term(
    session: &ApiSession,
    buffer: BufHandle,
    opts: Dict,
) -> Result<i64, ApiError> {
    let callback = match opts.get(&OxStr::from("on_input")) {
        Some(Object::LuaRef(reference)) => Some(
            usize::try_from(*reference)
                .map_err(|_| ApiError::validation("Invalid 'on_input': expected LuaRef"))?,
        ),
        Some(Object::Nil) | None => None,
        Some(_) => {
            return Err(ApiError::validation("Invalid 'on_input': expected LuaRef"));
        }
    };
    let buffer = session.with_editor(|editor| {
        if buffer.is_current() {
            editor
                .current_buffer()
                .ok_or_else(|| ApiError::validation("No current buffer"))
        } else {
            editor
                .buffer(buffer)
                .map_err(|error| ApiError::validation(error.to_string()))?;
            Ok(buffer)
        }
    })?;
    let channel = session.with_editor(Editor::allocate_channel_id);
    session.with_state_mut(|state| {
        state
            .channels
            .insert(channel, ChannelInfo::terminal(channel, i64::from(buffer)));
        state.terminal_inputs.insert(
            channel,
            crate::runtime::TerminalInput {
                callback,
                bracketed_paste: false,
            },
        );
        i64::try_from(channel)
            .map_err(|_| ApiError::exception("channel id exceeds API Integer range"))
    })
}

fn queue(session: &ApiSession, data: &[u8], remap: Remap) {
    let keys = Keys::encode(data);
    session.with_editor_mut(|editor| {
        editor.typeahead_mut().append(
            &keys,
            TypeaheadFlags {
                remap,
                ..TypeaheadFlags::default()
            },
        );
    });
}

fn normalize_paste(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            output.push(b'\n');
            index += usize::from(bytes.get(index + 1) == Some(&b'\n'));
        } else {
            output.push(bytes[index]);
        }
        index += 1;
    }
    output
}

fn paste_lines(bytes: &[u8], crlf: bool) -> Vec<Object> {
    let normalized = if crlf {
        normalize_paste(bytes)
    } else {
        bytes.to_vec()
    };
    normalized
        .split(|byte| *byte == b'\n')
        .map(|line| {
            Object::String(OxStr(
                line.iter()
                    .map(|byte| if *byte == 0 { b'\n' } else { *byte })
                    .collect(),
            ))
        })
        .collect()
}

fn terminal_paste(
    session: &ApiSession,
    data: &OxStr,
    phase: i64,
) -> Result<Option<bool>, ApiError> {
    let Some(buffer) = session.with_editor(Editor::current_buffer) else {
        return Ok(None);
    };
    let terminal = session.with_state(|state| {
        state.channels.iter().find_map(|(&channel, info)| {
            (info.mode == OxStr::from("terminal") && info.buffer == Some(i64::from(buffer)))
                .then(|| {
                    state
                        .terminal_inputs
                        .get(&channel)
                        .map(|input| (channel, input.callback, input.bracketed_paste))
                })
                .flatten()
        })
    });
    let Some((channel, callback, bracketed)) = terminal else {
        return Ok(None);
    };
    let Some(callback) = callback else {
        return Ok(Some(true));
    };
    let mut chunks = Vec::new();
    if bracketed && phase < 2 {
        chunks.push(b"\x1b[200~".to_vec());
    }
    let mut start = 0;
    for (index, byte) in data.as_bytes().iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if start < index {
            chunks.push(data.as_bytes()[start..index].to_vec());
        }
        chunks.push(b"\n".to_vec());
        start = index + 1;
    }
    if start < data.as_bytes().len() {
        chunks.push(data.as_bytes()[start..].to_vec());
    }
    if bracketed && matches!(phase, -1 | 3) {
        chunks.push(b"\x1b[201~".to_vec());
    }
    crate::runtime::with_lua_executor(session, |session, executor| {
        for chunk in chunks {
            executor
                .call_ref(
                    session,
                    callback,
                    vec![
                        Object::String(OxStr::from("input")),
                        Object::Integer(i64::try_from(channel).unwrap_or(i64::MAX)),
                        Object::Buffer(buffer),
                        Object::String(OxStr(chunk)),
                    ],
                )
                .map_err(ApiError::exception)?;
        }
        Ok(())
    })?;
    Ok(Some(true))
}

fn with_paste_machine(
    session: &ApiSession,
    operation: impl FnOnce(&mut ox_editor::ModeMachine),
) -> Result<(), ApiError> {
    let machine = crate::runtime::mode_machine(session)
        .ok_or_else(|| ApiError::exception("input mode state is not installed"))?;
    let mut machine = machine
        .try_borrow_mut()
        .map_err(|_| ApiError::exception("input mode state is busy"))?;
    operation(&mut machine);
    Ok(())
}

/// Pastes one complete payload or one phase of a streamed payload.
///
/// # Errors
///
/// Returns an error for an invalid phase or when the active paste handler fails.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes paste data as an owned String"
)]
#[api(since = 6)]
pub fn nvim_paste(
    session: &ApiSession,
    data: OxStr,
    crlf: bool,
    phase: i64,
) -> Result<bool, ApiError> {
    if ![-1, 1, 2, 3].contains(&phase) {
        return Err(ApiError::validation(format!("Invalid 'phase': {phase}")));
    }
    let starts = phase < 2;
    let ends = matches!(phase, -1 | 3);
    let record = session.requesting_channel().is_some();
    if starts {
        session.with_state_mut(|state| state.paste_cancelled = false);
        with_paste_machine(session, |machine| machine.paste_store_start(record))?;
    } else if session.with_state(|state| state.paste_cancelled) {
        return Ok(false);
    }

    let result = match terminal_paste(session, &data, phase) {
        Ok(Some(result)) => Ok(Object::Boolean(result)),
        Ok(None) => {
            let lines = paste_lines(data.as_bytes(), crlf);
            session.with_editor_mut(Editor::sync_current_undo);
            let result = crate::runtime::with_lua_executor(session, |session, executor| {
                executor
                    .exec(
                        session,
                        "return vim.paste(...)",
                        vec![Object::Array(lines), Object::Integer(phase)],
                    )
                    .map_err(ApiError::exception)
            });
            session.with_editor_mut(Editor::sync_current_undo);
            result
        }
        Err(error) => Err(error),
    };
    let cancelled = match result {
        Ok(Object::Boolean(false)) => true,
        Ok(_) => {
            with_paste_machine(session, |machine| {
                machine.paste_store_content(data.as_bytes(), crlf, record);
            })?;
            false
        }
        Err(error) => {
            session.with_state_mut(|state| state.paste_cancelled = true);
            with_paste_machine(session, |machine| machine.paste_store_end(record))?;
            return Err(error);
        }
    };
    session.with_state_mut(|state| state.paste_cancelled = cancelled);
    if ends || cancelled {
        with_paste_machine(session, |machine| machine.paste_store_end(record))?;
    }
    Ok(!cancelled)
}

fn delete_visual_for_put(session: &ApiSession, after: &mut bool) -> Result<(), ApiError> {
    let Some(machine) = crate::runtime::mode_machine(session) else {
        return Ok(());
    };
    let mut machine = machine.borrow_mut();
    let ox_editor::Mode::Visual(state) = machine.mode() else {
        return Ok(());
    };
    let state = state.clone();
    let (buffer, character_at_eol, linewise_at_eof) = session.with_editor(|editor| {
        let buffer = editor
            .current_buffer()
            .ok_or_else(|| ApiError::validation("No current buffer"))?;
        let text = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let range = state.range();
        let character_at_eol = state.kind == ox_editor::VisualKind::Character
            && text
                .line(range.end.lnum)
                .is_ok_and(|line| range.end.col >= line.len().saturating_sub(1));
        let linewise_at_eof =
            state.kind == ox_editor::VisualKind::Line && range.end.lnum == text.line_count();
        Ok((buffer, character_at_eol, linewise_at_eof))
    })?;
    session.with_editor_mut(|editor| {
        machine
            .feed_keys(editor, "x", &mut NullExprEval)
            .map_err(|error| ApiError::exception(error.to_string()))
    })?;
    *after |= character_at_eol;
    if !linewise_at_eof {
        return Ok(());
    }

    session.with_editor_mut(|editor| {
        let window = editor
            .current_window()
            .ok_or_else(|| ApiError::validation("No current window"))?;
        let cursor = editor
            .window(window)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .cursor;
        let text = editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .text()
            .map_err(|error| ApiError::exception(error.to_string()))?;
        let last = text.line_count();
        let canonical_empty = last == 1 && text.line(1).is_ok_and(|line| line.is_empty());
        let target_lnum = if canonical_empty {
            1
        } else {
            editor
                .append_buffer_lines(buffer, last, &[Vec::new()], cursor, 0)
                .map_err(|error| ApiError::exception(error.to_string()))?;
            last.saturating_add(1)
        };
        editor
            .set_window_cursor(
                window,
                Position {
                    lnum: target_lnum,
                    col: 0,
                },
            )
            .map_err(|error| ApiError::exception(error.to_string()))
    })?;
    Ok(())
}

fn nvim_put_kind(lines: &[Object], put_type: &OxStr) -> Result<ox_editor::RegisterKind, ApiError> {
    let block_width = |declared: usize| {
        lines
            .iter()
            .filter_map(|line| match line {
                Object::String(line) => Some(line.0.len()),
                _ => None,
            })
            .max()
            .unwrap_or(1)
            .max(declared)
            .max(1)
    };
    match put_type.as_bytes() {
        b"" | b"v" | b"c" => Ok(ox_editor::RegisterKind::CharacterWise),
        b"V" | b"l" => Ok(ox_editor::RegisterKind::LineWise),
        b"b" | [0x16] => Ok(ox_editor::RegisterKind::BlockWise {
            width: block_width(1),
        }),
        [b'b' | 0x16, digits @ ..] if digits.iter().all(u8::is_ascii_digit) => {
            let declared = digits
                .iter()
                .try_fold(0_i32, |width, digit| {
                    width.checked_mul(10)?.checked_add(i32::from(*digit - b'0'))
                })
                .and_then(|width| usize::try_from(width).ok())
                .unwrap_or(1)
                .max(1);
            Ok(ox_editor::RegisterKind::BlockWise {
                width: block_width(declared),
            })
        }
        _ => Err(ApiError::validation(format!(
            "Invalid 'type': '{}'",
            put_type.to_string_lossy()
        ))),
    }
}

fn nvim_put_origin(
    session: &ApiSession,
    buffer: BufHandle,
    cursor: Position,
    kind: ox_editor::RegisterKind,
    after: bool,
) -> Result<Position, ApiError> {
    if matches!(kind, ox_editor::RegisterKind::LineWise) {
        return Ok(Position {
            lnum: if after {
                cursor.lnum
            } else {
                cursor.lnum.saturating_sub(1)
            },
            col: 0,
        });
    }
    if !after {
        return Ok(cursor);
    }
    let line = session.with_editor(|editor| {
        editor
            .buffer(buffer)
            .map_err(|error| ApiError::validation(error.to_string()))
            .and_then(|state| {
                state
                    .text()
                    .map(|text| text.line(cursor.lnum).unwrap_or_default())
                    .map_err(|error| ApiError::exception(error.to_string()))
            })
    })?;
    let width = line.get(cursor.col).map_or(0, |byte| match byte {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    });
    Ok(Position {
        lnum: cursor.lnum,
        col: cursor.col.saturating_add(width).min(line.len()),
    })
}

fn nvim_put_positions(
    content: &RegisterContent,
    kind: ox_editor::RegisterKind,
    origin: Position,
) -> (Position, Position, Position) {
    let last_line = content.lines().last().map_or(&[][..], Vec::as_slice);
    let last_scalar_len = if last_line.is_empty() {
        0
    } else {
        1 + last_line
            .iter()
            .rev()
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count()
    };
    if matches!(kind, ox_editor::RegisterKind::LineWise) {
        let lnum = origin.lnum.saturating_add(content.lines().len());
        return (
            Position {
                lnum: origin.lnum.saturating_add(1),
                col: 0,
            },
            Position { lnum, col: 0 },
            Position { lnum, col: 0 },
        );
    }
    let lnum = origin
        .lnum
        .saturating_add(content.lines().len().saturating_sub(1));
    let col = match kind {
        ox_editor::RegisterKind::CharacterWise if content.lines().len() > 1 => last_line.len(),
        _ => origin.col.saturating_add(last_line.len()),
    };
    (
        origin,
        Position {
            lnum,
            col: col.saturating_sub(last_scalar_len),
        },
        Position { lnum, col },
    )
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes the put type as an owned String"
)]
#[api(since = 6)]
pub fn nvim_put(
    session: &ApiSession,
    lines: Vec<Object>,
    put_type: OxStr,
    after: bool,
    follow: bool,
) -> Result<(), ApiError> {
    let kind = nvim_put_kind(&lines, &put_type)?;
    if lines.is_empty() {
        return Ok(());
    }
    let lines = lines
        .into_iter()
        .map(|line| match line {
            Object::String(line) => Ok(line),
            value => Err(invalid_arg_type("line", "String", &value)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut after = after;
    delete_visual_for_put(session, &mut after)?;
    let binary_lines = lines
        .into_iter()
        .map(|mut line| {
            for byte in &mut line.0 {
                if *byte == b'\n' {
                    *byte = 0;
                }
            }
            line.0
        })
        .collect();
    let content = RegisterContent::new(kind, binary_lines)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    let (window, buffer, cursor) = session.with_editor(|editor| {
        let window = editor
            .current_window()
            .ok_or_else(|| ApiError::validation("No current window"))?;
        let buffer = editor
            .current_buffer()
            .ok_or_else(|| ApiError::validation("No current buffer"))?;
        let cursor = editor
            .window(window)
            .map_err(|error| ApiError::validation(error.to_string()))?
            .cursor;
        Ok((window, buffer, cursor))
    })?;
    let origin = nvim_put_origin(session, buffer, cursor, kind, after)?;
    let (start_mark, end_mark, follow_position) = nvim_put_positions(&content, kind, origin);
    session.with_editor_mut(|editor| {
        editor
            .put_content(buffer, origin, &content, 0)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        editor
            .set_local_mark(buffer, '[', start_mark)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        editor
            .set_local_mark(buffer, ']', end_mark)
            .map_err(|error| ApiError::exception(error.to_string()))?;
        if follow {
            editor
                .set_window_cursor(window, follow_position)
                .map_err(|error| ApiError::exception(error.to_string()))?;
        }
        Ok(())
    })?;
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the RPC ABI deserializes keys and mode as owned Strings"
)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires handlers to return typed API errors"
)]
#[api(since = 1)]
pub fn nvim_feedkeys(
    session: &ApiSession,
    keys: OxStr,
    mode: OxStr,
    escape_ks: bool,
) -> Result<(), ApiError> {
    let _ = escape_ks;
    let remap = if mode.as_bytes().contains(&b'n') {
        Remap::No
    } else {
        Remap::Yes
    };
    if mode.as_bytes().contains(&b'x') {
        session.with_editor_mut(|editor| editor.typeahead_mut().flush());
    }
    queue(session, keys.as_bytes(), remap);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the RPC ABI requires handlers to return typed API errors"
)]
#[api(since = 6)]
pub fn nvim_select_popupmenu_item(
    session: &ApiSession,
    item: i64,
    insert: bool,
    finish: bool,
    opts: Dict,
) -> Result<(), ApiError> {
    let _ = (insert, finish);
    drop(opts);
    session.with_state_mut(|state| state.chrome.select_popupmenu(item));
    Ok(())
}

pub(crate) fn register(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(nvim_list_uis__API_META(), nvim_list_uis__API_DISPATCH)?;
    registry.register(nvim_ui_attach__API_META(), nvim_ui_attach__API_DISPATCH)?;
    registry.register(nvim_ui_detach__API_META(), nvim_ui_detach__API_DISPATCH)?;
    registry.register(
        nvim_ui_try_resize__API_META(),
        nvim_ui_try_resize__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_color_by_name__API_META(),
        nvim_get_color_by_name__API_DISPATCH,
    )?;
    registry.register(
        nvim_get_color_map__API_META(),
        nvim_get_color_map__API_DISPATCH,
    )?;
    registry.register(nvim_set_hl__API_META(), nvim_set_hl__API_DISPATCH)?;
    registry.register(nvim_get_hl__API_META(), nvim_get_hl__API_DISPATCH)?;
    registry.register(
        nvim_get_hl_id_by_name__API_META(),
        nvim_get_hl_id_by_name__API_DISPATCH,
    )?;
    registry.register(nvim_get_hl_ns__API_META(), nvim_get_hl_ns__API_DISPATCH)?;
    registry.register(nvim_set_hl_ns__API_META(), nvim_set_hl_ns__API_DISPATCH)?;
    registry.register(
        nvim_set_hl_ns_fast__API_META(),
        nvim_set_hl_ns_fast__API_DISPATCH,
    )?;
    registry.register(nvim_create_buf__API_META(), nvim_create_buf__API_DISPATCH)?;
    registry.register(nvim_open_term__API_META(), nvim_open_term__API_DISPATCH)?;
    registry.register(nvim_paste__API_META(), nvim_paste__API_DISPATCH)?;
    registry.register(nvim_put__API_META(), nvim_put__API_DISPATCH)?;
    registry.register(nvim_feedkeys__API_META(), nvim_feedkeys__API_DISPATCH)?;
    registry.register(
        nvim_select_popupmenu_item__API_META(),
        nvim_select_popupmenu_item__API_DISPATCH,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact (name, rgb) pairs `.references/neovim/test/functional/ui/screen.lua`
    /// consumes during `_init_colors` to build `Screen.colors` and the default
    /// attr ids. Keys must keep upstream's mixed-case spelling: Lua indexes
    /// `Screen.colors.Blue1` and friends case-sensitively.
    const BOOTSTRAP_COLORS: &[(&str, u32)] = &[
        ("Blue1", 0x0000_00ff),
        ("LightMagenta", 0x00ff_bbff),
        ("SeaGreen", 0x002e_8b57),
        ("Gray", 0x0080_8080),
        ("DarkBlue", 0x0000_008b),
        ("Brown", 0x00a5_2a2a),
        ("Red", 0x00ff_0000),
        ("Grey100", 0x00ff_ffff),
        ("Yellow", 0x00ff_ff00),
        ("LightGrey", 0x00d3_d3d3),
        ("DarkGray", 0x00a9_a9a9),
        ("SlateBlue", 0x006a_5acd),
        ("Black", 0x0000_0000),
        ("Grey90", 0x00e5_e5e5),
        ("LightBlue", 0x00ad_d8e6),
        ("LightCyan", 0x00e0_ffff),
        ("Cyan4", 0x0000_8b8b),
        ("Fuchsia", 0x00ff_00ff),
        ("Plum1", 0x00ff_bbff),
    ];

    #[expect(
        clippy::panic,
        reason = "helper asserts the color map carries only integer RGB values"
    )]
    fn color_map(dict: &Dict) -> std::collections::HashMap<String, i64> {
        dict.0
            .iter()
            .map(|(name, value)| match value {
                Object::Integer(rgb) => {
                    (String::from_utf8_lossy(name.as_bytes()).into_owned(), *rgb)
                }
                _ => panic!("nvim_get_color_map returned a non-integer value"),
            })
            .collect()
    }

    #[expect(
        clippy::unwrap_used,
        reason = "asserts handlers that never fail return Ok"
    )]
    #[test]
    fn screen_bootstrap_colors_are_exact() {
        let map = color_map(&nvim_get_color_map().unwrap());
        for (name, rgb) in BOOTSTRAP_COLORS {
            assert_eq!(
                map.get(*name).copied(),
                Some(i64::from(*rgb)),
                "map entry {name:?}"
            );
            assert_eq!(
                nvim_get_color_by_name(OxStr::from(*name)).unwrap(),
                i64::from(*rgb),
                "direct lookup {name:?}"
            );
        }
    }

    #[expect(
        clippy::unwrap_used,
        reason = "asserts handlers that never fail return Ok"
    )]
    #[test]
    fn map_and_lookup_agree_for_every_entry() {
        let map = color_map(&nvim_get_color_map().unwrap());
        assert_eq!(
            map.len(),
            COLOR_TABLE.len(),
            "map must carry exactly one entry per table name"
        );
        for (name, rgb) in COLOR_TABLE {
            assert_eq!(
                map.get(*name).copied(),
                Some(i64::from(*rgb)),
                "map entry {name:?}"
            );
            assert_eq!(
                named_color(&OxStr::from(*name)),
                Some(*rgb),
                "lookup {name:?}"
            );
        }
    }

    #[test]
    fn table_names_are_unique_case_insensitively() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in COLOR_TABLE {
            assert!(
                seen.insert(name.to_ascii_lowercase()),
                "duplicate name {name:?}"
            );
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        for (name, rgb) in BOOTSTRAP_COLORS {
            assert_eq!(
                named_color(&OxStr::from(name.to_ascii_uppercase().as_str())),
                Some(*rgb),
                "upper {name:?}"
            );
            assert_eq!(
                named_color(&OxStr::from(name.to_ascii_lowercase().as_str())),
                Some(*rgb),
                "lower {name:?}"
            );
        }
    }

    #[expect(
        clippy::unwrap_used,
        reason = "asserts handlers that never fail return Ok"
    )]
    #[test]
    fn unknown_names_and_invalid_hex_are_rejected() {
        for probe in [
            "",
            "NotAColor",
            " blue",
            "blue ",
            "#ff000",
            "#gg0000",
            "#+12345",
            "##########",
        ] {
            assert_eq!(named_color(&OxStr::from(probe)), None, "probe {probe:?}");
        }
        assert_eq!(
            nvim_get_color_by_name(OxStr::from("NotAColor")).unwrap(),
            -1
        );
        assert_eq!(
            nvim_get_color_by_name(OxStr::from("#ff0000")).unwrap(),
            0x00ff_0000
        );
    }
    #[test]
    fn hl_def_to_dict_uses_short_rgb_keys() {
        let def = HlDef {
            rgb: HlAttrs {
                foreground: Some(0x00ff_0000),
                background: Some(0x0000_ff00),
                special: Some(0x0000_00ff),
                ..HlAttrs::default()
            },
            cterm_fg: Some(1),
            cterm_bg: Some(2),
            ..HlDef::default()
        };
        let dict = hl_def_to_dict(&def, None);
        assert!(
            dict.0
                .iter()
                .any(|(k, v)| *k == OxStr::from("fg") && *v == Object::Integer(0x00ff_0000))
        );
        assert!(
            dict.0
                .iter()
                .any(|(k, v)| *k == OxStr::from("bg") && *v == Object::Integer(0x0000_ff00))
        );
        assert!(
            dict.0
                .iter()
                .any(|(k, v)| *k == OxStr::from("sp") && *v == Object::Integer(0x0000_00ff))
        );
        assert!(!dict.0.iter().any(|(k, _)| *k == OxStr::from("foreground")));
        assert!(!dict.0.iter().any(|(k, _)| *k == OxStr::from("background")));
        assert!(!dict.0.iter().any(|(k, _)| *k == OxStr::from("special")));
        assert!(
            dict.0
                .iter()
                .any(|(k, v)| *k == OxStr::from("ctermfg") && *v == Object::Integer(1))
        );
        assert!(
            dict.0
                .iter()
                .any(|(k, v)| *k == OxStr::from("ctermbg") && *v == Object::Integer(2))
        );
    }
}
