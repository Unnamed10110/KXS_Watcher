//! Color themes and the shared look (spacing, rounding, text sizes). Each theme is four colors;
//! every fill, border and highlight is mixed from them, so a new theme is one line in `palette`.
use egui::{vec2, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Ui, Visuals};
use serde::{Deserialize, Serialize};

/// `Dark`, `Light` and `System` keep the names egui's `ThemePreference` was saved under, so old
/// settings still load.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
    Amoled,
    AmoledRed,
    AmoledGreen,
    Crimson,
    Forest,
    Ocean,
    Violet,
}

pub const ALL: [Theme; 10] = [
    Theme::Dark, Theme::Light, Theme::System, Theme::Amoled, Theme::AmoledRed, Theme::AmoledGreen,
    Theme::Crimson, Theme::Forest, Theme::Ocean, Theme::Violet,
];

struct Palette {
    bg: Color32,
    /// Windows, menus, popups.
    surface: Color32,
    text: Color32,
    accent: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `a` → `b` by `t` (0..1), per sRGB channel.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::System => "System",
            Theme::Amoled => "AMOLED",
            Theme::AmoledRed => "AMOLED Red",
            Theme::AmoledGreen => "AMOLED Green",
            Theme::Crimson => "Crimson",
            Theme::Forest => "Forest",
            Theme::Ocean => "Ocean",
            Theme::Violet => "Violet",
        }
    }

    fn palette(self) -> Palette {
        let p = |bg, surface, text, accent| Palette { bg: rgb(bg), surface: rgb(surface), text: rgb(text), accent: rgb(accent) };
        match self {
            Theme::Dark | Theme::System => p(0x1e1f22, 0x2b2d31, 0xdcdde1, 0x4f8cff),
            Theme::Light => p(0xf3f4f6, 0xffffff, 0x1f2328, 0x2563eb),
            Theme::Amoled => p(0x000000, 0x0a0a0a, 0xe6e6e6, 0x22d3ee),
            Theme::AmoledRed => p(0x000000, 0x0d0606, 0xf2e4e4, 0xff3b3b),
            Theme::AmoledGreen => p(0x000000, 0x050d08, 0xe2f2e7, 0x22e06b),
            Theme::Crimson => p(0x1c1214, 0x2a1a1d, 0xf0e1e3, 0xef4444),
            Theme::Forest => p(0x111a15, 0x1a2820, 0xdcece2, 0x22c55e),
            Theme::Ocean => p(0x111827, 0x1b2436, 0xdbe4f0, 0x38bdf8),
            Theme::Violet => p(0x1d1a2b, 0x28233c, 0xe8e3fb, 0xa78bfa),
        }
    }
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let (bg, text, accent) = (p.bg, p.text, p.accent);
    let tone = |t: f32| mix(bg, text, t); // toward the text color: raised fills, borders
    let round = CornerRadius::same(5);

    v.panel_fill = bg;
    v.window_fill = p.surface;
    // Text fields sit below the panel; on pure black (AMOLED) nothing is lower, so they rise instead.
    let sunk = mix(bg, Color32::BLACK, 0.4);
    v.extreme_bg_color = if !dark { Color32::WHITE } else if sunk == bg { tone(0.11) } else { sunk };
    v.faint_bg_color = tone(0.035); // striped rows
    v.code_bg_color = tone(0.06);
    v.window_stroke = Stroke::new(1.0, tone(0.14));
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.window_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(if dark { 110 } else { 40 }) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 10, spread: 0, color: Color32::from_black_alpha(if dark { 90 } else { 30 }) };
    v.hyperlink_color = if dark { mix(accent, Color32::WHITE, 0.3) } else { mix(accent, Color32::BLACK, 0.1) };
    v.selection.bg_fill = mix(bg, accent, if dark { 0.45 } else { 0.28 });
    v.selection.stroke = Stroke::new(1.0, if dark { mix(text, Color32::WHITE, 0.6) } else { mix(accent, Color32::BLACK, 0.45) });
    v.text_cursor.stroke = Stroke::new(2.0, accent);
    v.slider_trailing_fill = true;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = bg;
    w.noninteractive.weak_bg_fill = bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, tone(0.12)); // separators, frames
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.inactive.bg_fill = tone(0.10);
    w.inactive.weak_bg_fill = tone(0.07); // buttons
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, mix(text, bg, 0.12));
    w.hovered.bg_fill = tone(0.16);
    w.hovered.weak_bg_fill = tone(0.12);
    w.hovered.bg_stroke = Stroke::new(1.0, mix(bg, accent, 0.5));
    w.hovered.fg_stroke = Stroke::new(1.5, text);
    w.active.bg_fill = tone(0.22);
    w.active.weak_bg_fill = mix(bg, accent, 0.35);
    w.active.bg_stroke = Stroke::new(1.0, accent);
    w.active.fg_stroke = Stroke::new(2.0, text);
    w.open.bg_fill = tone(0.10);
    w.open.weak_bg_fill = tone(0.10);
    w.open.bg_stroke = Stroke::new(1.0, tone(0.2));
    w.open.fg_stroke = Stroke::new(1.0, text);
    for s in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        s.corner_radius = round;
    }
    v
}

fn polish(s: &mut egui::Style, p: &Palette, dark: bool) {
    s.visuals = visuals(p, dark);
    s.spacing.item_spacing = vec2(8.0, 4.0);
    s.spacing.button_padding = vec2(7.0, 3.0);
    s.spacing.interact_size.y = 20.0;
    s.spacing.window_margin = Margin::same(10);
    s.spacing.menu_margin = Margin::same(6);
    use FontFamily::{Monospace, Proportional};
    s.text_styles = [
        (TextStyle::Small, FontId::new(10.0, Proportional)),
        (TextStyle::Body, FontId::new(13.0, Proportional)),
        (TextStyle::Button, FontId::new(13.0, Proportional)),
        (TextStyle::Heading, FontId::new(18.0, Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, Monospace)),
    ]
    .into();
}

pub fn apply(ctx: &egui::Context, theme: Theme) {
    // Custom themes are dark ones; `System` switches between Dark and Light with the OS.
    let (pref, dark) = match theme {
        Theme::Light => (egui::ThemePreference::Light, Theme::Dark),
        Theme::System => (egui::ThemePreference::System, Theme::Dark),
        t => (egui::ThemePreference::Dark, t),
    };
    ctx.style_mut_of(egui::Theme::Dark, |s| polish(s, &dark.palette(), true));
    ctx.style_mut_of(egui::Theme::Light, |s| polish(s, &Theme::Light.palette(), false));
    ctx.set_theme(pref);
}

/// Tiles drawn in each theme's own colors; returns true when the pick changed.
pub fn picker(ui: &mut Ui, cur: &mut Theme) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        for t in ALL {
            let p = t.palette();
            let on = *cur == t;
            let tile = egui::Button::new(egui::RichText::new(t.name()).color(p.text))
                .fill(p.bg)
                .stroke(Stroke::new(if on { 2.0 } else { 1.0 }, if on { p.accent } else { mix(p.bg, p.accent, 0.5) }))
                .min_size(vec2(112.0, 30.0));
            if ui.add(tile).clicked() && !on {
                *cur = t;
                changed = true;
            }
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_theme_setting_still_loads() {
        for (old, new) in [(egui::ThemePreference::Dark, Theme::Dark), (egui::ThemePreference::Light, Theme::Light), (egui::ThemePreference::System, Theme::System)] {
            assert_eq!(serde_json::from_str::<Theme>(&serde_json::to_string(&old).unwrap()).unwrap(), new);
        }
    }

    #[test]
    fn mixes_colors() {
        assert_eq!(mix(Color32::BLACK, Color32::WHITE, 0.5), Color32::from_rgb(128, 128, 128));
        assert_eq!(mix(rgb(0x102030), rgb(0x102030), 0.7), rgb(0x102030));
    }
}
