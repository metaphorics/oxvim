//! Client-owned color tokens and colorscheme highlight mapping.
#![allow(missing_docs)]

use std::collections::BTreeMap;

use thiserror::Error;

/// The design-documented contrast of ordinary foreground on the accent surface.
/// This pairing is intentionally forbidden; selected controls use `bg` text instead.
pub const FORBIDDEN_FG_ON_ACCENT_CONTRAST: f64 = 1.91;

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

    /// Quantizes surfaces first, then selects the nearest foreground cube
    /// entries that preserve every contrast contract for that role.
    pub fn quantized(self) -> QuantizedThemeTokens {
        let bg = self.bg.quantize_xterm();
        let float_bg = self.float_bg.quantize_xterm();
        let visual = self.visual.quantize_xterm();
        let all_surfaces = [bg.rgb, float_bg.rgb, visual.rgb];
        let ordinary_surfaces = [bg.rgb, float_bg.rgb];
        let fg = quantize_text(self.fg, &all_surfaces, 4.5);
        let fg_muted = quantize_text(self.fg_muted, &ordinary_surfaces, 4.5);
        let accent = quantize_text(self.accent, &all_surfaces, 4.5);
        let error = quantize_text(self.error, &all_surfaces, 4.5);
        let warn = quantize_text(self.warn, &all_surfaces, 4.5);
        let hint = quantize_text(self.hint, &all_surfaces, 4.5);

        QuantizedThemeTokens {
            bg,
            float_bg,
            visual,
            fg,
            fg_muted,
            accent,
            error,
            warn,
            hint,
        }
    }

    pub fn ansi16(self) -> Ansi16ThemeTokens {
        Ansi16ThemeTokens {
            bg: nearest_ansi16(self.bg),
            float_bg: nearest_ansi16(self.float_bg),
            visual: nearest_ansi16(self.visual),
            fg: nearest_ansi16(self.fg),
            fg_muted: nearest_ansi16(self.fg_muted),
            accent: nearest_ansi16(self.accent),
            error: nearest_ansi16(self.error),
            warn: nearest_ansi16(self.warn),
            hint: nearest_ansi16(self.hint),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantizedColor {
    pub index: u8,
    pub rgb: Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantizedThemeTokens {
    pub bg: QuantizedColor,
    pub float_bg: QuantizedColor,
    pub visual: QuantizedColor,
    pub fg: QuantizedColor,
    pub fg_muted: QuantizedColor,
    pub accent: QuantizedColor,
    pub error: QuantizedColor,
    pub warn: QuantizedColor,
    pub hint: QuantizedColor,
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
pub struct Ansi16ThemeTokens {
    pub bg: Ansi16Color,
    pub float_bg: Ansi16Color,
    pub visual: Ansi16Color,
    pub fg: Ansi16Color,
    pub fg_muted: Ansi16Color,
    pub accent: Ansi16Color,
    pub error: Ansi16Color,
    pub warn: Ansi16Color,
    pub hint: Ansi16Color,
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
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Normal" => Some(Self::Normal),
            "NormalFloat" => Some(Self::NormalFloat),
            "FloatBorder" => Some(Self::FloatBorder),
            "Pmenu" => Some(Self::Pmenu),
            "PmenuSel" => Some(Self::PmenuSel),
            "PmenuKind" => Some(Self::PmenuKind),
            "PmenuExtra" => Some(Self::PmenuExtra),
            "PmenuSbar" => Some(Self::PmenuSbar),
            "PmenuThumb" => Some(Self::PmenuThumb),
            "MsgArea" => Some(Self::MsgArea),
            "MsgSeparator" => Some(Self::MsgSeparator),
            "WildMenu" => Some(Self::WildMenu),
            "ErrorMsg" => Some(Self::ErrorMsg),
            "WarningMsg" => Some(Self::WarningMsg),
            _ => None,
        }
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

fn quantize_text(source: Rgb, backgrounds: &[Rgb], floor: f64) -> QuantizedColor {
    closest_xterm(source, |candidate| {
        backgrounds.iter().all(|background| candidate.contrast(*background) >= floor)
    })
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

fn nearest_ansi16(source: Rgb) -> Ansi16Color {
    let mut best = Ansi16Color::Black;
    let mut best_distance = u32::MAX;
    for index in 0..=15 {
        let candidate = xterm_rgb(index);
        let red = i32::from(source.r) - i32::from(candidate.r);
        let green = i32::from(source.g) - i32::from(candidate.g);
        let blue = i32::from(source.b) - i32::from(candidate.b);
        let distance = (red * red + green * green + blue * blue) as u32;
        if distance < best_distance {
            best = match index {
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
            };
            best_distance = distance;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_text_gate(tokens: ThemeTokens) {
        for foreground in [tokens.fg, tokens.accent, tokens.error, tokens.warn, tokens.hint] {
            for background in [tokens.bg, tokens.float_bg, tokens.visual] {
                assert!(foreground.contrast(background) >= 4.5);
            }
        }
        for background in [tokens.bg, tokens.float_bg] {
            assert!(tokens.fg_muted.contrast(background) >= 4.5);
        }
        assert!(tokens.bg.contrast(tokens.accent) >= 4.5);
        assert!(tokens.accent.contrast(tokens.bg) >= 3.0);
        assert!(tokens.accent.contrast(tokens.float_bg) >= 3.0);
    }

    fn assert_quantized_gate(tokens: QuantizedThemeTokens) {
        for foreground in [tokens.fg, tokens.accent, tokens.error, tokens.warn, tokens.hint] {
            for background in [tokens.bg, tokens.float_bg, tokens.visual] {
                assert!(foreground.rgb.contrast(background.rgb) >= 4.5);
            }
        }
        for background in [tokens.bg, tokens.float_bg] {
            assert!(tokens.fg_muted.rgb.contrast(background.rgb) >= 4.5);
        }
        assert!(tokens.bg.rgb.contrast(tokens.accent.rgb) >= 4.5);
        assert!(tokens.accent.rgb.contrast(tokens.bg.rgb) >= 3.0);
        assert!(tokens.accent.rgb.contrast(tokens.float_bg.rgb) >= 3.0);
    }

    #[test]
    fn exact_design_tokens_and_wcag_gates_hold() {
        assert_eq!(ThemeTokens::DARK.bg, Rgb::new(0x16, 0x18, 0x1d));
        assert_eq!(ThemeTokens::DARK.accent, Rgb::new(0xda, 0x83, 0x4f));
        assert_eq!(ThemeTokens::LIGHT.bg, Rgb::new(0xf3, 0xf5, 0xfb));
        assert_eq!(ThemeTokens::LIGHT.accent, Rgb::new(0x8d, 0x45, 0x12));
        assert_text_gate(ThemeTokens::DARK);
        assert_text_gate(ThemeTokens::LIGHT);
    }

    #[test]
    fn quantized_design_tokens_keep_wcag_gates() {
        assert_quantized_gate(ThemeTokens::DARK.quantized());
        assert_quantized_gate(ThemeTokens::LIGHT.quantized());
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
        for name in [
            "Normal", "NormalFloat", "FloatBorder", "Pmenu", "PmenuSel", "PmenuKind",
            "PmenuExtra", "PmenuSbar", "PmenuThumb", "MsgArea", "MsgSeparator",
            "WildMenu", "ErrorMsg", "WarningMsg",
        ] {
            let group = HighlightGroup::from_name(name).expect("known group");
            let style = theme.style(group);
            assert!(style.foreground.is_some());
            assert!(style.background.is_some());
        }
    }

    #[test]
    fn foreground_on_accent_is_the_documented_forbidden_pair() {
        assert_eq!(FORBIDDEN_FG_ON_ACCENT_CONTRAST, 1.91);
        for tokens in [ThemeTokens::DARK, ThemeTokens::LIGHT] {
            assert!(tokens.fg.contrast(tokens.accent) < 4.5);
        }
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
