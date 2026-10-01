//! Chrome palette: light / dark / system, with a custom accent color.
//! Colors follow the design reference (Chrome-family Material grays) with
//! tighter contrast for a cleaner 2026 look.

use egui::{Color32, Context, CornerRadius, Stroke, Vec2};

use rowser_shell::ThemeMode;

/// The full chrome palette.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Window background (tab strip + toolbar gaps).
    pub chrome_bg: Color32,
    /// Toolbar background.
    pub toolbar_bg: Color32,
    /// Active tab / raised surfaces.
    pub surface: Color32,
    /// Elevated surface (popups, dialogs).
    pub elevated: Color32,
    /// Field background (omnibox pill).
    pub field_bg: Color32,
    /// Field background when focused.
    pub field_focus_bg: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text.
    pub text_dim: Color32,
    /// Hairline borders.
    pub border: Color32,
    /// Hover wash.
    pub hover: Color32,
    /// The accent color.
    pub accent: Color32,
    /// Accent at low alpha (chips, highlights).
    pub accent_soft: Color32,
    /// On-accent text.
    pub on_accent: Color32,
    /// Destructive.
    pub danger: Color32,
    /// Success.
    pub success: Color32,
    /// Warning.
    pub warning: Color32,
    /// True when dark mode.
    pub dark: bool,
}

impl Theme {
    /// Builds the palette for `mode` + accent (`RRGGBB` hex).
    pub fn new(mode: ThemeMode, accent_hex: &str) -> Theme {
        let dark = match mode {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            // egui's fallback dark detection (no OS query without extra deps).
            ThemeMode::System => prefers_dark(),
        };
        let accent = parse_hex(accent_hex).unwrap_or(Color32::from_rgb(0x1A, 0x73, 0xE8));
        if dark {
            let chrome = Color32::from_rgb(0x29, 0x2A, 0x31);
            Theme {
                chrome_bg: chrome,
                toolbar_bg: Color32::from_rgb(0x35, 0x36, 0x3D),
                surface: Color32::from_rgb(0x41, 0x42, 0x4A),
                elevated: Color32::from_rgb(0x2B, 0x2C, 0x33),
                field_bg: Color32::from_rgb(0x4F, 0x50, 0x58),
                field_focus_bg: Color32::from_rgb(0x46, 0x47, 0x4F),
                text: Color32::from_rgb(0xE8, 0xEA, 0xED),
                text_dim: Color32::from_rgb(0x9A, 0xA0, 0xA6),
                border: Color32::from_rgb(0x50, 0x51, 0x58),
                hover: Color32::from_rgba_premultiplied(255, 255, 255, 16),
                accent,
                accent_soft: accent.gamma_multiply(0.35),
                on_accent: Color32::WHITE,
                danger: Color32::from_rgb(0xF2, 0x8B, 0x82),
                success: Color32::from_rgb(0x81, 0xC9, 0x95),
                warning: Color32::from_rgb(0xFD, 0xC6, 0x6C),
                dark,
            }
        } else {
            Theme {
                chrome_bg: Color32::from_rgb(0xDE, 0xE1, 0xE6),
                toolbar_bg: Color32::from_rgb(0xF8, 0xF9, 0xFA),
                surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                elevated: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                field_bg: Color32::from_rgb(0xF1, 0xF3, 0xF4),
                field_focus_bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                text: Color32::from_rgb(0x20, 0x21, 0x24),
                text_dim: Color32::from_rgb(0x5F, 0x63, 0x68),
                border: Color32::from_rgb(0xDA, 0xDC, 0xE0),
                hover: Color32::from_rgba_premultiplied(0, 0, 0, 14),
                accent,
                accent_soft: accent.gamma_multiply(0.22),
                on_accent: Color32::WHITE,
                danger: Color32::from_rgb(0xD9, 0x30, 0x25),
                success: Color32::from_rgb(0x1E, 0x8E, 0x3E),
                warning: Color32::from_rgb(0xE3, 0x71, 0x0A),
                dark,
            }
        }
    }

    /// Applies egui visuals matching the palette.
    pub fn apply(&self, ctx: &Context) {
        let mut style = (*ctx.style()).clone();
        let v = &mut style.visuals;
        v.dark_mode = self.dark;
        v.panel_fill = self.toolbar_bg;
        v.window_fill = self.elevated;
        v.extreme_bg_color = self.field_bg;
        v.faint_bg_color = self.field_bg;
        v.code_bg_color = self.field_bg;
        v.override_text_color = Some(self.text);
        v.widgets.noninteractive.bg_fill = self.toolbar_bg;
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, self.text_dim);
        v.widgets.inactive.bg_fill = self.field_bg;
        v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, self.text);
        v.widgets.hovered.bg_fill = self.hover;
        v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, self.text);
        v.widgets.active.bg_fill = self.accent_soft;
        v.widgets.active.fg_stroke = Stroke::new(1.0_f32, self.accent);
        v.widgets.open.bg_fill = self.hover;
        v.widgets.open.fg_stroke = Stroke::new(1.0_f32, self.text);
        v.selection.bg_fill = self.accent_soft;
        v.selection.stroke = Stroke::new(1.0_f32, self.accent);
        v.hyperlink_color = self.accent;
        v.window_stroke = Stroke::new(1.0_f32, self.border);
        v.window_corner_radius = CornerRadius::same(10);
        v.widgets.noninteractive.corner_radius = CornerRadius::same(4);
        v.widgets.inactive.corner_radius = CornerRadius::same(8);
        v.widgets.hovered.corner_radius = CornerRadius::same(8);
        v.widgets.active.corner_radius = CornerRadius::same(8);
        v.widgets.open.corner_radius = CornerRadius::same(8);
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.interact_size = Vec2::new(26.0, 26.0);
        style.spacing.scroll.bar_width = 10.0;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.5));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(19.0));
        ctx.set_style(style);
    }
}

fn prefers_dark() -> bool {
    // Respect the common desktop hint variables; default to light like the
    // reference mockups.
    std::env::var("EGUI_DARK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Parses `RRGGBB` or `#RRGGBB`.
pub fn parse_hex(hex: &str) -> Option<Color32> {
    let cleaned: String = hex.trim().trim_start_matches('#').to_string();
    if cleaned.len() != 6 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&cleaned[0..2], 16).ok()?;
    let g = u8::from_str_radix(&cleaned[2..4], 16).ok()?;
    let b = u8::from_str_radix(&cleaned[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}
