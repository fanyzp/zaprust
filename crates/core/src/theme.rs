//! Theme system: a theme is **generated**, not stored.
//!
//! Every color the UI uses comes from a semantic [`Tokens`] entry,
//! never a literal, so recoloring the app never means branching UI
//! code. The built-in schemes are generated from a single seed
//! color by [`Tokens::dynamic`]: pick a seed and a dark or light
//! scheme and the whole token set is derived from it, so any seed
//! yields a complete, contrast-checked theme. Custom themes remain
//! data: a JSON document in the [`Tokens::from_json`] /
//! [`CustomTheme`] format can still pin or override tokens.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An RGB color (alpha is always opaque for theme tokens).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parses `#RRGGBB`.
    ///
    /// # Errors
    /// Returns [`ThemeError`] when the string is not `#` + 6 hex digits.
    pub fn from_hex(s: &str) -> Result<Self, ThemeError> {
        let hex = s.strip_prefix('#').ok_or_else(|| ThemeError::BadHex(s.to_owned()))?;
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ThemeError::BadHex(s.to_owned()));
        }
        let v = u32::from_str_radix(hex, 16).map_err(|_| ThemeError::BadHex(s.to_owned()))?;
        Ok(Self(((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8))
    }

    /// Formats as `#RRGGBB`.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }
}

/// Errors a theme file can produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    /// A color was not in `#RRGGBB` form.
    BadHex(String),
    /// The theme JSON could not be parsed.
    Json(String),
    /// A required token is missing.
    MissingToken(String),
    /// The token set fails the surface-ladder or contrast
    /// floors the built-in themes are generated under.
    InvalidLadder(String),
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadHex(s) => write!(f, "invalid color {s:?}, expected #RRGGBB"),
            Self::Json(s) => write!(f, "invalid theme JSON: {s}"),
            Self::MissingToken(s) => write!(f, "theme is missing required token {s}"),
            Self::InvalidLadder(s) => write!(f, "theme fails the design floors: {s}"),
        }
    }
}

impl std::error::Error for ThemeError {}

/// Which color scheme (or rule) is active. The concrete colors are
/// generated dynamically from a seed color, see [`Tokens::dynamic`];
/// this enum only picks the scheme family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub enum ThemeId {
    /// Dark scheme.
    #[default]
    Dark,
    /// Light scheme.
    Light,
    /// Follow the operating system (resolves to Dark or Light).
    Auto,
}

/// Deserialization is manual, not derived, for settings-file
/// compatibility: older versions stored one of the retired built-in
/// palette names ("Nord", "Monokai", "Catppuccin", "TokyoNight",
/// "Gruvbox") and those files must keep loading. Every retired or
/// unrecognized name lands on Dark instead of failing the load.
impl<'de> Deserialize<'de> for ThemeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ThemeIdVisitor;

        impl serde::de::Visitor<'_> for ThemeIdVisitor {
            type Value = ThemeId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a theme name")
            }

            fn visit_str<E>(self, v: &str) -> Result<ThemeId, E>
            where
                E: serde::de::Error,
            {
                Ok(match v {
                    "Light" => ThemeId::Light,
                    "Auto" => ThemeId::Auto,
                    _ => ThemeId::Dark,
                })
            }
        }

        deserializer.deserialize_str(ThemeIdVisitor)
    }
}

impl ThemeId {
    /// All selectable themes, in settings order.
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::Auto];

    /// Display label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::Auto => "System / Auto",
        }
    }

    /// Parses a CLI / config value (`dark`, `light`, `auto`,
    /// `system`). The retired built-in palette names (`nord`,
    /// `monokai`, `catppuccin`, `tokyo-night`, `gruvbox`) are
    /// accepted and degrade gracefully to Dark.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "auto" | "system" => Some(Self::Auto),
            "nord" | "monokai" | "catppuccin" | "catppuccin-mocha" | "tokyo-night"
            | "tokyonight" | "tokyo" | "gruvbox" => Some(Self::Dark),
            _ => None,
        }
    }

    /// Resolves `Auto` using the OS-reported dark-mode preference.
    #[must_use]
    pub fn resolve(self, os_dark: bool) -> Self {
        match self {
            Self::Auto => {
                if os_dark {
                    Self::Dark
                } else {
                    Self::Light
                }
            }
            other => other,
        }
    }

    /// Token set for a resolved (non-Auto) theme, generated from
    /// [`DEFAULT_SEED`]. `Auto` falls back to Dark here; the app
    /// resolves it with [`Self::resolve`] first.
    #[must_use]
    pub fn tokens(self) -> Tokens {
        match self {
            Self::Light => Tokens::dynamic(DEFAULT_SEED, false),
            Self::Dark | Self::Auto => Tokens::dynamic(DEFAULT_SEED, true),
        }
    }
}

/// Seed colors offered for the dynamic theme. One seed plus a dark
/// or light scheme generates the entire token set; the names are
/// plain color names of our own.
pub const SEED_PRESETS: [(&str, Rgb); 6] = [
    ("Blue", Rgb(0x0B, 0x57, 0xD0)),
    ("Green", Rgb(0x00, 0xA8, 0x84)),
    ("Purple", Rgb(0x7C, 0x6B, 0xD9)),
    ("Rose", Rgb(0xD9, 0x6B, 0xA0)),
    ("Amber", Rgb(0xD9, 0x93, 0x3B)),
    ("Teal", Rgb(0x2A, 0xA1, 0x98)),
];

/// The seed the built-in Dark / Light schemes are generated from:
/// the Blue preset, the canonical accent of the owner's design
/// system (design-rifan/DESIGN.md: light #0B57D0, dark #A8C7FA).
/// The derivation below pairs it with its lightened dark tint, so
/// the built-in schemes carry the canonical accent values.
pub const DEFAULT_SEED: Rgb = Rgb(0x0B, 0x57, 0xD0);

/// Semantic color tokens. UI code may only use these, never literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Outermost window background.
    pub bg_app: Rgb,
    /// Side panels (list, settings).
    pub bg_panel: Rgb,
    /// Alternate panel surface (cards, composer).
    pub bg_panel_alt: Rgb,
    /// Chat area background (flat).
    pub bg_chat: Rgb,
    /// Primary text.
    pub text_primary: Rgb,
    /// Secondary text (previews, subtitles).
    pub text_secondary: Rgb,
    /// Faint text (timestamps).
    pub text_faint: Rgb,
    /// Brand accent (buttons, links).
    pub accent: Rgb,
    /// Incoming bubble.
    pub bubble_in: Rgb,
    /// Outgoing bubble.
    pub bubble_out: Rgb,
    /// Unread badge background.
    pub badge_bg: Rgb,
    /// Unread badge text.
    pub badge_text: Rgb,
    /// Subtle borders and separators.
    pub border: Rgb,
    /// System chip inside the chat (notices).
    pub system_chip: Rgb,
    /// Legacy doodle line color. Kept because the serialized
    /// custom-theme format carries it, but nothing paints it: the
    /// chat background is flat now.
    pub doodle: Rgb,
    /// Danger / destructive.
    pub danger: Rgb,
    /// Read-tick color (the blue ticks familiar from `WhatsApp`).
    pub tick_read: Rgb,
    /// Text color of system chips (encryption notices).
    pub system_text: Rgb,
    /// Whether the glow treatment (Liquid Glass: translucent
    /// seed-tinted fills, the specular top edge, the deeper
    /// floating shadow) applies to floating surfaces. Not a
    /// color and not part of the 18-token custom-theme format:
    /// generated and parsed themes default it on, and the app
    /// stamps the user's Appearance setting (default off) over
    /// every token set it builds.
    #[serde(default = "default_glow")]
    pub glow: bool,
}

/// The serde default for [`Tokens::glow`] in theme documents
/// written before the field existed: the historical look.
fn default_glow() -> bool {
    true
}

impl Tokens {
    /// Blends this token set toward `other` by `p` (0 = self,
    /// 1 = other), field by field: the theme crossfade's
    /// interpolation. The one non-color field switches at the
    /// midpoint.
    #[must_use]
    pub fn blend(&self, other: &Self, p: f32) -> Self {
        let m = |a: Rgb, b: Rgb| mix(a, b, p);
        Self {
            bg_app: m(self.bg_app, other.bg_app),
            bg_panel: m(self.bg_panel, other.bg_panel),
            bg_panel_alt: m(self.bg_panel_alt, other.bg_panel_alt),
            bg_chat: m(self.bg_chat, other.bg_chat),
            text_primary: m(self.text_primary, other.text_primary),
            text_secondary: m(self.text_secondary, other.text_secondary),
            text_faint: m(self.text_faint, other.text_faint),
            accent: m(self.accent, other.accent),
            bubble_in: m(self.bubble_in, other.bubble_in),
            bubble_out: m(self.bubble_out, other.bubble_out),
            badge_bg: m(self.badge_bg, other.badge_bg),
            badge_text: m(self.badge_text, other.badge_text),
            border: m(self.border, other.border),
            system_chip: m(self.system_chip, other.system_chip),
            doodle: m(self.doodle, other.doodle),
            danger: m(self.danger, other.danger),
            tick_read: m(self.tick_read, other.tick_read),
            system_text: m(self.system_text, other.system_text),
            glow: if p < 0.5 { self.glow } else { other.glow },
        }
    }

    /// Parses a full theme from its JSON document:
    /// `{ "name": "…", "tokens": { "bg_app": "#0B141A", … } }` with all
    /// 18 tokens present. This is the canonical file format for
    /// custom themes; the built-in schemes are generated instead
    /// (see [`Tokens::dynamic`]).
    ///
    /// # Errors
    /// Returns [`ThemeError`] on malformed JSON, a bad color, or a
    /// missing token.
    pub fn from_json(s: &str) -> Result<Self, ThemeError> {
        #[derive(Deserialize)]
        struct ThemeFile {
            #[serde(default)]
            name: String,
            tokens: std::collections::BTreeMap<String, String>,
        }
        let file: ThemeFile =
            serde_json::from_str(s).map_err(|e| ThemeError::Json(e.to_string()))?;
        let _ = file.name;
        let get = |key: &str| -> Result<Rgb, ThemeError> {
            let raw =
                file.tokens.get(key).ok_or_else(|| ThemeError::MissingToken(key.to_owned()))?;
            Rgb::from_hex(raw)
        };
        let tokens = Self {
            bg_app: get("bg_app")?,
            bg_panel: get("bg_panel")?,
            bg_panel_alt: get("bg_panel_alt")?,
            bg_chat: get("bg_chat")?,
            text_primary: get("text_primary")?,
            text_secondary: get("text_secondary")?,
            text_faint: get("text_faint")?,
            accent: get("accent")?,
            bubble_in: get("bubble_in")?,
            bubble_out: get("bubble_out")?,
            badge_bg: get("badge_bg")?,
            badge_text: get("badge_text")?,
            border: get("border")?,
            system_chip: get("system_chip")?,
            doodle: get("doodle")?,
            danger: get("danger")?,
            tick_read: get("tick_read")?,
            system_text: get("system_text")?,
            glow: true,
        };
        tokens.validate_ladder()?;
        Ok(tokens)
    }

    /// The design floors a token set must satisfy, built-in or
    /// custom (P2-29): the surface ladder must be monotonic in
    /// luminance (app background, panel, panel alt, chat
    /// background in one consistent direction), primary text
    /// must reach 4.5:1 on the panel and app backgrounds, and
    /// secondary text 3:1. A custom theme that fails is
    /// rejected whole; the app keeps the current theme.
    fn validate_ladder(&self) -> Result<(), ThemeError> {
        let ladder = [
            relative_luminance(self.bg_app),
            relative_luminance(self.bg_panel),
            relative_luminance(self.bg_panel_alt),
            relative_luminance(self.bg_chat),
        ];
        let ascending = ladder.windows(2).all(|w| w[0] <= w[1] + 0.02);
        let descending = ladder.windows(2).all(|w| w[0] + 0.02 >= w[1]);
        if !ascending && !descending {
            return Err(ThemeError::InvalidLadder("surface ladder is not monotonic".to_owned()));
        }
        if contrast_ratio(self.text_primary, self.bg_panel) < 4.5
            || contrast_ratio(self.text_primary, self.bg_app) < 4.5
        {
            return Err(ThemeError::InvalidLadder("primary text contrast below 4.5:1".to_owned()));
        }
        if contrast_ratio(self.text_secondary, self.bg_panel) < 3.0 {
            return Err(ThemeError::InvalidLadder("secondary text contrast below 3:1".to_owned()));
        }
        Ok(())
    }

    /// The built-in dark scheme: [`DEFAULT_SEED`], generated.
    #[must_use]
    pub fn dark() -> Self {
        Self::dynamic(DEFAULT_SEED, true)
    }

    /// The built-in light scheme: [`DEFAULT_SEED`], generated.
    #[must_use]
    pub fn light() -> Self {
        Self::dynamic(DEFAULT_SEED, false)
    }

    /// Generates a complete token set from one seed color.
    ///
    /// The seed contributes its hue (and, for the accent family, its
    /// saturation); every token is then derived on fixed lightness
    /// ladders in HSL space, so the relationships between surfaces,
    /// text and accent hold for any seed:
    ///
    /// - Surfaces are near-neutral: the seed hue at a whisper of
    ///   saturation (a tenth of the seed's own, capped at 6%), so
    ///   backgrounds and panels read as the calm greys of
    ///   mainstream chat clients and the seed speaks through the
    ///   accent family instead of washing every surface. The
    ///   surface lightness steps are wide enough that adjacent
    ///   layers separate at a glance (the value ladder the eye
    ///   reads first): dark surfaces step 4% to 16% lightness
    ///   (chat darkest, then app, panel, panel alt, on the
    ///   canonical stops of the owner's design system), light
    ///   surfaces step 90% to 99%. One step above panel alt, the
    ///   elevated-card step is a derivation, not a token: see
    ///   [`elevated_step`].
    /// - Text sits at the opposite end of the ladder (84% / 64% /
    ///   57% dark, 10% / 32% / 47% light), which is what keeps body
    ///   text at WCAG AA on every surface, bubbles included.
    /// - The accent keeps the seed hue with its saturation floored
    ///   at 45%: in the dark scheme it is the seed lightened to
    ///   the canonical pairing lightness (0.82), the pattern of
    ///   the owner's design system (light #0B57D0 pairs with dark
    ///   #A8C7FA); in the light scheme it starts at the seed's own
    ///   lightness (capped at 0.45) and steps down when a naturally
    ///   bright hue (greens, ambers) needs the help to stay legible
    ///   on light panels. `bubble_out` is a deep tint of the accent
    ///   (dark) or a pale tint (light). `tick_read` ignores the
    ///   seed: it is a fixed blue (hue 199) in both schemes, the
    ///   read-tick color users recognize.
    /// - `danger` keeps its own semantic hue (red family) at a
    ///   scheme-appropriate lightness. The unread badge follows
    ///   the seed hue at a fixed saturation and lightness, so it
    ///   wears the user's chosen accent like every other
    ///   accent-driven element; the badge text is whichever of
    ///   black or white contrasts better, the same rule as
    ///   [`Tokens::on_accent`].
    ///
    /// Pure and deterministic: the same seed and scheme always
    /// produce the same tokens.
    #[must_use]
    pub fn dynamic(seed: Rgb, dark: bool) -> Tokens {
        /// Semantic hue for danger (red family), in HSL turns.
        const DANGER_HUE: f32 = 0.008;
        /// Fixed hue for read ticks (blue family, 199 degrees),
        /// in HSL turns. Seed-independent on purpose: blue ticks
        /// mean "read" whatever color the user picked.
        const READ_HUE: f32 = 199.0 / 360.0;

        let (h, seed_s, seed_l) = rgb_to_hsl(seed);
        // Surface tint: only a hint of the seed hue in the
        // neutrals (see the scheme notes above).
        let s_surf = (seed_s * 0.10).clamp(0.02, 0.06);
        // Accent saturation: the seed's own, floored so pastel seeds
        // still produce a confident accent, capped so neon seeds
        // stay usable.
        let s_acc = seed_s.clamp(0.45, 0.90);
        let at = |s: f32, l: f32| hsl_to_rgb(h, s, l);

        if dark {
            // The dark surface stops sit on the canonical ladder
            // of the owner's design system (DESIGN.md dark table:
            // bg #131314, surface #1E1F20, surface-2 #282A2C):
            // app 0.077, panel 0.115, panel alt 0.16, with the
            // chat one extra step below the app sheet, an app-only
            // level the canonical three-level nesting does not
            // name.
            let bg_panel = at(s_surf, 0.115);
            let bg_panel_alt = at(s_surf, 0.16);
            // The dark accent is the seed lightened to its
            // canonical pairing lightness (0.82): the pattern of
            // the owner's design system, whose dark accent is the
            // light accent's pale tint (#0B57D0 pairs with
            // #A8C7FA), never the seed at full strength, which
            // read as a dark jewel tone on dark panels. A seed
            // already lighter than the pairing keeps its own
            // lightness. At 0.82 the accent clears the panel
            // contrast floor by a wide margin, so no legibility
            // step-up loop is needed.
            let accent = at(s_acc, seed_l.max(0.82));
            let badge_bg = hsl_to_rgb(h, 0.55, 0.55);
            Tokens {
                bg_app: at(s_surf, 0.077),
                bg_panel,
                bg_panel_alt,
                bg_chat: at(s_surf, 0.04),
                // Soft off-whites, not pure white: full-brightness
                // text on dark surfaces glares in a dim room. The
                // ladder sits at the canonical ink steps (ink
                // #DCD8D1, muted #A3A3A3, faint #929292):
                // 0.84 / 0.64 / 0.57, far above the AA contrast
                // floor on every surface.
                text_primary: at(s_surf, 0.84),
                text_secondary: at(s_surf, 0.64),
                text_faint: at(s_surf, 0.57),
                accent,
                bubble_in: bg_panel_alt,
                bubble_out: hsl_to_rgb(h, seed_s.clamp(0.35, 0.60), 0.15),
                badge_bg,
                badge_text: on_color(badge_bg),
                border: at(s_surf, 0.19),
                system_chip: at(s_surf, 0.20),
                doodle: at(s_surf, 0.15),
                danger: hsl_to_rgb(DANGER_HUE, 0.65, 0.60),
                tick_read: hsl_to_rgb(READ_HUE, 0.80, 0.62),
                system_text: at(s_surf, 0.78),
                glow: true,
            }
        } else {
            let bg_panel = at(s_surf, 0.955);
            let bg_panel_alt = at(s_surf, 0.90);
            // The light accent starts at the seed's own lightness,
            // capped at 0.45, so the canonical seed reproduces its
            // canonical value (#0B57D0) exactly instead of a
            // ladder-shifted neighbour. Legibility guard: bright
            // hues are naturally luminous, so step the lightness
            // down until the accent clears 3.2:1 on the panel (the
            // suite's AA floor for UI colors is 3:1).
            let mut accent_l = seed_l.clamp(0.30, 0.45);
            let mut accent = at(s_acc, accent_l);
            while contrast_ratio(accent, bg_panel) < 3.2 && accent_l > 0.20 {
                accent_l -= 0.02;
                accent = at(s_acc, accent_l);
            }
            let badge_bg = hsl_to_rgb(h, 0.52, 0.40);
            Tokens {
                bg_app: at(s_surf, 0.99),
                bg_panel,
                bg_panel_alt,
                bg_chat: at(s_surf, 0.935),
                text_primary: at(s_surf, 0.10),
                text_secondary: at(s_surf, 0.32),
                text_faint: at(s_surf, 0.47),
                accent,
                bubble_in: bg_panel_alt,
                bubble_out: hsl_to_rgb(h, seed_s.clamp(0.30, 0.55), 0.87),
                badge_bg,
                badge_text: on_color(badge_bg),
                border: at(s_surf, 0.76),
                system_chip: at(s_surf, 0.865),
                doodle: at(s_surf, 0.86),
                danger: hsl_to_rgb(DANGER_HUE, 0.62, 0.44),
                tick_read: hsl_to_rgb(READ_HUE, 0.80, 0.45),
                system_text: at(s_surf, 0.30),
                glow: true,
            }
        }
    }

    /// Applies the user's Saturation / Contrast appearance controls
    /// to a token set. Pure and deterministic.
    ///
    /// - **Saturation** (`saturation`, clamped to `0.0..=1.5`): each
    ///   color is converted to HSL and its saturation is multiplied,
    ///   keeping hue and lightness, greys stay grey, palette identity
    ///   survives.
    /// - **Contrast** (`contrast`, clamped to `0.8..=1.2`): each
    ///   channel is scaled away from (or toward) the theme's anchor,
    ///   the relative luminance of `bg_app`, in RGB space:
    ///   `c' = clamp(anchor + (c - anchor) * contrast)`.
    ///
    /// Saturation is applied first, then contrast. At the defaults
    /// (`1.0`, `1.0`) the transform is the identity. All tokens are
    /// opaque RGB, so there is no alpha to preserve.
    #[must_use]
    pub fn adjust(self, saturation: f32, contrast: f32) -> Tokens {
        if (saturation - 1.0).abs() < f32::EPSILON && (contrast - 1.0).abs() < f32::EPSILON {
            return self;
        }
        let saturation = saturation.clamp(0.0, 1.5);
        let contrast = contrast.clamp(0.8, 1.2);
        // Contrast pivots around the background color itself: the
        // background maps to itself, and every other color's
        // per-channel distance from it is scaled, so raising
        // contrast can never wash the background out or dim text
        // toward it.
        let anchor = self.bg_app;
        let map = |c: Rgb| adjust_color(c, saturation, contrast, anchor);
        Tokens {
            bg_app: map(self.bg_app),
            bg_panel: map(self.bg_panel),
            bg_panel_alt: map(self.bg_panel_alt),
            bg_chat: map(self.bg_chat),
            text_primary: map(self.text_primary),
            text_secondary: map(self.text_secondary),
            text_faint: map(self.text_faint),
            accent: map(self.accent),
            bubble_in: map(self.bubble_in),
            bubble_out: map(self.bubble_out),
            badge_bg: map(self.badge_bg),
            badge_text: map(self.badge_text),
            border: map(self.border),
            system_chip: map(self.system_chip),
            doodle: map(self.doodle),
            danger: map(self.danger),
            tick_read: map(self.tick_read),
            system_text: map(self.system_text),
            glow: self.glow,
        }
    }

    /// Returns the token set with the glow treatment switched
    /// on or off (see [`Tokens::glow`]). Pure.
    #[must_use]
    pub fn with_glow(mut self, glow: bool) -> Self {
        self.glow = glow;
        self
    }
}

/// WCAG relative luminance of an sRGB color (0.0 black – 1.0 white).
#[must_use]
pub(crate) fn relative_luminance(c: Rgb) -> f32 {
    let f = |v: u8| {
        let s = f32::from(v) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.0) + 0.7152 * f(c.1) + 0.0722 * f(c.2)
}

/// WCAG contrast ratio between two colors (1.0 – 21.0).
#[must_use]
pub(crate) fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Linear mix of two colors: `t = 0` is `a`, `t = 1` is `b`.
#[must_use]
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

const BLACK: Rgb = Rgb(0, 0, 0);
const WHITE: Rgb = Rgb(255, 255, 255);

/// The hover step for a filled control: 12% toward white when the
/// fill is dark, 12% toward black when it is light. One documented
/// derivation for every theme, built in or custom.
#[must_use]
pub fn hover_step(fill: Rgb) -> Rgb {
    if relative_luminance(fill) < 0.35 {
        mix(fill, WHITE, 0.12)
    } else {
        mix(fill, BLACK, 0.12)
    }
}

/// The pressed step: the opposite direction from [`hover_step`],
/// same 12% distance.
#[must_use]
pub fn pressed_step(fill: Rgb) -> Rgb {
    if relative_luminance(fill) < 0.35 {
        mix(fill, BLACK, 0.12)
    } else {
        mix(fill, WHITE, 0.12)
    }
}

/// The raised top edge for a solid raised surface (a bubble, the
/// composer, a selected row): the fill stepped a small way toward
/// the scheme's text color on dark fills, toward white on light
/// fills, painted as a 1 px line along the surface's top. It is
/// what gives solid surfaces the lifted read the glass treatment
/// gives floating ones, in the glow-off path too. A derivation
/// like [`hover_step`], not a token, so custom themes get it from
/// their own colors.
#[must_use]
pub fn raised_edge(fill: Rgb, text: Rgb) -> Rgb {
    if relative_luminance(text) > relative_luminance(fill) {
        // Dark scheme: the text color is the light anchor.
        mix(fill, text, 0.128)
    } else {
        // Light scheme: white is the light anchor.
        mix(fill, WHITE, 0.48)
    }
}

/// The elevated-card step: the resting fill for a card that sits
/// on a panel (grouped sections in the info dock, the solid base
/// under floating glass). One value step above the panel fill it
/// sits on, in the ladder's own terms: a small mix toward white
/// on dark fills (where a few RGB points double the luminance),
/// a long mix toward white on light fills (where the card reads
/// as the near-white sheet on the grey panel, the mainstream
/// light-theme card). A derivation like [`hover_step`], not a
/// token, so custom themes get it from their own panel colors.
#[must_use]
pub fn elevated_step(fill: Rgb) -> Rgb {
    if relative_luminance(fill) < 0.35 {
        mix(fill, WHITE, 0.07)
    } else {
        mix(fill, WHITE, 0.45)
    }
}

/// The text color for content sitting on a filled surface:
/// whichever of black or white computes to the higher WCAG
/// contrast against the fill. (For any fill, the better of the two
/// always clears AA: the maximum of the two ratios is at least
/// the square root of the full 21:1 range.)
#[must_use]
pub fn on_color(fill: Rgb) -> Rgb {
    if contrast_ratio(BLACK, fill) >= contrast_ratio(WHITE, fill) {
        BLACK
    } else {
        WHITE
    }
}

impl Tokens {
    /// Text color for content sitting on the accent fill (the
    /// Primary button); see [`on_color`].
    #[must_use]
    pub fn on_accent(&self) -> Rgb {
        on_color(self.accent)
    }

    /// Text color for content on the danger fill; same rule as
    /// [`Tokens::on_accent`].
    #[must_use]
    pub fn on_danger(&self) -> Rgb {
        on_color(self.danger)
    }
}

fn adjust_color(c: Rgb, saturation: f32, contrast: f32, anchor: Rgb) -> Rgb {
    let (h, s, l) = rgb_to_hsl(c);
    let saturated = hsl_to_rgb(h, (s * saturation).clamp(0.0, 1.0), l);
    let scale = |v: u8, pivot: u8| {
        let c01 = f32::from(v) / 255.0;
        let p01 = f32::from(pivot) / 255.0;
        let out = p01 + (c01 - p01) * contrast;
        (out.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    Rgb(scale(saturated.0, anchor.0), scale(saturated.1, anchor.1), scale(saturated.2, anchor.2))
}

fn rgb_to_hsl(c: Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (f32::from(c.0) / 255.0, f32::from(c.1) / 255.0, f32::from(c.2) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = f32::midpoint(max, min);
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < f32::EPSILON {
        let mut h = (g - b) / d;
        if g < b {
            h += 6.0;
        }
        h
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Rgb {
    if s.abs() < f32::EPSILON {
        let v = (l.clamp(0.0, 1.0) * 255.0).round() as u8;
        return Rgb(v, v, v);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let chan = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    Rgb(chan(h + 1.0 / 3.0), chan(h), chan(h - 1.0 / 3.0))
}

impl Tokens {
    /// Whether this is a light theme (decided by app-background luminance).
    /// The app uses it for base visuals and card shadows.
    #[must_use]
    pub fn is_light(&self) -> bool {
        let lum = 0.2126 * f32::from(self.bg_app.0)
            + 0.7152 * f32::from(self.bg_app.1)
            + 0.0722 * f32::from(self.bg_app.2);
        lum > 150.0
    }
}

/// A custom (community) theme file: JSON with a partial `tokens` map.
/// Missing tokens fall back to the `extends` built-in theme, handled by
/// [`CustomTheme::resolve`]. Same key names as the built-in files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomTheme {
    /// Theme display name.
    pub name: String,
    /// Base theme for fallback tokens: "dark" | "light". Retired
    /// built-in names are accepted too and fall back to "dark",
    /// like everywhere else (see [`ThemeId::parse`]).
    #[serde(default = "default_extends")]
    pub extends: String,
    /// Token overrides as `#RRGGBB`, keyed by token name
    /// (e.g. `bg_app`, `accent`, `bubble_out`).
    #[serde(default)]
    pub tokens: std::collections::BTreeMap<String, String>,
}

fn default_extends() -> String {
    "dark".to_owned()
}

impl CustomTheme {
    /// Parses a theme JSON document.
    ///
    /// # Errors
    /// Returns [`ThemeError`] on malformed JSON or a bad color value.
    pub fn from_json(s: &str) -> Result<Self, ThemeError> {
        let theme: Self = serde_json::from_str(s).map_err(|e| ThemeError::Json(e.to_string()))?;
        for value in theme.tokens.values() {
            Rgb::from_hex(value)?;
        }
        Ok(theme)
    }

    /// Resolves to a full token set: overrides applied over the base theme.
    ///
    /// # Errors
    /// Returns [`ThemeError`] for an unknown token name or bad color.
    pub fn resolve(&self) -> Result<Tokens, ThemeError> {
        let mut tokens = ThemeId::parse(&self.extends).unwrap_or_default().tokens();
        for (key, value) in &self.tokens {
            let rgb = Rgb::from_hex(value)?;
            match key.as_str() {
                "bg_app" => tokens.bg_app = rgb,
                "bg_panel" => tokens.bg_panel = rgb,
                "bg_panel_alt" => tokens.bg_panel_alt = rgb,
                "bg_chat" => tokens.bg_chat = rgb,
                "text_primary" => tokens.text_primary = rgb,
                "text_secondary" => tokens.text_secondary = rgb,
                "text_faint" => tokens.text_faint = rgb,
                "accent" => tokens.accent = rgb,
                "bubble_in" => tokens.bubble_in = rgb,
                "bubble_out" => tokens.bubble_out = rgb,
                "badge_bg" => tokens.badge_bg = rgb,
                "badge_text" => tokens.badge_text = rgb,
                "border" => tokens.border = rgb,
                "system_chip" => tokens.system_chip = rgb,
                "doodle" => tokens.doodle = rgb,
                "danger" => tokens.danger = rgb,
                "tick_read" => tokens.tick_read = rgb,
                "system_text" => tokens.system_text = rgb,
                other => return Err(ThemeError::MissingToken(other.to_owned())),
            }
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        for hex in ["#00A884", "#2E3440", "#FFFFFF", "#000000"] {
            assert_eq!(Rgb::from_hex(hex).unwrap().to_hex(), hex);
        }
    }

    #[test]
    fn hex_rejects_garbage() {
        assert!(Rgb::from_hex("00A884").is_err());
        assert!(Rgb::from_hex("#12345").is_err());
        assert!(Rgb::from_hex("#GGGGGG").is_err());
    }

    #[test]
    fn auto_resolves_by_os_preference() {
        assert_eq!(ThemeId::Auto.resolve(true), ThemeId::Dark);
        assert_eq!(ThemeId::Auto.resolve(false), ThemeId::Light);
        assert_eq!(ThemeId::Dark.resolve(false), ThemeId::Dark);
        assert_eq!(ThemeId::Light.resolve(true), ThemeId::Light);
    }

    #[test]
    fn parse_theme_names() {
        assert_eq!(ThemeId::parse("dark"), Some(ThemeId::Dark));
        assert_eq!(ThemeId::parse("SYSTEM"), Some(ThemeId::Auto));
        assert_eq!(ThemeId::parse("sepia"), None);
    }

    #[test]
    fn dynamic_schemes_report_light_correctly() {
        assert!(Tokens::light().is_light());
        assert!(!Tokens::dark().is_light());
        for (_, seed) in SEED_PRESETS {
            assert!(Tokens::dynamic(seed, false).is_light());
            assert!(!Tokens::dynamic(seed, true).is_light());
        }
    }

    #[test]
    fn schemes_and_seeds_have_distinct_backgrounds() {
        assert_ne!(Tokens::dark().bg_app, Tokens::light().bg_app);
        // The seed hue tints the surfaces, so different seeds differ.
        let blue = Tokens::dynamic(SEED_PRESETS[0].1, true);
        let violet = Tokens::dynamic(SEED_PRESETS[2].1, true);
        assert_ne!(blue.bg_app, violet.bg_app);
        assert_ne!(blue.accent, violet.accent);
        // Read ticks are the fixed blue in every seed and scheme:
        // they must not follow the accent.
        for (_, seed) in SEED_PRESETS {
            assert_eq!(Tokens::dynamic(seed, true).tick_read, blue.tick_read);
            assert_eq!(
                Tokens::dynamic(seed, false).tick_read,
                Tokens::dynamic(SEED_PRESETS[0].1, false).tick_read
            );
        }
        assert_ne!(blue.tick_read, blue.accent);
    }

    #[test]
    fn badge_follows_the_seed_hue() {
        // The unread badge wears the user's accent: its hue is
        // the seed's hue in both schemes (the owner's blue seed
        // must produce blue badges, not the old fixed green).
        for (_, seed) in SEED_PRESETS {
            let (seed_hue, _, _) = rgb_to_hsl(seed);
            for dark in [true, false] {
                let tokens = Tokens::dynamic(seed, dark);
                let (badge_hue, _, _) = rgb_to_hsl(tokens.badge_bg);
                let delta = (badge_hue - seed_hue).abs();
                assert!(
                    delta < 0.01 || delta > 0.99,
                    "badge hue {badge_hue} must track seed hue {seed_hue}"
                );
            }
        }
        // And the default seed's badge is the canonical blue
        // family, unchanged for users who never pick an accent.
        let (default_hue, _, _) = rgb_to_hsl(Tokens::dark().badge_bg);
        let (blue_hue, _, _) = rgb_to_hsl(SEED_PRESETS[0].1);
        assert!((default_hue - blue_hue).abs() < 0.01);
    }

    #[test]
    fn custom_theme_overrides_and_falls_back() {
        let theme = CustomTheme::from_json(
            r##"{ "name": "Test", "extends": "dark", "tokens": { "accent": "#FF0000" } }"##,
        )
        .unwrap();
        let tokens = theme.resolve().unwrap();
        assert_eq!(tokens.accent, Rgb(0xFF, 0, 0));
        assert_eq!(tokens.bg_app, Tokens::dark().bg_app, "unset tokens fall back to base");
    }

    #[test]
    fn custom_theme_rejects_unknown_token() {
        let theme = CustomTheme::from_json(
            r##"{ "name": "Test", "tokens": { "bogus_token": "#FF0000" } }"##,
        )
        .unwrap();
        assert!(theme.resolve().is_err());
    }

    #[test]
    fn defaults_come_from_the_default_seed() {
        assert_eq!(Tokens::dark(), Tokens::dynamic(DEFAULT_SEED, true));
        assert_eq!(Tokens::light(), Tokens::dynamic(DEFAULT_SEED, false));
        assert_eq!(ThemeId::Dark.tokens(), Tokens::dark());
        assert_eq!(ThemeId::Light.tokens(), Tokens::light());
        assert_eq!(ThemeId::Auto.tokens(), Tokens::dark(), "Auto falls back to Dark");
        // The default seed is the Blue preset, the canonical accent.
        assert_eq!(DEFAULT_SEED, SEED_PRESETS[0].1);
    }

    #[test]
    fn from_json_still_parses_a_full_theme_file() {
        // The JSON format stays canonical for custom themes even
        // though the built-ins are generated now.
        let tokens = Tokens::from_json(
            r##"{
                "name": "Full",
                "tokens": {
                    "bg_app": "#0B141A", "bg_panel": "#111B21",
                    "bg_panel_alt": "#202C33", "bg_chat": "#070D12",
                    "text_primary": "#E9EDEF", "text_secondary": "#8696A3",
                    "text_faint": "#5F6D77", "accent": "#00A884",
                    "bubble_in": "#1F2C34", "bubble_out": "#005C4B",
                    "badge_bg": "#00A884", "badge_text": "#111B21",
                    "border": "#222D34", "system_chip": "#18232B",
                    "doodle": "#D8DEE9", "danger": "#EA5A5A",
                    "tick_read": "#53BDEB", "system_text": "#FFD279"
                }
            }"##,
        )
        .unwrap();
        assert_eq!(tokens.accent, Rgb(0x00, 0xA8, 0x84));
        assert_eq!(tokens.bg_chat, Rgb(0x07, 0x0D, 0x12));
        // A missing token is still an error.
        assert!(Tokens::from_json(r#"{ "name": "X", "tokens": {} }"#).is_err());
    }

    #[test]
    fn legacy_theme_names_deserialize_to_dark() {
        // Settings files written by older versions carry the retired
        // palette names; every one must load as Dark, not error.
        for name in ["Nord", "Monokai", "Catppuccin", "TokyoNight", "Gruvbox"] {
            let stored = format!("\"{name}\"");
            assert_eq!(
                serde_json::from_str::<ThemeId>(&stored).unwrap(),
                ThemeId::Dark,
                "{name} must degrade to Dark"
            );
        }
    }

    #[test]
    fn theme_id_serialization_round_trips() {
        for (id, stored) in [
            (ThemeId::Dark, "\"Dark\""),
            (ThemeId::Light, "\"Light\""),
            (ThemeId::Auto, "\"Auto\""),
        ] {
            assert_eq!(serde_json::to_string(&id).unwrap(), stored);
            assert_eq!(serde_json::from_str::<ThemeId>(stored).unwrap(), id);
        }
    }

    /// Every generated token set the invariant tests run against:
    /// all seed presets in both schemes, plus the two built-in
    /// schemes (the default seed through `ThemeId`).
    fn all_generated_tokens() -> Vec<(String, Tokens)> {
        let mut out = Vec::new();
        for (name, seed) in SEED_PRESETS {
            out.push((format!("{name} dark"), Tokens::dynamic(seed, true)));
            out.push((format!("{name} light"), Tokens::dynamic(seed, false)));
        }
        out.push(("Default dark".to_owned(), ThemeId::Dark.tokens()));
        out.push(("Default light".to_owned(), ThemeId::Light.tokens()));
        out
    }

    /// Button pairs: the computed on-accent / on-danger text must
    /// reach AA (4.5:1) on every generated theme, and the value
    /// steps behind the surfaces must layer monotonically (dark
    /// schemes rise bg_app < bg_panel < bg_panel_alt; light schemes
    /// invert). Hover/pressed steps must actually move.
    #[test]
    fn button_pairs_and_value_layering_hold_for_all_generated() {
        for (name, t) in all_generated_tokens() {
            let name = &name;
            let t = &t;
            let on_acc = contrast_ratio(t.on_accent(), t.accent);
            assert!(on_acc >= 4.5, "{name}: on_accent on accent is {on_acc:.2}:1");
            let on_dan = contrast_ratio(t.on_danger(), t.danger);
            assert!(on_dan >= 4.5, "{name}: on_danger on danger is {on_dan:.2}:1");
            let (app, panel, alt) = (
                relative_luminance(t.bg_app),
                relative_luminance(t.bg_panel),
                relative_luminance(t.bg_panel_alt),
            );
            if relative_luminance(t.text_primary) > relative_luminance(t.bg_app) {
                assert!(
                    app < panel && panel <= alt + 0.001 && app != alt,
                    "{name}: dark layering broken: {app:.3} {panel:.3} {alt:.3}"
                );
            } else {
                assert!(
                    app > panel || panel > alt,
                    "{name}: light layering broken: {app:.3} {panel:.3} {alt:.3}"
                );
            }
            // The conversation surface and incoming bubbles must
            // sit on their own steps too (a Round 2 review catch:
            // four themes had bg_chat == bg_app or == bg_panel and
            // bubble_in == bg_panel, flattening the chat surface).
            assert_ne!(t.bg_chat, t.bg_app, "{name}: bg_chat must differ from bg_app");
            assert_ne!(t.bg_chat, t.bg_panel, "{name}: bg_chat must differ from bg_panel");
            assert_ne!(t.bubble_in, t.bg_chat, "{name}: bubble_in must differ from bg_chat");
            if relative_luminance(t.text_primary) > relative_luminance(t.bg_app) {
                assert_ne!(t.bubble_in, t.bg_panel, "{name}: bubble_in must differ from bg_panel");
                assert!(
                    relative_luminance(t.bg_chat) < relative_luminance(t.bg_app),
                    "{name}: a dark theme's chat surface is its darkest step"
                );
            }
            assert_ne!(hover_step(t.accent), t.accent, "{name}: hover step must move");
            assert_ne!(pressed_step(t.accent), t.accent, "{name}: pressed step must move");
            assert_ne!(hover_step(t.accent), pressed_step(t.accent), "{name}: steps must differ");
        }
    }

    #[test]
    fn retired_theme_names_degrade_to_dark() {
        for name in
            ["nord", "monokai", "Catppuccin", "tokyo-night", "tokyonight", "tokyo", "gruvbox"]
        {
            assert_eq!(ThemeId::parse(name), Some(ThemeId::Dark), "{name} must degrade");
        }
        assert_eq!(ThemeId::ALL.len(), 3);
        assert_eq!(ThemeId::ALL, [ThemeId::Dark, ThemeId::Light, ThemeId::Auto]);
    }

    /// WCAG AA contract for every generated theme: body text
    /// ≥ 4.5:1 on app/panel/bubble surfaces, secondary text
    /// ≥ 4.5:1, faint text ≥ 3:1 (large/secondary use), badge and
    /// system-chip text ≥ 4.5:1, accent ≥ 3:1 as a UI color.
    #[test]
    fn generated_themes_meet_wcag_aa() {
        for (name, t) in all_generated_tokens() {
            let name = &name;
            let t = &t;
            for (label, fg, bg, min) in [
                ("text_primary on bg_app", t.text_primary, t.bg_app, 4.5),
                ("text_primary on bg_panel", t.text_primary, t.bg_panel, 4.5),
                ("text_primary on bubble_in", t.text_primary, t.bubble_in, 4.5),
                ("text_primary on bubble_out", t.text_primary, t.bubble_out, 4.5),
                ("text_secondary on bg_panel", t.text_secondary, t.bg_panel, 4.5),
                ("text_secondary on bg_app", t.text_secondary, t.bg_app, 4.5),
                ("text_faint on bg_app", t.text_faint, t.bg_app, 3.0),
                ("badge_text on badge_bg", t.badge_text, t.badge_bg, 4.5),
                ("system_text on system_chip", t.system_text, t.system_chip, 4.5),
                ("accent on bg_panel", t.accent, t.bg_panel, 3.0),
            ] {
                let ratio = contrast_ratio(fg, bg);
                assert!(ratio >= min, "{name}: {label} is {ratio:.2}:1, needs {min}:1");
            }
        }
    }

    /// The dark accent must stay in the seed's color family at
    /// the canonical pairing lightness: the same hue (within
    /// rounding), lightened to 0.82 unless the seed is already
    /// lighter, the pattern of the owner's design system (light
    /// #0B57D0 pairs with dark #A8C7FA). This is the regression
    /// pin for the pairing rule, replacing the earlier
    /// seed-lightness pin from when the dark accent was the seed
    /// itself at full strength.
    #[test]
    fn dark_accent_stays_in_the_seed_family() {
        let hue_gap = |a: f32, b: f32| {
            let d = (a - b).abs();
            d.min(1.0 - d)
        };
        for (name, seed) in SEED_PRESETS {
            let (seed_h, _, seed_l) = rgb_to_hsl(seed);
            let t = Tokens::dynamic(seed, true);
            let (h, _, l) = rgb_to_hsl(t.accent);
            assert!(
                hue_gap(h, seed_h) <= 0.03,
                "{name}: dark accent hue {h:.3} drifted from seed hue {seed_h:.3}"
            );
            assert!(
                (l - seed_l.max(0.82)).abs() <= 0.04,
                "{name}: dark accent lightness {l:.2} drifted from the pairing lightness"
            );
            assert!(l >= 0.75, "{name}: dark accent must stay a light tint");
        }
        // And the light accent keeps the seed hue too.
        for (name, seed) in SEED_PRESETS {
            let (seed_h, _, _) = rgb_to_hsl(seed);
            let (h, _, _) = rgb_to_hsl(Tokens::dynamic(seed, false).accent);
            assert!(
                hue_gap(h, seed_h) <= 0.03,
                "{name}: light accent hue {h:.3} drifted from seed hue {seed_h:.3}"
            );
        }
    }

    /// The surface value ladder must separate at a glance: in
    /// dark schemes each resting surface is at least 1.4x the
    /// luminance of the one below it, in light schemes each step
    /// drops at least 0.035 luminance, and the elevated-card
    /// derivation lands strictly above the panel it sits on.
    /// (A flat ladder is what made whole screens read as one
    /// undifferentiated mass; this test is the regression pin.)
    #[test]
    fn surface_ladder_steps_are_clearly_separated() {
        for (name, t) in all_generated_tokens() {
            let dark = relative_luminance(t.text_primary) > relative_luminance(t.bg_app);
            // The ladder in ascending-value order per scheme:
            // dark surfaces rise chat < app < panel < panel alt <
            // chip; light surfaces fall app > panel > chat >
            // panel alt > chip (the app background is the lightest
            // sheet, cards and wells step down from it).
            let ladder = if dark {
                [
                    relative_luminance(t.bg_chat),
                    relative_luminance(t.bg_app),
                    relative_luminance(t.bg_panel),
                    relative_luminance(t.bg_panel_alt),
                    relative_luminance(t.system_chip),
                ]
            } else {
                [
                    relative_luminance(t.system_chip),
                    relative_luminance(t.bg_panel_alt),
                    relative_luminance(t.bg_chat),
                    relative_luminance(t.bg_panel),
                    relative_luminance(t.bg_app),
                ]
            };
            for pair in ladder.windows(2) {
                let (lo, hi) = (pair[0], pair[1]);
                if dark {
                    assert!(
                        hi >= lo * 1.4,
                        "{name}: dark ladder step too flat: {lo:.4} -> {hi:.4}"
                    );
                } else {
                    assert!(
                        hi >= lo + 0.035,
                        "{name}: light ladder step too flat: {lo:.4} -> {hi:.4}"
                    );
                }
            }
            let elevated = relative_luminance(elevated_step(t.bg_panel));
            assert!(
                elevated > relative_luminance(t.bg_panel),
                "{name}: elevated card must sit above the panel"
            );
            assert_ne!(elevated_step(t.bg_panel), t.bg_panel, "{name}: elevated step must move");
        }
    }

    /// The state steps and the raised edge must actually read:
    /// hover and pressed move every surface fill, differ from
    /// each other, and the raised edge moves a surface toward
    /// the light (up in luminance) in both schemes.
    #[test]
    fn state_steps_and_raised_edge_move_visibly() {
        for (name, t) in all_generated_tokens() {
            for fill in [t.accent, t.bg_panel_alt, t.bubble_in, t.bubble_out] {
                assert_ne!(hover_step(fill), fill, "{name}: hover must move {fill:?}");
                assert_ne!(pressed_step(fill), fill, "{name}: pressed must move {fill:?}");
                assert_ne!(
                    hover_step(fill),
                    pressed_step(fill),
                    "{name}: hover and pressed must differ on {fill:?}"
                );
                let edge = raised_edge(fill, t.text_primary);
                assert_ne!(edge, fill, "{name}: raised edge must move {fill:?}");
                assert!(
                    relative_luminance(edge) > relative_luminance(fill),
                    "{name}: raised edge must lighten {fill:?}"
                );
            }
        }
    }

    #[test]
    fn adjust_is_identity_at_defaults() {
        let t = Tokens::dark();
        assert_eq!(t.adjust(1.0, 1.0), t);
    }

    #[test]
    fn adjust_saturation_zero_greys_everything() {
        let t = Tokens::dark().adjust(0.0, 1.0);
        for c in [t.accent, t.bubble_out, t.danger, t.bg_panel] {
            assert_eq!(c.0, c.1, "saturation 0 must equalise channels: {c:?}");
            assert_eq!(c.1, c.2, "saturation 0 must equalise channels: {c:?}");
        }
    }

    #[test]
    fn adjust_saturation_is_monotonic() {
        let spread = |c: Rgb| i32::from(c.0.max(c.1).max(c.2)) - i32::from(c.0.min(c.1).min(c.2));
        let base = Tokens::dynamic(SEED_PRESETS[2].1, true);
        let low = base.adjust(0.5, 1.0).accent;
        let mid = base.adjust(1.0, 1.0).accent;
        let high = base.adjust(1.5, 1.0).accent;
        assert!(spread(low) < spread(mid), "less saturation, less spread");
        assert!(spread(mid) <= spread(high), "more saturation, more spread");
    }

    #[test]
    fn adjust_contrast_pushes_away_from_anchor() {
        let t = Tokens::dark();
        let up = t.adjust(1.0, 1.2);
        // The near-white text gets no darker; the dark background gets
        // no lighter, contrast scales distance from the anchor.
        assert!(relative_luminance(up.text_primary) >= relative_luminance(t.text_primary) - 0.01);
        assert!(relative_luminance(up.bg_app) <= relative_luminance(t.bg_app) + 0.001);
        let down = t.adjust(1.0, 0.8);
        assert!(
            relative_luminance(down.text_primary) <= relative_luminance(t.text_primary) + 0.001
        );
    }

    #[test]
    fn adjust_clamps_out_of_range_inputs() {
        let t = Tokens::light();
        // Must not panic and must equal the clamped transform.
        assert_eq!(t.adjust(99.0, 99.0), t.adjust(1.5, 1.2));
        assert_eq!(t.adjust(-1.0, 0.0), t.adjust(0.0, 0.8));
    }

    #[test]
    fn glow_defaults_on_and_survives_adjust() {
        let t = Tokens::dark();
        assert!(t.glow);
        assert!(!t.with_glow(false).glow);
        assert!(t.adjust(1.2, 1.1).glow);
        assert!(!t.with_glow(false).adjust(1.2, 1.1).glow);
    }

    #[test]
    fn parsed_theme_defaults_glow_on() {
        let json = r##"{"name":"T","tokens":{
            "bg_app":"#0B141A","bg_panel":"#111B21","bg_panel_alt":"#1F2C34",
            "bg_chat":"#0B141A","text_primary":"#E9EDEF","text_secondary":"#8696A0",
            "text_faint":"#667781","accent":"#00A884","bubble_in":"#1F2C34",
            "bubble_out":"#005C4B","badge_bg":"#25D366","badge_text":"#111B21",
            "border":"#222D34","system_chip":"#182229","doodle":"#182229",
            "danger":"#EA0038","tick_read":"#53BDEB","system_text":"#FFD279"}}"##;
        let t = Tokens::from_json(json).expect("valid theme");
        assert!(t.glow);
    }
}
