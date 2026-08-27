//! Stable highlight identifiers and protocol emission.

use std::collections::BTreeMap;

use ox_types::{Dict, Object, OxStr};
use thiserror::Error;

/// RGB or terminal highlight attributes.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct HlAttrs {
    /// Foreground color (`0xRRGGBB`) or terminal color index.
    pub foreground: Option<u32>,
    /// Background color (`0xRRGGBB`) or terminal color index.
    pub background: Option<u32>,
    /// Special color (`0xRRGGBB`) or terminal color index.
    pub special: Option<u32>,
    /// Bold text.
    pub bold: bool,
    /// Italic text.
    pub italic: bool,
    /// Underlined text.
    pub underline: bool,
    /// Undercurled text.
    pub undercurl: bool,
    /// Double-underlined text.
    pub underdouble: bool,
    /// Dotted-underlined text.
    pub underdotted: bool,
    /// Dashed-underlined text.
    pub underdashed: bool,
    /// Struck-through text.
    pub strikethrough: bool,
    /// Reverse foreground and background.
    pub reverse: bool,
    /// Alternative font.
    pub altfont: bool,
    /// Faint text.
    pub dim: bool,
    /// Blinking text.
    pub blink: bool,
    /// Concealed text.
    pub conceal: bool,
    /// Overlined text.
    pub overline: bool,
    /// Blend percentage from zero through one hundred.
    pub blend: Option<u8>,
    /// Clickable hyperlink URL.
    pub url: Option<OxStr>,
    /// Foreground is a terminal color index, not an RGB value.
    pub fg_indexed: bool,
}

impl HlAttrs {
    /// Converts attributes to the ordered dictionary used by `hl_attr_define`.
    #[must_use]
    pub fn to_object(&self) -> Object {
        let mut entries = Vec::new();
        push_color(&mut entries, "foreground", self.foreground);
        push_color(&mut entries, "background", self.background);
        push_color(&mut entries, "special", self.special);
        push_flag(&mut entries, "bold", self.bold);
        push_flag(&mut entries, "italic", self.italic);
        push_flag(&mut entries, "underline", self.underline);
        push_flag(&mut entries, "undercurl", self.undercurl);
        push_flag(&mut entries, "underdouble", self.underdouble);
        push_flag(&mut entries, "underdotted", self.underdotted);
        push_flag(&mut entries, "underdashed", self.underdashed);
        push_flag(&mut entries, "strikethrough", self.strikethrough);
        push_flag(&mut entries, "reverse", self.reverse);
        push_flag(&mut entries, "altfont", self.altfont);
        push_flag(&mut entries, "dim", self.dim);
        push_flag(&mut entries, "blink", self.blink);
        push_flag(&mut entries, "conceal", self.conceal);
        push_flag(&mut entries, "overline", self.overline);
        if let Some(blend) = self.blend {
            entries.push((OxStr::from("blend"), Object::Integer(i64::from(blend.min(100)))));
        }
        if let Some(url) = &self.url {
            entries.push((OxStr::from("url"), Object::String(url.clone())));
        }
        Object::Dict(Dict(entries))
    }
}

/// Metadata identifying the source highlight group.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HlInfo {
    /// Kind: `ui`, `syntax`, or `terminal`.
    pub kind: OxStr,
    /// Final highlight group defining the attributes.
    pub hi_name: Option<OxStr>,
    /// Built-in UI group name, for `ui` entries.
    pub ui_name: Option<OxStr>,
    /// Unique numeric source identifier.
    pub id: Option<i64>,
}

impl HlInfo {
    /// Converts metadata to the dictionary carried in the `info` array.
    #[must_use]
    pub fn to_object(&self) -> Object {
        let mut values = vec![(OxStr::from("kind"), Object::String(self.kind.clone()))];
        if let Some(name) = &self.hi_name {
            values.push((OxStr::from("hi_name"), Object::String(name.clone())));
        }
        if let Some(name) = &self.ui_name {
            values.push((OxStr::from("ui_name"), Object::String(name.clone())));
        }
        if let Some(id) = self.id {
            values.push((OxStr::from("id"), Object::Integer(id)));
        }
        Object::Dict(Dict(values))
    }
}

/// One highlight definition and its fallback data.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Highlight {
    /// RGB attributes.
    pub rgb: HlAttrs,
    /// Cterm fallback attributes.
    pub cterm: HlAttrs,
    /// Whether cterm attributes were explicitly supplied (vs inherited from gui).
    pub cterm_explicit: bool,
    /// Whether `default=true` was set (don't override existing definition).
    pub default_flag: bool,
    /// Source metadata entries.
    pub info: Vec<HlInfo>,
}

/// Highlight protocol event.
#[derive(Clone, Debug, PartialEq)]
pub struct HlEvent {
    /// Event name.
    pub name: &'static str,
    /// Event arguments.
    pub args: Vec<Object>,
}

/// Highlight state failures.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HlError {
    /// Attribute identifiers have exhausted the signed protocol range.
    #[error("highlight identifier space exhausted")]
    IdExhausted,
    /// A requested identifier does not exist.
    #[error("unknown highlight id {0}")]
    UnknownId(u64),
}

/// Stable highlight table. Identifier zero is always the default group.
#[derive(Clone, Debug)]
pub struct HlState {
    definitions: Vec<Highlight>,
    ids: BTreeMap<Highlight, u64>,
    groups: BTreeMap<OxStr, u64>,
}

impl Default for HlState {
    fn default() -> Self { Self::new() }
}

impl HlState {
    /// Creates a table containing the default group at identifier zero.
    #[must_use]
    pub fn new() -> Self {
        let default = Highlight::default();
        let mut ids = BTreeMap::new();
        ids.insert(default.clone(), 0);
        Self { definitions: vec![default], ids, groups: BTreeMap::new() }
    }

    /// Creates the render table with the standard syntax groups the editor
    /// establishes while initializing highlighting (`Comment`, `String`).
    #[must_use]
    pub fn with_default_syntax_groups() -> Self {
        let mut state = Self::new();
        let comment = Highlight {
            rgb: HlAttrs { foreground: Some(0x0000ff), ..HlAttrs::default() },
            ..Highlight::default()
        };
        let string = Highlight {
            rgb: HlAttrs { foreground: Some(0x0000ff), bold: true, ..HlAttrs::default() },
            ..Highlight::default()
        };
        let _ = state.define_group("Comment", comment);
        let _ = state.define_group("String", string);
        state
    }

    /// Interns an attribute set, returning its stable id and an event only once.
    pub fn intern(&mut self, highlight: Highlight) -> Result<(u64, Option<HlEvent>), HlError> {
        if let Some(id) = self.ids.get(&highlight) {
            return Ok((*id, None));
        }
        let id = u64::try_from(self.definitions.len()).map_err(|_| HlError::IdExhausted)?;
        i64::try_from(id).map_err(|_| HlError::IdExhausted)?;
        self.definitions.push(highlight.clone());
        self.ids.insert(highlight.clone(), id);
        Ok((id, Some(define_event(id, &highlight))))
    }

    /// Replaces a definition while retaining its identifier.
    pub fn redefine(&mut self, id: u64, highlight: Highlight) -> Result<Option<HlEvent>, HlError> {
        let index = usize::try_from(id).map_err(|_| HlError::UnknownId(id))?;
        let existing = self.definitions.get_mut(index).ok_or(HlError::UnknownId(id))?;
        if *existing == highlight { return Ok(None); }
        if self.ids.get(existing) == Some(&id) {
            self.ids.remove(existing);
        }
        *existing = highlight.clone();
        self.ids.entry(highlight.clone()).or_insert(id);
        Ok(Some(define_event(id, &highlight)))
    }

    /// Associates a named highlight group with an id, emitting only on change.
    pub fn set_group(&mut self, name: impl Into<OxStr>, id: u64) -> Result<Option<HlEvent>, HlError> {
        if usize::try_from(id).ok().is_none_or(|index| index >= self.definitions.len()) {
            return Err(HlError::UnknownId(id));
        }
        let name = name.into();
        if self.groups.get(&name) == Some(&id) { return Ok(None); }
        self.groups.insert(name.clone(), id);
        Ok(Some(HlEvent {
            name: "hl_group_set",
            args: vec![Object::String(name), Object::Integer(i64::try_from(id).map_err(|_| HlError::IdExhausted)?)],
        }))
    }

    /// Returns a definition by id.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Highlight> {
        usize::try_from(id).ok().and_then(|index| self.definitions.get(index))
    }

    /// Iterates definitions in identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &Highlight)> {
        self.definitions
            .iter()
            .enumerate()
            .filter_map(|(id, highlight)| u64::try_from(id).ok().map(|id| (id, highlight)))
    }

    /// Iterates named group bindings in stable name order.
    pub fn groups(&self) -> impl Iterator<Item = (&OxStr, u64)> {
        self.groups.iter().map(|(name, id)| (name, *id))
    }

    /// Emits every current definition, including the default group.
    #[must_use]
    pub fn definitions(&self) -> Vec<HlEvent> {
        self.definitions
            .iter()
            .enumerate()
            .map(|(id, highlight)| define_event(u64::try_from(id).unwrap_or(u64::MAX), highlight))
            .collect()
    }

    /// Defines a named group, interning the highlight and binding the name.
    pub fn define_group(&mut self, name: impl Into<OxStr>, highlight: Highlight) -> Result<u64, HlError> {
        let (id, _) = self.intern(highlight)?;
        self.set_group(name, id)?;
        Ok(id)
    }

    /// Looks up a group id by name.
    #[must_use]
    pub fn group_id(&self, name: &OxStr) -> Option<u64> {
        self.groups.get(name).copied()
    }

    /// Interns the result of stacking the overlay over the base highlight.
    ///
    /// Colors explicitly supplied by the later layer replace earlier colors;
    /// style flags accumulate, matching Neovim's range-highlight composition.
    pub fn combine(
        &mut self,
        base_id: u64,
        overlay_id: u64,
    ) -> Result<(u64, Option<HlEvent>), HlError> {
        let base = self.get(base_id).ok_or(HlError::UnknownId(base_id))?.clone();
        let overlay = self.get(overlay_id).ok_or(HlError::UnknownId(overlay_id))?.clone();
        let mut combined = base;
        combine_attrs(&mut combined.rgb, &overlay.rgb);
        combine_attrs(&mut combined.cterm, &overlay.cterm);
        combined.default_flag |= overlay.default_flag;
        if !overlay.info.is_empty() {
            combined.info = overlay.info;
        }
        self.intern(combined)
    }

    /// Interns the blend-mode composite of `overlay_id` layered over `base_id`.
    pub fn blend(
        &mut self,
        base_id: u64,
        overlay_id: u64,
    ) -> Result<(u64, Option<HlEvent>), HlError> {
        let base = self.get(base_id).ok_or(HlError::UnknownId(base_id))?.clone();
        let overlay = self.get(overlay_id).ok_or(HlError::UnknownId(overlay_id))?.clone();
        let amount = overlay.rgb.blend.unwrap_or(0).min(100);
        let (overlay_fg, base_fg) = (overlay.rgb.foreground, base.rgb.foreground);
        let (overlay_bg, base_bg) = (overlay.rgb.background, base.rgb.background);
        let (overlay_sp, base_sp) = (overlay.rgb.special, base.rgb.special);
        let mut mixed = base;
        combine_attrs(&mut mixed.rgb, &overlay.rgb);
        combine_attrs(&mut mixed.cterm, &overlay.cterm);
        if let (Some(over), Some(under)) = (overlay_fg, base_fg) {
            mixed.rgb.foreground = Some(premix_color(over, under, amount));
        }
        if let (Some(over), Some(under)) = (overlay_bg, base_bg) {
            mixed.rgb.background = Some(premix_color(over, under, amount));
        }
        if let (Some(over), Some(under)) = (overlay_sp, base_sp) {
            mixed.rgb.special = Some(premix_color(over, under, amount));
        }
        mixed.rgb.blend = None;
        mixed.default_flag |= overlay.default_flag;
        if !overlay.info.is_empty() {
            mixed.info = overlay.info;
        }
        self.intern(mixed)
    }

    /// Interns a winblend-premixed variant of `foreground_id` over `background_id`.
    pub fn premix(
        &mut self,
        foreground_id: u64,
        background_id: u64,
        blend: u8,
    ) -> Result<(u64, Option<HlEvent>), HlError> {
        let foreground = self.get(foreground_id).ok_or(HlError::UnknownId(foreground_id))?.clone();
        let background = self.get(background_id).ok_or(HlError::UnknownId(background_id))?.clone();
        let mut mixed = foreground;
        let amount = blend.min(100);
        mixed.rgb.foreground = mix_optional(mixed.rgb.foreground, background.rgb.foreground, amount);
        mixed.rgb.background = mix_optional(mixed.rgb.background, background.rgb.background, amount);
        mixed.rgb.special = mix_optional(mixed.rgb.special, background.rgb.special, amount);
        mixed.rgb.blend = None;
        self.intern(mixed)
    }
}

fn combine_attrs(base: &mut HlAttrs, overlay: &HlAttrs) {
    if overlay.foreground.is_some() { base.foreground = overlay.foreground; }
    if overlay.background.is_some() { base.background = overlay.background; }
    if overlay.special.is_some() { base.special = overlay.special; }
    base.bold |= overlay.bold;
    base.italic |= overlay.italic;
    base.underline |= overlay.underline;
    base.undercurl |= overlay.undercurl;
    base.underdouble |= overlay.underdouble;
    base.underdotted |= overlay.underdotted;
    base.underdashed |= overlay.underdashed;
    base.strikethrough |= overlay.strikethrough;
    base.reverse |= overlay.reverse;
    base.altfont |= overlay.altfont;
    base.dim |= overlay.dim;
    base.blink |= overlay.blink;
    base.conceal |= overlay.conceal;
    base.overline |= overlay.overline;
    if overlay.blend.is_some() { base.blend = overlay.blend; }
    if overlay.url.is_some() { base.url.clone_from(&overlay.url); }
}

/// Premixes a foreground RGB color over a background with Neovim-style percentage rounding.
#[must_use]
pub fn premix_color(foreground: u32, background: u32, blend: u8) -> u32 {
    let blend = u32::from(blend.min(100));
    let opaque = 100 - blend;
    let channel = |shift: u32| {
        let fg = (foreground >> shift) & 0xff;
        let bg = (background >> shift) & 0xff;
        ((fg * opaque + bg * blend + 50) / 100) << shift
    };
    channel(16) | channel(8) | channel(0)
}

fn mix_optional(foreground: Option<u32>, background: Option<u32>, blend: u8) -> Option<u32> {
    match (foreground, background) {
        (Some(foreground), Some(background)) => Some(premix_color(foreground, background, blend)),
        (foreground, _) => foreground,
    }
}

fn define_event(id: u64, highlight: &Highlight) -> HlEvent {
    HlEvent {
        name: "hl_attr_define",
        args: vec![
            Object::Integer(i64::try_from(id).unwrap_or(i64::MAX)),
            highlight.rgb.to_object(),
            highlight.cterm.to_object(),
            Object::Array(highlight.info.iter().map(HlInfo::to_object).collect()),
        ],
    }
}

fn push_color(entries: &mut Vec<(OxStr, Object)>, name: &'static str, color: Option<u32>) {
    if let Some(color) = color {
        entries.push((OxStr::from(name), Object::Integer(i64::from(color))));
    }
}

fn push_flag(entries: &mut Vec<(OxStr, Object)>, name: &'static str, enabled: bool) {
    if enabled { entries.push((OxStr::from(name), Object::Boolean(true))); }
}
