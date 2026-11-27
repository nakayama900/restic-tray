//! Design system for the GUI: color tokens, typography, egui style, vector icons and widgets.
//!
//! Warm ink and paper neutrals with one lime signal color. All text tokens keep a contrast
//! ratio of at least 4.5:1 against every background token. Icons are drawn with
//! `egui::Painter` (`VectorIcon`); the UI never uses emoji glyphs.

mod icons;
mod widgets;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{
    Color32, Context, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId,
    Margin, Shadow, Stroke, Style, TextStyle, Theme, Vec2, Visuals,
};

pub use icons::{VectorIcon, paint_brand_mark, paint_icon};
pub use widgets::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DesignTokens {
    pub is_dark: bool,
    /// Canvas behind page content.
    pub bg: Color32,
    pub sidebar: Color32,
    /// Inputs, popups and raised rows.
    pub surface: Color32,
    pub surface_hover: Color32,
    /// Hairline dividers.
    pub line: Color32,
    /// Control borders.
    pub line_strong: Color32,
    pub text: Color32,
    pub text_2: Color32,
    pub text_3: Color32,
    /// Signal color as a fill. On paper it is too light for text; use `accent_text` there.
    pub accent: Color32,
    /// Signal color that is safe for text, strokes and charts on every background.
    pub accent_text: Color32,
    pub primary_fill: Color32,
    pub on_primary: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub on_err: Color32,
}

const INK: Color32 = Color32::from_rgb(0x0E, 0x0E, 0x0D);
const LIME: Color32 = Color32::from_rgb(0xD4, 0xF8, 0x4A);
const PAPER: Color32 = Color32::from_rgb(0xF3, 0xF1, 0xEC);

impl DesignTokens {
    pub fn dark() -> Self {
        Self {
            is_dark: true,
            bg: INK,
            sidebar: Color32::from_rgb(0x12, 0x12, 0x11),
            surface: Color32::from_rgb(0x18, 0x18, 0x17),
            surface_hover: Color32::from_rgb(0x20, 0x20, 0x1E),
            line: Color32::from_rgb(0x26, 0x26, 0x23),
            line_strong: Color32::from_rgb(0x3D, 0x3C, 0x38),
            text: Color32::from_rgb(0xED, 0xEB, 0xE5),
            text_2: Color32::from_rgb(0xA8, 0xA5, 0x9D),
            text_3: Color32::from_rgb(0x8A, 0x87, 0x7F),
            accent: LIME,
            accent_text: LIME,
            primary_fill: LIME,
            on_primary: INK,
            warn: Color32::from_rgb(0xF2, 0xB5, 0x44),
            err: Color32::from_rgb(0xFF, 0x6F, 0x61),
            on_err: INK,
        }
    }

    pub fn light() -> Self {
        Self {
            is_dark: false,
            bg: PAPER,
            sidebar: Color32::from_rgb(0xEC, 0xE9, 0xE3),
            surface: Color32::from_rgb(0xFB, 0xFA, 0xF7),
            surface_hover: Color32::from_rgb(0xE9, 0xE6, 0xDF),
            line: Color32::from_rgb(0xDD, 0xD9, 0xD0),
            line_strong: Color32::from_rgb(0xC2, 0xBD, 0xB2),
            text: Color32::from_rgb(0x15, 0x15, 0x13),
            text_2: Color32::from_rgb(0x4E, 0x4C, 0x46),
            text_3: Color32::from_rgb(0x67, 0x64, 0x5D),
            accent: LIME,
            accent_text: Color32::from_rgb(0x4C, 0x66, 0x00),
            primary_fill: Color32::from_rgb(0x15, 0x15, 0x13),
            on_primary: LIME,
            warn: Color32::from_rgb(0x8F, 0x54, 0x00),
            err: Color32::from_rgb(0xBF, 0x33, 0x24),
            on_err: PAPER,
        }
    }

    pub fn for_theme(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// Healthy / successful state. Same hue as the signal so "protected" reads as the brand.
    pub fn ok(&self) -> Color32 {
        self.accent_text
    }

    pub fn apply(&self, ctx: &Context) {
        let theme = if self.is_dark {
            Theme::Dark
        } else {
            Theme::Light
        };
        // Pin the theme so egui does not swap in its default style when the OS theme changes.
        ctx.set_theme(theme);
        ctx.set_style_of(theme, self.style());
        // Match the native title bar (DWM immersive dark mode) to the app theme.
        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(if self.is_dark {
            egui::SystemTheme::Dark
        } else {
            egui::SystemTheme::Light
        }));
    }

    fn style(&self) -> Style {
        let mut v = if self.is_dark {
            Visuals::dark()
        } else {
            Visuals::light()
        };
        v.panel_fill = self.bg;
        // egui's light theme writes glyph coverage to the atlas as is, which draws this app's
        // text visibly thinner than the dark theme does. Use the dark theme's curve in both.
        v.text_options.color_transfer_function =
            egui::epaint::FontColorTransferFunction::TwoCoverageMinusCoverageSq;
        v.window_fill = self.surface;
        v.window_stroke = Stroke::new(1.0_f32, self.line_strong);
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_shadow = Shadow {
            offset: [0, 16],
            blur: 40,
            spread: 0,
            color: Color32::from_black_alpha(if self.is_dark { 150 } else { 46 }),
        };
        v.popup_shadow = Shadow {
            offset: [0, 8],
            blur: 20,
            spread: 0,
            color: Color32::from_black_alpha(if self.is_dark { 120 } else { 30 }),
        };
        v.extreme_bg_color = self.surface;
        v.faint_bg_color = self.surface;
        v.code_bg_color = self.surface;
        v.hyperlink_color = self.accent_text;
        v.warn_fg_color = self.warn;
        v.error_fg_color = self.err;
        v.selection.bg_fill = self.accent_text.gamma_multiply(0.32);
        v.selection.stroke = Stroke::new(1.5_f32, self.accent_text);
        v.interact_cursor = Some(CursorIcon::PointingHand);
        v.slider_trailing_fill = true;
        v.striped = false;
        v.indent_has_left_vline = false;
        v.window_highlight_topmost = false;

        let paint = |w: &mut egui::style::WidgetVisuals, fill: Color32, stroke: Color32| {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0_f32, stroke);
            w.fg_stroke = Stroke::new(1.5_f32, self.text);
            w.corner_radius = CornerRadius::same(8);
            w.expansion = 0.0;
        };
        paint(&mut v.widgets.noninteractive, self.surface, self.line);
        paint(&mut v.widgets.inactive, self.surface, self.line_strong);
        paint(&mut v.widgets.hovered, self.surface_hover, self.text_3);
        paint(&mut v.widgets.active, self.surface_hover, self.accent_text);
        paint(&mut v.widgets.open, self.surface_hover, self.line_strong);

        let mut style = Style {
            visuals: v,
            ..Style::default()
        };
        style.text_styles = [
            (TextStyle::Small, body(12.0)),
            (TextStyle::Body, body(14.0)),
            (TextStyle::Button, body(14.0)),
            (TextStyle::Heading, strong(22.0)),
            (TextStyle::Monospace, mono(13.0)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.spacing.interact_size = Vec2::new(36.0, 30.0);
        style.spacing.menu_margin = Margin::same(6);
        style.spacing.window_margin = Margin::same(20);
        style.spacing.scroll = egui::style::ScrollStyle::thin();
        style.interaction.selectable_labels = false;
        style.interaction.tooltip_delay = 0.3;
        style
    }
}

const STRONG_FAMILY: &str = "strong";
const DISPLAY_FAMILY: &str = "display";

/// Body text: Segoe UI for Latin, BIZ UDPGothic for Japanese.
pub fn body(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Semibold headings and button labels.
pub fn strong(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(STRONG_FAMILY.into()))
}

/// DIN-style numerals (Bahnschrift) for ASCII figures, times and sizes.
/// Do not use for strings that mix digits and Japanese units (such as "3.2秒"):
/// use [`body`] (calibrated Latin + Japanese metrics) or [`figure`] instead.
pub fn display(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(DISPLAY_FAMILY.into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

const JP_FONT_CANDIDATES: &[(&str, u32)] = &[
    ("BIZ-UDGothicR.ttc", 1),
    ("YuGothM.ttc", 1),
    ("meiryo.ttc", 2),
];
const JP_BOLD_FONT_CANDIDATES: &[(&str, u32)] = &[
    ("BIZ-UDGothicB.ttc", 1),
    ("YuGothB.ttc", 2),
    ("meiryob.ttc", 2),
];

#[derive(Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
struct LoadedFonts {
    jp_file: Option<&'static str>,
    din_loaded: bool,
}

fn windows_fonts_dir() -> PathBuf {
    std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts")
}

/// Loads Windows system fonts. Every family falls back to egui's bundled fonts, so a missing
/// file only changes the look; the named families always exist (egui panics on unknown ones).
pub fn install_fonts(ctx: &Context) {
    let _ = install_fonts_from(ctx, &windows_fonts_dir(), JP_FONT_CANDIDATES);
}

fn install_fonts_from(
    ctx: &Context,
    dir: &Path,
    jp_candidates: &[(&'static str, u32)],
) -> LoadedFonts {
    let mut fonts = FontDefinitions::default();
    let default_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let default_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let latin = load_font(&mut fonts, dir, "latin", &[("segoeui.ttf", 0)]).map(|(k, _)| k);
    let latin_bold =
        load_font(&mut fonts, dir, "latin-semibold", &[("seguisb.ttf", 0)]).map(|(k, _)| k);
    // BIZ UDPGothic draws kana close to the full em, so Segoe UI looks a size smaller in
    // mixed lines ("アカウント ID"). Scale the Latin faces up to match the kana.
    for key in ["latin", "latin-semibold"] {
        if let Some(data) = fonts.font_data.get_mut(key) {
            Arc::make_mut(data).tweak.scale = 1.07;
        }
    }
    let loaded_jp = load_font(&mut fonts, dir, "jp", jp_candidates);
    let jp_file = loaded_jp.as_ref().map(|(_, file)| *file);
    let jp = loaded_jp.map(|(k, _)| k);
    let jp_bold = load_font(&mut fonts, dir, "jp-bold", JP_BOLD_FONT_CANDIDATES).map(|(k, _)| k);
    let din = load_font(&mut fonts, dir, "din", &[("bahnschrift.ttf", 0)]).map(|(k, _)| k);
    let din_loaded = din.is_some();
    let code = load_font(
        &mut fonts,
        dir,
        "code",
        &[("CascadiaMono.ttf", 0), ("consola.ttf", 0)],
    )
    .map(|(k, _)| k);

    let chain = |names: &[&Option<String>], tail: &[String]| -> Vec<String> {
        names
            .iter()
            .filter_map(|n| (*n).clone())
            .chain(tail.iter().cloned())
            .collect()
    };
    let families = [
        (
            FontFamily::Proportional,
            chain(&[&latin, &jp], &default_prop),
        ),
        (
            FontFamily::Name(STRONG_FAMILY.into()),
            chain(&[&latin_bold, &latin, &jp_bold, &jp], &default_prop),
        ),
        (
            FontFamily::Name(DISPLAY_FAMILY.into()),
            chain(&[&din, &latin, &jp], &default_prop),
        ),
        (FontFamily::Monospace, chain(&[&code, &jp], &default_mono)),
    ];
    fonts.families.extend(families);
    ctx.set_fonts(fonts);
    LoadedFonts {
        jp_file,
        din_loaded,
    }
}

fn load_font(
    fonts: &mut FontDefinitions,
    dir: &Path,
    key: &str,
    candidates: &[(&'static str, u32)],
) -> Option<(String, &'static str)> {
    candidates.iter().find_map(|&(file, index)| {
        let bytes = std::fs::read(dir.join(file)).ok()?;
        let mut data = FontData::from_owned(bytes);
        data.index = index;
        fonts.font_data.insert(key.to_owned(), Arc::new(data));
        Some((key.to_owned(), file))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DURATION_SAMPLES: &[&str] = &[
        "3.2秒",
        "5.1秒",
        "14秒",
        "3分12秒",
        "1時間5分",
        "延期",
        "失敗",
    ];

    fn glyph_span(values: impl Iterator<Item = f32>) -> Option<(f32, f32)> {
        let mut min_v = f32::INFINITY;
        let mut max_v = f32::NEG_INFINITY;
        let mut any = false;
        for v in values {
            min_v = min_v.min(v);
            max_v = max_v.max(v);
            any = true;
        }
        any.then_some((min_v, max_v))
    }

    fn assert_duration_layout(ctx: &Context, check_calibrated_ink_bottom: bool) {
        for ppp in [1.0_f32, 1.25, 1.5, 2.0] {
            let mut input = egui::RawInput::default();
            input
                .viewports
                .entry(input.viewport_id)
                .or_default()
                .native_pixels_per_point = Some(ppp);
            ctx.begin_pass(input);
            for sample in DURATION_SAMPLES {
                let galley = ctx.fonts_mut(|f| {
                    f.layout_no_wrap((*sample).to_owned(), body(14.0), Color32::WHITE)
                });
                let glyphs = &galley.rows[0].glyphs;
                for pair in glyphs.windows(2) {
                    let prev_max_x =
                        pair[0].pos.x + pair[0].uv_rect.offset.x + pair[0].uv_rect.size.x;
                    let next_min_x = pair[1].pos.x + pair[1].uv_rect.offset.x;
                    assert!(
                        prev_max_x <= next_min_x + 0.01,
                        "glyph overlap in {sample:?} at ppp={ppp}: '{}' max_x={prev_max_x:.2} > '{}' min_x={next_min_x:.2}",
                        pair[0].chr,
                        pair[1].chr,
                    );
                }
                // Typographic baseline (`glyph.pos.y`) must stay aligned across Latin digits
                // and CJK units on every font configuration (unlike `display`, where Bahnschrift
                // drops CJK fallback glyphs by ~3 pt).
                if let Some((min_y, max_y)) = glyph_span(
                    glyphs
                        .iter()
                        .filter(|g| !g.uv_rect.is_nothing())
                        .map(|g| g.pos.y),
                ) {
                    assert!(
                        max_y - min_y <= 1.5,
                        "typographic baseline mismatch in {sample:?} at ppp={ppp}: min={min_y:.2}, max={max_y:.2}",
                    );
                }
                // Ink bottom (`pos.y + offset.y + size.y`) includes per-glyph CJK descenders:
                // BIZ UDPGothic keeps kanji ink flush with Segoe UI digits, whereas fallback
                // Yu Gothic UI ("分") and Meiryo UI ("間") extend 1.6-2.0 pt below the shared
                // baseline even when `pos.y` matches identically. Enforce the tighter ink-bottom
                // bound when the calibrated primary face (`BIZ-UDGothicR.ttc`) is installed.
                if check_calibrated_ink_bottom
                    && let Some((min_b, max_b)) = glyph_span(
                        glyphs
                            .iter()
                            .filter(|g| !g.uv_rect.is_nothing())
                            .map(|g| g.pos.y + g.uv_rect.offset.y + g.uv_rect.size.y),
                    )
                {
                    assert!(
                        max_b - min_b <= 1.5,
                        "baseline mismatch in {sample:?} at ppp={ppp}: min={min_b:.2}, max={max_b:.2}",
                    );
                }
            }
            let mut output = ctx.end_pass();
            output.textures_delta.clear();
        }
    }

    #[test]
    fn body_duration_glyphs_do_not_overlap_or_drop_baseline() {
        let ctx = Context::default();
        let loaded = install_fonts_from(&ctx, &windows_fonts_dir(), JP_FONT_CANDIDATES);
        assert_duration_layout(&ctx, loaded.jp_file == Some("BIZ-UDGothicR.ttc"));

        // Verify the typographic baseline check is not vacuous: `display` (Bahnschrift)
        // genuinely drops CJK fallback glyphs beyond the 1.5 pt threshold whenever both
        // Bahnschrift and a Japanese face are available.
        if loaded.din_loaded && loaded.jp_file.is_some() {
            let mut input = egui::RawInput::default();
            input
                .viewports
                .entry(input.viewport_id)
                .or_default()
                .native_pixels_per_point = Some(1.0);
            ctx.begin_pass(input);
            let galley = ctx.fonts_mut(|f| {
                f.layout_no_wrap("3分12秒".to_owned(), display(14.0), Color32::WHITE)
            });
            let (min_y, max_y) = glyph_span(
                galley.rows[0]
                    .glyphs
                    .iter()
                    .filter(|g| !g.uv_rect.is_nothing())
                    .map(|g| g.pos.y),
            )
            .expect("non-empty glyph run");
            assert!(
                max_y - min_y > 1.5,
                "expected Bahnschrift + CJK fallback in display() to show baseline drop (> 1.5), got {:.2}",
                max_y - min_y,
            );
            let mut output = ctx.end_pass();
            output.textures_delta.clear();
        }
    }

    #[test]
    fn body_duration_layout_holds_on_fallback_and_missing_fonts() {
        // 1. Environment without any Windows system fonts (egui bundled fonts only).
        let empty_dir = tempfile::tempdir().expect("tempdir");
        let ctx_empty = Context::default();
        let loaded_empty = install_fonts_from(&ctx_empty, empty_dir.path(), JP_FONT_CANDIDATES);
        assert_eq!(loaded_empty.jp_file, None);
        assert_duration_layout(&ctx_empty, false);

        // 2. English-locale Windows fallback faces (e.g. GitHub Actions windows-latest where
        //    BIZ-UDGothicR.ttc is absent and YuGothM.ttc is selected instead).
        let dir = windows_fonts_dir();
        for &candidate in &JP_FONT_CANDIDATES[1..] {
            let ctx = Context::default();
            let loaded = install_fonts_from(&ctx, &dir, &[candidate]);
            if loaded.jp_file == Some(candidate.0) {
                assert_duration_layout(&ctx, false);
            }
        }
    }
}
