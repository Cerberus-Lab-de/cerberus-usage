//! Cerberus Lab dark theme: palette, text styles and system fonts.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
};

/// Brand palette (exact values from the design).
pub mod palette {
    use eframe::egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x0A, 0x14, 0x24);
    pub const PANEL: Color32 = Color32::from_rgb(0x0F, 0x1D, 0x33);
    pub const PANEL2: Color32 = Color32::from_rgb(0x13, 0x25, 0x3F);
    pub const LINE: Color32 = Color32::from_rgb(0x1D, 0x35, 0x57);
    pub const CYAN: Color32 = Color32::from_rgb(0x22, 0xC4, 0xDC);
    pub const CYAN_DARK: Color32 = Color32::from_rgb(0x0E, 0x8F, 0xA6);
    pub const AMBER: Color32 = Color32::from_rgb(0xFF, 0xA6, 0x30);
    pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xF1, 0xFA);
    pub const MUTED: Color32 = Color32::from_rgb(0x9B, 0xB1, 0xC9);
    pub const OK_GREEN: Color32 = Color32::from_rgb(0x3D, 0xDC, 0x97);
    pub const NEUTRAL: Color32 = Color32::from_rgb(0x6F, 0x86, 0xA3);
}

/// Name of the extra text style used for large figures.
pub const BIG: &str = "big";

fn channel(c: u8) -> f32 {
    let s = f32::from(c) / 255.0;
    if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
}

fn luminance(c: Color32) -> f32 {
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}

/// WCAG contrast ratio between two colors (1.0 ..= 21.0).
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Builds font definitions with Segoe UI first, keeping the egui defaults as fallback.
/// `read` returns the bytes of a font file, or `None` if unreadable.
pub fn system_fonts_with(
    fonts_dir: &Path,
    read: impl Fn(&Path) -> Option<Vec<u8>>,
) -> Option<FontDefinitions> {
    let (name, bytes) = ["SegUIVar.ttf", "segoeui.ttf"]
        .iter()
        .find_map(|file| read(&fonts_dir.join(file)).map(|b| ((*file).to_string(), b)))?;
    let mut defs = FontDefinitions::default();
    defs.font_data.insert(name.clone(), std::sync::Arc::new(FontData::from_owned(bytes)));
    defs.families.entry(FontFamily::Proportional).or_default().insert(0, name);
    Some(defs)
}

/// Reads Segoe UI from `fonts_dir`; `None` if neither candidate file is readable.
pub fn system_fonts(fonts_dir: &Path) -> Option<FontDefinitions> {
    system_fonts_with(fonts_dir, |p| std::fs::read(p).ok())
}

/// `%WINDIR%\Fonts`, falling back to `C:\Windows\Fonts`.
fn windows_fonts_dir() -> PathBuf {
    std::env::var_os("WINDIR")
        .map(|w| PathBuf::from(w).join("Fonts"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\Fonts"))
}

/// Applies fonts, visuals, spacing and text styles. Call once at startup.
pub fn apply(ctx: &egui::Context) {
    if let Some(fonts) = system_fonts(&windows_fonts_dir()) {
        ctx.set_fonts(fonts);
    }
    ctx.set_visuals(visuals());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.text_styles = [
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            (TextStyle::Name(BIG.into()), FontId::new(24.0, FontFamily::Proportional)),
        ]
        .into();
    });
}

fn visuals() -> egui::Visuals {
    use palette::*;
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = PANEL2;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(10);
    v.override_text_color = Some(TEXT);
    v.weak_text_color = Some(MUTED);
    v.hyperlink_color = CYAN;
    v.selection.bg_fill = CYAN_DARK;
    v.selection.stroke = Stroke::new(1.0, TEXT);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.weak_bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.bg_fill = PANEL2;
    w.inactive.weak_bg_fill = PANEL2;
    w.inactive.bg_stroke = Stroke::new(1.0, LINE);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.bg_fill = PANEL2;
    w.hovered.weak_bg_fill = PANEL2;
    w.hovered.bg_stroke = Stroke::new(1.0, CYAN);
    w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
    w.active.bg_fill = PANEL;
    w.active.weak_bg_fill = PANEL;
    w.active.bg_stroke = Stroke::new(1.0, CYAN);
    w.active.fg_stroke = Stroke::new(2.0, TEXT);
    w.open.bg_fill = PANEL2;
    w.open.weak_bg_fill = PANEL2;
    w.open.bg_stroke = Stroke::new(1.0, CYAN);
    for ws in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        ws.corner_radius = CornerRadius::same(8);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::palette::*;
    use super::*;

    #[test]
    fn text_pairs_meet_wcag_aa() {
        for fg in [TEXT, MUTED] {
            for bg in [BG, PANEL, PANEL2] {
                let r = contrast_ratio(fg, bg);
                assert!(r >= 4.5, "{fg:?} on {bg:?} = {r}");
            }
        }
        let button = contrast_ratio(BG, CYAN);
        assert!(button >= 4.5, "BG on CYAN = {button}");
        let amber = contrast_ratio(AMBER, PANEL);
        assert!(amber >= 3.0, "AMBER on PANEL = {amber}");
    }

    #[test]
    fn contrast_ratio_known_values() {
        assert!((contrast_ratio(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(CYAN, CYAN) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn system_fonts_none_when_missing() {
        assert!(system_fonts_with(Path::new("x"), |_| None).is_none());
    }

    #[test]
    fn system_fonts_loads_segoe_when_present() {
        let defs = system_fonts_with(Path::new("x"), |p| {
            (p.file_name().is_some_and(|n| n == "segoeui.ttf")).then(|| vec![1, 2, 3])
        })
        .expect("segoeui.ttf found");
        assert_eq!(defs.families[&FontFamily::Proportional][0], "segoeui.ttf");
        assert!(defs.font_data.contains_key("segoeui.ttf"));
        // egui defaults stay behind Segoe as fallback
        assert!(defs.families[&FontFamily::Proportional].len() > 1);
    }

    #[test]
    fn system_fonts_prefers_variable_font() {
        let defs = system_fonts_with(Path::new("x"), |_| Some(vec![0])).unwrap();
        assert_eq!(defs.families[&FontFamily::Proportional][0], "SegUIVar.ttf");
    }
}
