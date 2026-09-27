//! Color themes and the shared look (fonts, spacing, rounding, text sizes). Each theme is four colors;
//! every fill, border and highlight is mixed from them, so a new theme is one line in `palette`.
use std::sync::Arc;

use egui::{vec2, Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, Sense, Shadow, Stroke, StrokeKind, TextStyle, Ui, Visuals};
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
    AmoledPurple,
    AmoledPink,
    AmoledBlue,
    AmoledYellow,
    AmoledOrange,
    Crimson,
    Forest,
    Ocean,
    Violet,
}

pub const ALL: [Theme; 15] = [
    Theme::Dark, Theme::Light, Theme::System, Theme::Amoled, Theme::AmoledRed, Theme::AmoledGreen, Theme::AmoledPurple,
    Theme::AmoledPink, Theme::AmoledBlue, Theme::AmoledYellow, Theme::AmoledOrange, Theme::Crimson, Theme::Forest,
    Theme::Ocean, Theme::Violet,
];

/// Neon accents: the AMOLED themes' colors, also offered for clusters.
pub const NEON: [(Color32, &str); 8] = [
    (rgb(0x00e5ff), "Neon cyan"),
    (rgb(0xff1744), "Neon red"),
    (rgb(0x39ff14), "Neon green"),
    (rgb(0xb026ff), "Neon purple"),
    (rgb(0xff2bd6), "Neon pink"),
    (rgb(0x2e8bff), "Neon blue"),
    (rgb(0xffe600), "Neon yellow"),
    (rgb(0xff7a00), "Neon orange"),
];

/// How much air around rows and controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

/// Font family name for headings and emphasis (a real semibold face, not a brighter color).
pub const SEMIBOLD: &str = "semibold";

/// Choices for names, YAML and logs; "Auto" takes the first one installed.
pub const MONO_FONTS: [&str; 4] = ["Auto", "JetBrains Mono", "Cascadia Mono", "Consolas"];

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
    pub fn is_amoled(self) -> bool {
        self.palette().bg == Color32::BLACK
    }

    pub fn name(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::System => "System",
            Theme::Amoled => "AMOLED Cyan",
            Theme::AmoledRed => "AMOLED Red",
            Theme::AmoledGreen => "AMOLED Green",
            Theme::AmoledPurple => "AMOLED Purple",
            Theme::AmoledPink => "AMOLED Pink",
            Theme::AmoledBlue => "AMOLED Blue",
            Theme::AmoledYellow => "AMOLED Yellow",
            Theme::AmoledOrange => "AMOLED Orange",
            Theme::Crimson => "Crimson",
            Theme::Forest => "Forest",
            Theme::Ocean => "Ocean",
            Theme::Violet => "Violet",
        }
    }

    fn palette(self) -> Palette {
        let p = |bg, surface, text, accent| Palette { bg: rgb(bg), surface: rgb(surface), text: rgb(text), accent: rgb(accent) };
        match self {
            // Graphite: near-black, hairline borders, a soft indigo accent.
            Theme::Dark | Theme::System => p(0x0e1014, 0x181c23, 0xe7e9ee, 0x7c83ff),
            Theme::Light => p(0xf3f4f6, 0xffffff, 0x1f2328, 0x2563eb),
            // AMOLED: pure black everywhere; only the neon accent (and the text) lights up.
            Theme::Amoled => p(0x000000, 0x000000, 0xececec, 0x00e5ff),
            Theme::AmoledRed => p(0x000000, 0x000000, 0xececec, 0xff1744),
            Theme::AmoledGreen => p(0x000000, 0x000000, 0xececec, 0x39ff14),
            Theme::AmoledPurple => p(0x000000, 0x000000, 0xececec, 0xb026ff),
            Theme::AmoledPink => p(0x000000, 0x000000, 0xececec, 0xff2bd6),
            Theme::AmoledBlue => p(0x000000, 0x000000, 0xececec, 0x2e8bff),
            Theme::AmoledYellow => p(0x000000, 0x000000, 0xececec, 0xffe600),
            Theme::AmoledOrange => p(0x000000, 0x000000, 0xececec, 0xff7a00),
            Theme::Crimson => p(0x1c1214, 0x2a1a1d, 0xf0e1e3, 0xef4444),
            Theme::Forest => p(0x111a15, 0x1a2820, 0xdcece2, 0x22c55e),
            Theme::Ocean => p(0x111827, 0x1b2436, 0xdbe4f0, 0x38bdf8),
            Theme::Violet => p(0x1d1a2b, 0x28233c, 0xe8e3fb, 0xa78bfa),
        }
    }
}

/// Recolors the accent parts of `v` (selection, links, cursor, focus) — the theme's or a cluster's.
/// On pure black (AMOLED) every line, fill and glow comes from the accent too.
pub fn tint(v: &mut Visuals, accent: Color32) {
    let (bg, dark) = (v.panel_fill, v.dark_mode);
    let neon = bg == Color32::BLACK;
    v.selection.bg_fill = mix(bg, accent, if neon { 0.3 } else if dark { 0.2 } else { 0.16 });
    v.hyperlink_color = if !dark { mix(accent, Color32::BLACK, 0.1) } else { mix(accent, Color32::WHITE, if neon { 0.15 } else { 0.35 }) };
    v.text_cursor.stroke = Stroke::new(2.0, accent);
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent);
    if !neon {
        return;
    }
    let a = |t: f32| mix(Color32::BLACK, accent, t);
    let glow = |alpha: u8| Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
    v.window_fill = Color32::BLACK;
    v.extreme_bg_color = Color32::BLACK;
    v.code_bg_color = Color32::BLACK;
    v.faint_bg_color = a(0.06);
    v.window_stroke = Stroke::new(1.0, a(0.6));
    v.window_shadow.color = glow(46); // a neon glow instead of a shadow
    v.popup_shadow.color = glow(36);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, a(0.3));
    w.inactive.bg_fill = a(0.16); // check boxes, slider rails, scroll bars
    w.inactive.weak_bg_fill = Color32::BLACK; // buttons
    w.inactive.bg_stroke = Stroke::new(1.0, a(0.5));
    w.hovered.bg_fill = a(0.26);
    w.hovered.weak_bg_fill = a(0.14);
    w.hovered.bg_stroke = Stroke::new(1.0, accent);
    w.active.bg_fill = a(0.34);
    w.active.weak_bg_fill = a(0.3);
    w.open.bg_fill = a(0.16);
    w.open.weak_bg_fill = a(0.14);
    w.open.bg_stroke = Stroke::new(1.0, a(0.6));
}

/// The accent `tint` applied (kept in the cursor color, which is always the accent).
pub fn accent(v: &Visuals) -> Color32 {
    v.text_cursor.stroke.color
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let (bg, text) = (p.bg, p.text);
    let tone = |t: f32| mix(bg, text, t); // toward the text color: raised fills, borders
    let round = CornerRadius::same(6);

    v.panel_fill = bg;
    v.window_fill = p.surface;
    // Text fields sit below the panel; on pure black (AMOLED) nothing is lower, so they rise instead.
    let sunk = mix(bg, Color32::BLACK, 0.35);
    v.extreme_bg_color = if !dark { Color32::WHITE } else if sunk == bg { tone(0.08) } else { sunk };
    v.faint_bg_color = tone(0.025); // striped rows
    v.code_bg_color = tone(0.06);
    v.window_stroke = Stroke::new(1.0, tone(0.14));
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(if dark { 120 } else { 40 }) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(if dark { 100 } else { 30 }) };
    v.selection.stroke = Stroke::new(1.0, text);
    v.slider_trailing_fill = true;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = bg;
    w.noninteractive.weak_bg_fill = bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, tone(0.1)); // separators, frames
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.inactive.bg_fill = tone(0.1);
    w.inactive.weak_bg_fill = tone(0.05); // buttons
    w.inactive.bg_stroke = Stroke::new(1.0, tone(0.13)); // borders on buttons and text fields
    w.inactive.fg_stroke = Stroke::new(1.0, mix(text, bg, 0.1));
    w.hovered.bg_fill = tone(0.16);
    w.hovered.weak_bg_fill = tone(0.1);
    w.hovered.bg_stroke = Stroke::new(1.0, tone(0.24));
    w.hovered.fg_stroke = Stroke::new(1.5, text);
    w.active.bg_fill = tone(0.22);
    w.active.weak_bg_fill = mix(bg, p.accent, 0.3);
    w.active.fg_stroke = Stroke::new(2.0, text);
    w.open.bg_fill = tone(0.1);
    w.open.weak_bg_fill = tone(0.1);
    w.open.bg_stroke = Stroke::new(1.0, tone(0.2));
    w.open.fg_stroke = Stroke::new(1.0, text);
    for s in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        s.corner_radius = round;
    }
    tint(&mut v, p.accent);
    v
}

fn polish(s: &mut egui::Style, p: &Palette, dark: bool, density: Density, text: f32) {
    s.visuals = visuals(p, dark);
    let compact = density == Density::Compact;
    s.spacing.item_spacing = if compact { vec2(8.0, 3.0) } else { vec2(8.0, 6.0) };
    s.spacing.button_padding = if compact { vec2(6.0, 2.0) } else { vec2(8.0, 4.0) };
    s.spacing.interact_size.y = if compact { 20.0 } else { 24.0 }; // table rows are this + 6
    s.spacing.window_margin = Margin::same(if compact { 10 } else { 14 });
    s.spacing.menu_margin = Margin::same(6);
    use FontFamily::{Monospace, Proportional};
    s.text_styles = [
        (TextStyle::Small, FontId::new(text - 2.5, Proportional)),
        (TextStyle::Body, FontId::new(text, Proportional)),
        (TextStyle::Button, FontId::new(text, Proportional)),
        (TextStyle::Heading, FontId::new(text + 6.0, FontFamily::Name(SEMIBOLD.into()))),
        (TextStyle::Monospace, FontId::new(text - 0.5, Monospace)),
    ]
    .into();
}

pub fn apply(ctx: &egui::Context, theme: Theme, density: Density, text: f32) {
    // Custom themes are dark ones; `System` switches between Dark and Light with the OS.
    let (pref, dark) = match theme {
        Theme::Light => (egui::ThemePreference::Light, Theme::Dark),
        Theme::System => (egui::ThemePreference::System, Theme::Dark),
        t => (egui::ThemePreference::Dark, t),
    };
    let text = text.clamp(11.0, 17.0);
    ctx.style_mut_of(egui::Theme::Dark, |s| polish(s, &dark.palette(), true, density, text));
    ctx.style_mut_of(egui::Theme::Light, |s| polish(s, &Theme::Light.palette(), false, density, text));
    ctx.set_theme(pref);
}

/// First existing font file among `names` in the system and per-user font folders.
fn font_file(names: &[&str]) -> Option<Vec<u8>> {
    let mut dirs = vec![std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts")];
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(std::path::PathBuf::from(l).join("Microsoft\\Windows\\Fonts"));
    }
    names.iter().flat_map(|n| dirs.iter().map(move |d| d.join(n))).find_map(|p| std::fs::read(p).ok())
}

/// Interface font (Geist if installed, else Segoe UI) with its semibold face, and the monospace
/// font for names and logs. egui's own fonts stay as fallbacks (symbols, emoji). Returns the
/// names in use, for Settings.
pub fn install_fonts(ctx: &egui::Context, mono: &str) -> (String, String) {
    let mut f = egui::FontDefinitions::default();
    let mut add = |key: &str, files: &[(&str, &str)]| -> Option<String> {
        files.iter().find_map(|(file, name)| font_file(&[file]).map(|b| (b, *name))).map(|(bytes, name)| {
            f.font_data.insert(key.into(), Arc::new(egui::FontData::from_owned(bytes)));
            name.to_string()
        })
    };
    let ui_name = add("ui", &[("Geist-Regular.ttf", "Geist"), ("segoeui.ttf", "Segoe UI")]);
    let semi = add("ui-semibold", &[("Geist-SemiBold.ttf", "Geist"), ("seguisb.ttf", "Segoe UI")]);
    let monos: &[(&str, &str)] = match mono {
        "JetBrains Mono" => &[("JetBrainsMono-Regular.ttf", "JetBrains Mono")],
        "Cascadia Mono" => &[("CascadiaMono.ttf", "Cascadia Mono")],
        "Consolas" => &[("consola.ttf", "Consolas")],
        _ => &[("JetBrainsMono-Regular.ttf", "JetBrains Mono"), ("CascadiaMono.ttf", "Cascadia Mono"), ("consola.ttf", "Consolas")],
    };
    let mono_name = add("mono", monos);
    let prop = f.families.entry(FontFamily::Proportional).or_default();
    if ui_name.is_some() {
        prop.insert(0, "ui".into());
    }
    let mut semi_list = prop.clone();
    if semi.is_some() {
        semi_list.insert(0, "ui-semibold".into());
    }
    f.families.insert(FontFamily::Name(SEMIBOLD.into()), semi_list); // must exist even without the face
    if mono_name.is_some() {
        f.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
    }
    ctx.set_fonts(f);
    (ui_name.unwrap_or_else(|| "egui default".into()), mono_name.unwrap_or_else(|| "egui default".into()))
}

/// Theme tiles, each a small preview in its own colors; returns true when the pick changed.
pub fn tiles(ui: &mut Ui, cur: &mut Theme) -> bool {
    let t = crate::ui_kit::tokens(ui);
    let mut changed = false;
    let per_row = ((ui.available_width() + 12.0) / 164.0).floor().max(1.0) as usize;
    for row in ALL.chunks(per_row) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for &th in row {
                let p = th.palette();
                let on = *cur == th;
                let (rect, r) = ui.allocate_exact_size(vec2(152.0, 106.0), Sense::click());
                let prev = Rect::from_min_size(rect.min, vec2(152.0, 78.0));
                let painter = ui.painter();
                painter.rect_filled(prev, CornerRadius::same(10), p.bg);
                let side = mix(p.bg, p.surface, 0.8);
                painter.rect_filled(Rect::from_min_size(prev.min, vec2(prev.width(), 12.0)), CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 }, p.surface);
                painter.rect_filled(Rect::from_min_size(prev.min + vec2(0.0, 12.0), vec2(32.0, prev.height() - 12.0)), CornerRadius { nw: 0, ne: 0, sw: 10, se: 0 }, side);
                let bar = |x: f32, y: f32, w: f32, h: f32, c: Color32| painter.rect_filled(Rect::from_min_size(prev.min + vec2(x, y), vec2(w, h)), CornerRadius::same((h / 2.0) as u8), c);
                bar(6.0, 20.0, 20.0, 5.0, p.accent);
                bar(42.0, 22.0, 60.0, 6.0, mix(p.bg, p.text, 0.85));
                bar(42.0, 35.0, 90.0, 5.0, mix(p.bg, p.text, 0.35));
                bar(42.0, 46.0, 74.0, 5.0, mix(p.bg, p.text, 0.35));
                bar(42.0, 58.0, 34.0, 11.0, p.accent);
                if th == Theme::System {
                    let l = Theme::Light.palette();
                    let half = Rect::from_min_max(Pos2::new(prev.center().x, prev.top()), prev.max);
                    painter.rect_filled(half, CornerRadius { nw: 0, ne: 10, sw: 0, se: 10 }, l.bg);
                    painter.rect_filled(Rect::from_min_size(half.min, vec2(half.width(), 12.0)), CornerRadius { nw: 0, ne: 10, sw: 0, se: 0 }, l.surface);
                    painter.rect_filled(Rect::from_min_size(half.min + vec2(10.0, 35.0), vec2(46.0, 5.0)), CornerRadius::same(2), mix(l.bg, l.text, 0.35));
                    painter.rect_filled(Rect::from_min_size(half.min + vec2(10.0, 58.0), vec2(34.0, 11.0)), CornerRadius::same(5), l.accent);
                }
                let ring = if on { Stroke::new(2.0, t.accent) } else { Stroke::new(1.0, if r.hovered() { t.line_strong } else { t.line }) };
                painter.rect_stroke(prev, CornerRadius::same(10), ring, StrokeKind::Outside);
                let font = if on { crate::ui_kit::semibold(13.0) } else { FontId::proportional(13.0) };
                painter.text(Pos2::new(rect.left() + 2.0, prev.bottom() + 15.0), egui::Align2::LEFT_CENTER, th.name(), font, if on { t.text } else { t.muted });
                if on {
                    crate::ui_kit::paint_icon(ui, Rect::from_center_size(Pos2::new(rect.right() - 9.0, prev.bottom() + 15.0), vec2(14.0, 14.0)), crate::ui_kit::Icon::Check, t.accent);
                }
                if r.clicked() && !on {
                    *cur = th;
                    changed = true;
                }
                r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, th.name()));
            }
        });
        ui.add_space(4.0);
    }
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

    #[test]
    fn tint_is_readable_back() {
        let mut v = visuals(&Theme::Dark.palette(), true);
        tint(&mut v, rgb(0xf0507a));
        assert_eq!(accent(&v), rgb(0xf0507a));
    }
}
