// colours from the app icon (deep green, amber, cream), the system's own UI fonts, and
// flatter, squarer widgets than egui's defaults; light or dark follows the system

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
    Theme, Visuals,
};
use std::sync::Arc;

/// Colours that mean something in the window, for the current theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Color32,
    pub good: Color32,
    pub warn: Color32,
    pub bad: Color32,
    pub tile: Color32,
    pub muted: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            accent: Color32::from_rgb(0xF2, 0xA5, 0x2B),
            good: Color32::from_rgb(0x7F, 0xD1, 0xA4),
            warn: Color32::from_rgb(0xF2, 0xA5, 0x2B),
            bad: Color32::from_rgb(0xF2, 0x7A, 0x66),
            tile: Color32::from_rgb(0x17, 0x3B, 0x34),
            muted: Color32::from_rgb(0xB9, 0xC4, 0xBC),
        }
    } else {
        Palette {
            accent: Color32::from_rgb(0x8F, 0x52, 0x00),
            good: Color32::from_rgb(0x17, 0x6B, 0x4C),
            warn: Color32::from_rgb(0x8F, 0x52, 0x00),
            bad: Color32::from_rgb(0xB0, 0x2E, 0x1C),
            tile: Color32::from_rgb(0xFF, 0xFB, 0xF2),
            muted: Color32::from_rgb(0x4E, 0x5E, 0x58),
        }
    }
}

fn visuals(dark: bool) -> Visuals {
    let p = palette(dark);
    let (mut v, bg, panel, ink, line) = if dark {
        (
            Visuals::dark(),
            Color32::from_rgb(0x0E, 0x26, 0x21),
            Color32::from_rgb(0x12, 0x30, 0x2A),
            Color32::from_rgb(0xF3, 0xEA, 0xD7),
            Color32::from_rgb(0x2C, 0x52, 0x49),
        )
    } else {
        (
            Visuals::light(),
            Color32::from_rgb(0xF4, 0xEE, 0xE0),
            Color32::from_rgb(0xF8, 0xF3, 0xE8),
            Color32::from_rgb(0x12, 0x2E, 0x28),
            Color32::from_rgb(0xC9, 0xC0, 0xAA),
        )
    };
    let square = CornerRadius::same(3);
    v.panel_fill = panel;
    v.window_fill = panel;
    v.extreme_bg_color = bg;
    v.faint_bg_color = p.tile;
    v.code_bg_color = bg;
    v.override_text_color = Some(ink);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.bad;
    v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.45 } else { 0.30 });
    v.selection.stroke = Stroke::new(1.0, ink);
    v.window_corner_radius = square;
    v.menu_corner_radius = square;
    v.window_stroke = Stroke::new(1.0, line);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = square;
        w.fg_stroke.color = ink;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, line);
    v.widgets.inactive.weak_bg_fill = p.tile;
    v.widgets.inactive.bg_fill = p.tile;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, line);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.bg_stroke = Stroke::new(2.0, p.accent);
    v
}

// the platform's own UI and monospace faces, read from where every install keeps them
fn system_fonts() -> Option<(Vec<u8>, Vec<u8>)> {
    let candidates: &[(&str, &str)] = if cfg!(windows) {
        &[(
            r"C:\Windows\Fonts\segoeui.ttf",
            r"C:\Windows\Fonts\consola.ttf",
        )]
    } else {
        &[
            (
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            ),
            (
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
                "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            ),
        ]
    };
    candidates
        .iter()
        .find_map(|(ui, mono)| Some((std::fs::read(ui).ok()?, std::fs::read(mono).ok()?)))
}

pub fn apply(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    if let Some((ui, mono)) = system_fonts() {
        fonts
            .font_data
            .insert("ui".into(), Arc::new(FontData::from_owned(ui)));
        fonts
            .font_data
            .insert("mono".into(), Arc::new(FontData::from_owned(mono)));
        if let Some(f) = fonts.families.get_mut(&FontFamily::Proportional) {
            f.insert(0, "ui".into());
        }
        if let Some(f) = fonts.families.get_mut(&FontFamily::Monospace) {
            f.insert(0, "mono".into());
        }
    }
    ctx.set_fonts(fonts);
    for theme in [Theme::Dark, Theme::Light] {
        ctx.set_visuals_of(theme, visuals(theme == Theme::Dark));
        ctx.style_mut_of(theme, |s| {
            s.text_styles = [
                (TextStyle::Heading, FontId::proportional(21.0)),
                (TextStyle::Body, FontId::proportional(14.5)),
                (TextStyle::Button, FontId::proportional(14.5)),
                (TextStyle::Small, FontId::proportional(12.0)),
                (TextStyle::Monospace, FontId::monospace(13.5)),
            ]
            .into();
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(10.0, 4.0);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // WCAG relative luminance and contrast ratio
    fn luminance(c: Color32) -> f32 {
        let f = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn status_colours_are_readable_on_their_tiles_in_both_themes() {
        for dark in [true, false] {
            let v = visuals(dark);
            let p = palette(dark);
            let ink = v.override_text_color.unwrap();
            for (name, c) in [
                ("text", ink),
                ("accent", p.accent),
                ("good", p.good),
                ("bad", p.bad),
                ("muted", p.muted),
            ] {
                for (bg_name, bg) in [("tile", p.tile), ("panel", v.panel_fill)] {
                    let r = contrast(c, bg);
                    assert!(r >= 4.5, "{name} on {bg_name}, dark {dark}: {r:.2}");
                }
            }
        }
    }
}
