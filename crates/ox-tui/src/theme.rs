//! Client-owned color tokens and colorscheme highlight mapping.
#![allow(missing_docs)]

use std::collections::BTreeMap;

use thiserror::Error;

/// The contrast every client-owned text pair must clear.
pub const TEXT_CONTRAST_FLOOR: f64 = 4.5;

/// Measured contrast of ordinary foreground on the accent surface, per variant.
///
/// The design brief quotes a single 1.91:1 for this pair, which reproduces for
/// neither variant: the dark tokens give 1.78:1 and the light tokens 2.05:1.
/// The prohibition stands either way — both are far below
/// [`TEXT_CONTRAST_FLOOR`] — so the numbers here are the measured ones and the
/// quoted aggregate is not used. Selected controls carry `bg`-colored text on
/// accent instead.
pub const FORBIDDEN_FG_ON_ACCENT_DARK: f64 = 1.78;
/// See [`FORBIDDEN_FG_ON_ACCENT_DARK`].
pub const FORBIDDEN_FG_ON_ACCENT_LIGHT: f64 = 2.05;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn relative_luminance(self) -> f64 {
        fn linear(channel: u8) -> f64 {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }

        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    pub fn contrast(self, other: Self) -> f64 {
        let first = self.relative_luminance();
        let second = other.relative_luminance();
        let lighter = first.max(second);
        let darker = first.min(second);
        (lighter + 0.05) / (darker + 0.05)
    }

    /// Returns the closest stable xterm-256 color. Slots 0–15 are deliberately
    /// excluded because applications cannot assume their RGB values.
    pub fn quantize_xterm(self) -> QuantizedColor {
        closest_xterm(self, |_| true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeVariant {
    Dark,
    Light,
}

impl ThemeVariant {
    pub fn from_normal_background(background: Rgb) -> Self {
        if background.relative_luminance() >= 0.5 {
            Self::Light
        } else {
            Self::Dark
        }
    }

    pub fn from_colorfgbg(value: &str) -> Option<Self> {
        let background = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
        if background > 15 {
            return None;
        }
        Some(Self::from_normal_background(xterm_rgb(background)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeTokens {
    pub bg: Rgb,
    pub float_bg: Rgb,
    pub visual: Rgb,
    pub fg: Rgb,
    pub fg_muted: Rgb,
    pub accent: Rgb,
    pub error: Rgb,
    pub warn: Rgb,
    pub hint: Rgb,
}

impl ThemeTokens {
    pub const DARK: Self = Self {
        bg: Rgb::new(0x16, 0x18, 0x1d),
        float_bg: Rgb::new(0x1d, 0x20, 0x26),
        visual: Rgb::new(0x2b, 0x31, 0x40),
        fg: Rgb::new(0xc9, 0xcc, 0xd4),
        fg_muted: Rgb::new(0x87, 0x8c, 0x99),
        accent: Rgb::new(0xda, 0x83, 0x4f),
        error: Rgb::new(0xf2, 0x6f, 0x74),
        warn: Rgb::new(0xd0, 0xa3, 0x5c),
        hint: Rgb::new(0x6f, 0xa8, 0xa3),
    };

    pub const LIGHT: Self = Self {
        bg: Rgb::new(0xf3, 0xf5, 0xfb),
        float_bg: Rgb::new(0xe6, 0xe9, 0xf1),
        visual: Rgb::new(0xc7, 0xcf, 0xe4),
        fg: Rgb::new(0x26, 0x2a, 0x34),
        fg_muted: Rgb::new(0x59, 0x5d, 0x69),
        accent: Rgb::new(0x8d, 0x45, 0x12),
        error: Rgb::new(0xa3, 0x2d, 0x39),
        warn: Rgb::new(0x78, 0x51, 0x00),
        hint: Rgb::new(0x2c, 0x61, 0x5e),
    };

    pub const fn for_variant(variant: ThemeVariant) -> Self {
        match variant {
            ThemeVariant::Dark => Self::DARK,
            ThemeVariant::Light => Self::LIGHT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantizedColor {
    pub index: u8,
    pub rgb: Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Ansi16Color {
    Black = 0,
    Red = 1,
    Green = 2,
    Yellow = 3,
    Blue = 4,
    Magenta = 5,
    Cyan = 6,
    White = 7,
    BrightBlack = 8,
    BrightRed = 9,
    BrightGreen = 10,
    BrightYellow = 11,
    BrightBlue = 12,
    BrightMagenta = 13,
    BrightCyan = 14,
    BrightWhite = 15,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonoStyle {
    pub reverse: bool,
    pub bold: bool,
    pub underline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonoTheme {
    pub normal: MonoStyle,
    pub subdued: MonoStyle,
    pub selected: MonoStyle,
    pub border: MonoStyle,
    pub error: MonoStyle,
}

impl Default for MonoTheme {
    fn default() -> Self {
        Self {
            normal: MonoStyle { reverse: false, bold: false, underline: false },
            subdued: MonoStyle { reverse: false, bold: false, underline: true },
            selected: MonoStyle { reverse: true, bold: true, underline: false },
            border: MonoStyle { reverse: false, bold: true, underline: false },
            error: MonoStyle { reverse: false, bold: true, underline: true },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HighlightStyle {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub special: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub reverse: bool,
}

impl HighlightStyle {
    pub const fn colors(foreground: Rgb, background: Rgb) -> Self {
        Self {
            foreground: Some(foreground),
            background: Some(background),
            special: None,
            bold: false,
            italic: false,
            underline: false,
            undercurl: false,
            reverse: false,
        }
    }

    fn with_fallback(self, fallback: Self) -> Self {
        Self {
            foreground: self.foreground.or(fallback.foreground),
            background: self.background.or(fallback.background),
            special: self.special.or(fallback.special),
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            undercurl: self.undercurl,
            reverse: self.reverse,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HighlightGroup {
    Normal,
    NormalFloat,
    FloatBorder,
    Pmenu,
    PmenuSel,
    PmenuKind,
    PmenuExtra,
    PmenuSbar,
    PmenuThumb,
    MsgArea,
    MsgSeparator,
    WildMenu,
    ErrorMsg,
    WarningMsg,
}

impl HighlightGroup {
    /// Every group this client maps and can paint.
    ///
    /// [`Self::from_name`] is derived from this list, so a variant left out of
    /// it is unreachable from the server's `hl_attr_define` metadata; the
    /// contrast audit iterates it, so a variant left out is also unaudited.
    /// `pinned_group_count` fails when the list and the enum drift.
    pub const ALL: [Self; 14] = [
        Self::Normal,
        Self::NormalFloat,
        Self::FloatBorder,
        Self::Pmenu,
        Self::PmenuSel,
        Self::PmenuKind,
        Self::PmenuExtra,
        Self::PmenuSbar,
        Self::PmenuThumb,
        Self::MsgArea,
        Self::MsgSeparator,
        Self::WildMenu,
        Self::ErrorMsg,
        Self::WarningMsg,
    ];

    /// The Vim highlight-group name, as it arrives in `ui_name`/`hi_name`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::NormalFloat => "NormalFloat",
            Self::FloatBorder => "FloatBorder",
            Self::Pmenu => "Pmenu",
            Self::PmenuSel => "PmenuSel",
            Self::PmenuKind => "PmenuKind",
            Self::PmenuExtra => "PmenuExtra",
            Self::PmenuSbar => "PmenuSbar",
            Self::PmenuThumb => "PmenuThumb",
            Self::MsgArea => "MsgArea",
            Self::MsgSeparator => "MsgSeparator",
            Self::WildMenu => "WildMenu",
            Self::ErrorMsg => "ErrorMsg",
            Self::WarningMsg => "WarningMsg",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|group| group.name() == name)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ThemeError {
    #[error("ordinary foreground on the accent background is forbidden; use background-colored text")]
    ForbiddenForegroundOnAccent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    variant: ThemeVariant,
    tokens: ThemeTokens,
    mapped: BTreeMap<HighlightGroup, HighlightStyle>,
    generation: u64,
    received_highlights: bool,
}

impl Theme {
    pub fn new(colorfgbg: Option<&str>) -> Self {
        let variant = colorfgbg
            .and_then(ThemeVariant::from_colorfgbg)
            .unwrap_or(ThemeVariant::Dark);
        Self {
            variant,
            tokens: ThemeTokens::for_variant(variant),
            mapped: BTreeMap::new(),
            generation: 0,
            received_highlights: false,
        }
    }

    pub const fn variant(&self) -> ThemeVariant {
        self.variant
    }

    pub const fn tokens(&self) -> ThemeTokens {
        self.tokens
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn received_highlights(&self) -> bool {
        self.received_highlights
    }

    /// Replaces the complete named-highlight snapshot in one operation. The
    /// caller should render only after this method returns, so no mixed frame is
    /// observable during a colorscheme change.
    pub fn reswap<I>(&mut self, definitions: I)
    where
        I: IntoIterator<Item = (HighlightGroup, HighlightStyle)>,
    {
        let next: BTreeMap<_, _> = definitions.into_iter().collect();
        let variant = next
            .get(&HighlightGroup::Normal)
            .and_then(|normal| normal.background)
            .map(ThemeVariant::from_normal_background)
            .unwrap_or(self.variant);
        self.variant = variant;
        self.tokens = ThemeTokens::for_variant(variant);
        self.mapped = next;
        self.generation = self.generation.wrapping_add(1);
        self.received_highlights = true;
    }

    pub fn style(&self, group: HighlightGroup) -> HighlightStyle {
        let fallback = fallback_style(self.tokens, group);
        self.mapped
            .get(&group)
            .copied()
            .unwrap_or_default()
            .with_fallback(fallback)
    }

    pub fn validate_client_style(&self, style: HighlightStyle) -> Result<(), ThemeError> {
        if style.foreground == Some(self.tokens.fg) && style.background == Some(self.tokens.accent) {
            Err(ThemeError::ForbiddenForegroundOnAccent)
        } else {
            Ok(())
        }
    }
}

fn fallback_style(tokens: ThemeTokens, group: HighlightGroup) -> HighlightStyle {
    match group {
        HighlightGroup::Normal => HighlightStyle::colors(tokens.fg, tokens.bg),
        HighlightGroup::NormalFloat => HighlightStyle::colors(tokens.fg, tokens.float_bg),
        HighlightGroup::FloatBorder | HighlightGroup::MsgSeparator => {
            HighlightStyle::colors(tokens.accent, tokens.float_bg)
        }
        HighlightGroup::Pmenu | HighlightGroup::MsgArea => {
            HighlightStyle::colors(tokens.fg, tokens.float_bg)
        }
        HighlightGroup::PmenuSel | HighlightGroup::WildMenu => {
            HighlightStyle::colors(tokens.bg, tokens.accent)
        }
        HighlightGroup::PmenuKind | HighlightGroup::PmenuExtra => {
            HighlightStyle::colors(tokens.fg_muted, tokens.float_bg)
        }
        HighlightGroup::PmenuSbar => HighlightStyle::colors(tokens.visual, tokens.float_bg),
        HighlightGroup::PmenuThumb => HighlightStyle::colors(tokens.accent, tokens.visual),
        HighlightGroup::ErrorMsg => HighlightStyle::colors(tokens.error, tokens.float_bg),
        HighlightGroup::WarningMsg => HighlightStyle::colors(tokens.warn, tokens.float_bg),
    }
}

/// The nearest xterm-256 entry to `source` that still clears `floor` against
/// every background in `backgrounds`.
///
/// Quantizing a foreground and its background independently can drop a pair
/// below the design system's floor, so client-owned chrome resolves its text
/// color through here rather than by nearest-color alone.
#[must_use]
pub fn quantize_text(source: Rgb, backgrounds: &[Rgb], floor: f64) -> QuantizedColor {
    closest_xterm(source, |candidate| {
        backgrounds.iter().all(|background| candidate.contrast(*background) >= floor)
    })
}

/// The nearest of the sixteen ANSI colors to `source`.
#[must_use]
pub fn nearest_ansi16(source: Rgb) -> Ansi16Color {
    ansi16_from_index(nearest_ansi16_index(source, &|_| true))
}

/// The nearest ANSI color to `source` that clears `floor` against
/// `background`, or the nearest one outright when the sixteen colors offer
/// nothing that does.
#[must_use]
pub fn quantize_ansi16_text(source: Rgb, background: Ansi16Color, floor: f64) -> Ansi16Color {
    let surface = xterm_rgb(background as u8);
    let index = nearest_ansi16_index(source, &|candidate| candidate.contrast(surface) >= floor);
    ansi16_from_index(index)
}

fn closest_xterm(mut source: Rgb, predicate: impl Fn(Rgb) -> bool) -> QuantizedColor {
    let mut best = QuantizedColor { index: 16, rgb: xterm_rgb(16) };
    let mut best_distance = u32::MAX;
    for index in 16..=255 {
        let candidate = xterm_rgb(index);
        if !predicate(candidate) {
            continue;
        }
        let red = i32::from(source.r) - i32::from(candidate.r);
        let green = i32::from(source.g) - i32::from(candidate.g);
        let blue = i32::from(source.b) - i32::from(candidate.b);
        let distance = (red * red + green * green + blue * blue) as u32;
        if distance < best_distance {
            best = QuantizedColor { index, rgb: candidate };
            best_distance = distance;
        }
    }
    if best_distance == u32::MAX {
        source = if source.relative_luminance() >= 0.5 {
            Rgb::new(0xff, 0xff, 0xff)
        } else {
            Rgb::new(0x00, 0x00, 0x00)
        };
        best = source.quantize_xterm();
    }
    best
}

pub fn xterm_rgb(index: u8) -> Rgb {
    const ANSI: [Rgb; 16] = [
        Rgb::new(0x00, 0x00, 0x00), Rgb::new(0x80, 0x00, 0x00),
        Rgb::new(0x00, 0x80, 0x00), Rgb::new(0x80, 0x80, 0x00),
        Rgb::new(0x00, 0x00, 0x80), Rgb::new(0x80, 0x00, 0x80),
        Rgb::new(0x00, 0x80, 0x80), Rgb::new(0xc0, 0xc0, 0xc0),
        Rgb::new(0x80, 0x80, 0x80), Rgb::new(0xff, 0x00, 0x00),
        Rgb::new(0x00, 0xff, 0x00), Rgb::new(0xff, 0xff, 0x00),
        Rgb::new(0x00, 0x00, 0xff), Rgb::new(0xff, 0x00, 0xff),
        Rgb::new(0x00, 0xff, 0xff), Rgb::new(0xff, 0xff, 0xff),
    ];
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let offset = index - 16;
            Rgb::new(
                LEVELS[usize::from(offset / 36)],
                LEVELS[usize::from((offset % 36) / 6)],
                LEVELS[usize::from(offset % 6)],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            Rgb::new(level, level, level)
        }
    }
}

/// The index of the ANSI color nearest `source` among those satisfying
/// `predicate`, falling back to the nearest of all sixteen when none do.
fn nearest_ansi16_index(source: Rgb, predicate: &dyn Fn(Rgb) -> bool) -> u8 {
    let mut best = None;
    let mut best_distance = u32::MAX;
    for index in 0..=15u8 {
        let candidate = xterm_rgb(index);
        if !predicate(candidate) {
            continue;
        }
        let red = i32::from(source.r) - i32::from(candidate.r);
        let green = i32::from(source.g) - i32::from(candidate.g);
        let blue = i32::from(source.b) - i32::from(candidate.b);
        let distance = (red * red + green * green + blue * blue) as u32;
        if distance < best_distance {
            best = Some(index);
            best_distance = distance;
        }
    }
    match best {
        Some(index) => index,
        None => nearest_ansi16_index(source, &|_| true),
    }
}

const fn ansi16_from_index(index: u8) -> Ansi16Color {
    match index {
        0 => Ansi16Color::Black,
        1 => Ansi16Color::Red,
        2 => Ansi16Color::Green,
        3 => Ansi16Color::Yellow,
        4 => Ansi16Color::Blue,
        5 => Ansi16Color::Magenta,
        6 => Ansi16Color::Cyan,
        7 => Ansi16Color::White,
        8 => Ansi16Color::BrightBlack,
        9 => Ansi16Color::BrightRed,
        10 => Ansi16Color::BrightGreen,
        11 => Ansi16Color::BrightYellow,
        12 => Ansi16Color::BrightBlue,
        13 => Ansi16Color::BrightMagenta,
        14 => Ansi16Color::BrightCyan,
        _ => Ansi16Color::BrightWhite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The groups whose fallback pair the renderer paints as text. Every one
    /// of these must clear [`TEXT_CONTRAST_FLOOR`] at every color depth.
    const TEXT_GROUPS: [HighlightGroup; 12] = [
        HighlightGroup::Normal,
        HighlightGroup::NormalFloat,
        HighlightGroup::FloatBorder,
        HighlightGroup::Pmenu,
        HighlightGroup::PmenuSel,
        HighlightGroup::PmenuKind,
        HighlightGroup::PmenuExtra,
        HighlightGroup::MsgArea,
        HighlightGroup::MsgSeparator,
        HighlightGroup::WildMenu,
        HighlightGroup::ErrorMsg,
        HighlightGroup::WarningMsg,
    ];

    /// `PmenuSbar` and `PmenuThumb` describe a completion scrollbar this
    /// client does not draw. They exist only so a colorscheme's definitions
    /// for them are retained rather than discarded, so they ship no text pair
    /// and are audited at no threshold. Their fallbacks would not clear one:
    /// `visual` on `float_bg` is 1.26:1 dark and 1.28:1 light.
    const SURFACE_ONLY_GROUPS: [HighlightGroup; 2] =
        [HighlightGroup::PmenuSbar, HighlightGroup::PmenuThumb];

    /// The audit is only as complete as its partition, so the partition is
    /// checked against the enum rather than trusted.
    #[test]
    fn every_group_is_classified_as_text_or_surface_exactly_once() {
        assert_eq!(
            TEXT_GROUPS.len() + SURFACE_ONLY_GROUPS.len(),
            HighlightGroup::ALL.len(),
            "a new highlight group must join TEXT_GROUPS or SURFACE_ONLY_GROUPS"
        );
        for group in HighlightGroup::ALL {
            let text = TEXT_GROUPS.contains(&group);
            let surface = SURFACE_ONLY_GROUPS.contains(&group);
            assert!(text != surface, "{} is classified {text}/{surface}", group.name());
        }
    }

    #[test]
    fn pinned_group_count_matches_the_all_list() {
        // Bump deliberately: a group added to the enum without joining ALL is
        // unreachable from hl_attr_define and unaudited.
        assert_eq!(HighlightGroup::ALL.len(), 14);
        for group in HighlightGroup::ALL {
            assert_eq!(HighlightGroup::from_name(group.name()), Some(group));
        }
    }

    #[test]
    fn exact_design_tokens_hold() {
        assert_eq!(ThemeTokens::DARK.bg, Rgb::new(0x16, 0x18, 0x1d));
        assert_eq!(ThemeTokens::DARK.float_bg, Rgb::new(0x1d, 0x20, 0x26));
        assert_eq!(ThemeTokens::DARK.visual, Rgb::new(0x2b, 0x31, 0x40));
        assert_eq!(ThemeTokens::DARK.fg_muted, Rgb::new(0x87, 0x8c, 0x99));
        assert_eq!(ThemeTokens::DARK.accent, Rgb::new(0xda, 0x83, 0x4f));
        assert_eq!(ThemeTokens::DARK.error, Rgb::new(0xf2, 0x6f, 0x74));
        assert_eq!(ThemeTokens::LIGHT.bg, Rgb::new(0xf3, 0xf5, 0xfb));
        assert_eq!(ThemeTokens::LIGHT.float_bg, Rgb::new(0xe6, 0xe9, 0xf1));
        assert_eq!(ThemeTokens::LIGHT.visual, Rgb::new(0xc7, 0xcf, 0xe4));
        assert_eq!(ThemeTokens::LIGHT.fg_muted, Rgb::new(0x59, 0x5d, 0x69));
        assert_eq!(ThemeTokens::LIGHT.accent, Rgb::new(0x8d, 0x45, 0x12));
    }

    /// Every text pair the client paints, in both variants, measured rather
    /// than trusted. The truecolor pair is the design token pair; the
    /// xterm-256 and sixteen-color pairs are what the renderer's quantizers
    /// actually emit, so a quantizer that drops a pair below the floor fails
    /// here instead of shipping.
    #[test]
    fn every_painted_text_pair_clears_the_contrast_floor() {
        for variant in [ThemeVariant::Dark, ThemeVariant::Light] {
            let theme = ThemeTokens::for_variant(variant);
            for group in TEXT_GROUPS {
                let style = fallback_style(theme, group);
                let (Some(foreground), Some(background)) = (style.foreground, style.background)
                else {
                    panic!("{} has no fallback pair", group.name());
                };
                let direct = foreground.contrast(background);
                assert!(
                    direct >= TEXT_CONTRAST_FLOOR,
                    "{variant:?} {} truecolor {direct:.2}",
                    group.name()
                );

                let surface = background.quantize_xterm();
                let text = quantize_text(foreground, &[surface.rgb], TEXT_CONTRAST_FLOOR);
                let quantized = text.rgb.contrast(surface.rgb);
                assert!(
                    quantized >= TEXT_CONTRAST_FLOOR,
                    "{variant:?} {} xterm256 {quantized:.2}",
                    group.name()
                );

                let surface = nearest_ansi16(background);
                let text = quantize_ansi16_text(foreground, surface, TEXT_CONTRAST_FLOOR);
                let ansi = xterm_rgb(text as u8).contrast(xterm_rgb(surface as u8));
                assert!(
                    ansi >= TEXT_CONTRAST_FLOOR,
                    "{variant:?} {} ansi16 {ansi:.2}",
                    group.name()
                );
            }
        }
    }

    /// The floor is only worth having if nearest-color selection would in fact
    /// break a pair the client ships. It does, on a sixteen-color terminal:
    /// FloatBorder's accent lands on yellow beside bright white. If this ever
    /// stops failing, the guard above has become vacuous.
    #[test]
    fn nearest_color_selection_would_break_a_pair_the_floor_saves() {
        let light = ThemeTokens::LIGHT;
        let surface = nearest_ansi16(light.float_bg);
        let nearest = xterm_rgb(nearest_ansi16(light.accent) as u8)
            .contrast(xterm_rgb(surface as u8));
        assert!(nearest < TEXT_CONTRAST_FLOOR, "nearest-color accent-on-float is {nearest:.2}");

        let floored = xterm_rgb(quantize_ansi16_text(light.accent, surface, TEXT_CONTRAST_FLOOR) as u8)
            .contrast(xterm_rgb(surface as u8));
        assert!(floored >= TEXT_CONTRAST_FLOOR, "floored accent-on-float is {floored:.2}");
    }

    #[test]
    fn colorfgbg_uses_only_a_valid_final_palette_entry() {
        assert_eq!(ThemeVariant::from_colorfgbg("15;0"), Some(ThemeVariant::Dark));
        assert_eq!(ThemeVariant::from_colorfgbg("0;15"), Some(ThemeVariant::Light));
        assert_eq!(ThemeVariant::from_colorfgbg(" 0 ; 7 "), Some(ThemeVariant::Light));
        assert_eq!(ThemeVariant::from_colorfgbg("0;256"), None);
        assert_eq!(ThemeVariant::from_colorfgbg("light"), None);
    }

    #[test]
    fn normal_background_supersedes_environment_on_atomic_reswap() {
        let mut theme = Theme::new(Some("0;15"));
        assert_eq!(theme.variant(), ThemeVariant::Light);
        theme.reswap([
            (
                HighlightGroup::Normal,
                HighlightStyle { background: Some(ThemeTokens::DARK.bg), ..HighlightStyle::default() },
            ),
            (
                HighlightGroup::Pmenu,
                HighlightStyle { foreground: Some(Rgb::new(1, 2, 3)), ..HighlightStyle::default() },
            ),
        ]);
        assert_eq!(theme.variant(), ThemeVariant::Dark);
        assert_eq!(theme.generation(), 1);
        assert_eq!(theme.style(HighlightGroup::Pmenu).foreground, Some(Rgb::new(1, 2, 3)));
        assert_eq!(theme.style(HighlightGroup::Pmenu).background, Some(ThemeTokens::DARK.float_bg));
    }

    #[test]
    fn every_client_group_has_a_fallback() {
        let theme = Theme::new(None);
        for group in HighlightGroup::ALL {
            let style = theme.style(group);
            assert!(style.foreground.is_some(), "{} foreground", group.name());
            assert!(style.background.is_some(), "{} background", group.name());
        }
    }

    #[test]
    fn foreground_on_accent_is_forbidden_at_the_measured_ratio() {
        // The brief quotes one aggregate, 1.91:1, which reproduces for neither
        // variant. Each variant is asserted at its own measured value, so a
        // palette change that moved either one fails here.
        for (tokens, documented) in [
            (ThemeTokens::DARK, FORBIDDEN_FG_ON_ACCENT_DARK),
            (ThemeTokens::LIGHT, FORBIDDEN_FG_ON_ACCENT_LIGHT),
        ] {
            let measured = tokens.fg.contrast(tokens.accent);
            assert!(
                (measured - documented).abs() < 0.005,
                "measured {measured:.4}, documented {documented}"
            );
            assert!(measured < TEXT_CONTRAST_FLOOR);
        }
        // The replacement pairing, with the ratios the brief names.
        assert!((ThemeTokens::DARK.bg.contrast(ThemeTokens::DARK.accent) - 6.21).abs() < 0.005);
        assert!((ThemeTokens::LIGHT.bg.contrast(ThemeTokens::LIGHT.accent) - 6.44).abs() < 0.005);

        let dark = Theme::new(None);
        assert!(dark
            .validate_client_style(HighlightStyle::colors(ThemeTokens::DARK.fg, ThemeTokens::DARK.accent))
            .is_err());
        let light = Theme::new(Some("0;15"));
        assert!(light
            .validate_client_style(HighlightStyle::colors(ThemeTokens::LIGHT.fg, ThemeTokens::LIGHT.accent))
            .is_err());
        assert_eq!(light.style(HighlightGroup::PmenuSel).foreground, Some(ThemeTokens::LIGHT.bg));
    }
}
