//! The Graphite look shared by every screen: colors derived from the active visuals, painted stroke
//! icons (the default fonts have no consistent icon set) and the small widgets built from them.
use egui::{vec2, Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};

use crate::theme::mix;

/// Colors of the current theme (and cluster accent), computed from `ui.visuals()`.
pub struct Tokens {
    /// Top bar and sidebars, a step below the page.
    pub chrome: Color32,
    pub bg: Color32,
    pub card: Color32,
    pub line: Color32,
    pub line_strong: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    /// Text on an accent fill.
    pub on_accent: Color32,
    /// Buttons and other raised fills.
    pub raise: Color32,
    /// Hovered rows and buttons.
    pub hover: Color32,
    /// Chips, tags, the pressed segment.
    pub tag: Color32,
    /// The empty part of bars.
    pub track: Color32,
    /// Side panels (details).
    pub panel: Color32,
    pub dark: bool,
    /// AMOLED: pure black; lines and fills come from the accent.
    pub neon: bool,
}

pub fn tokens(ui: &Ui) -> Tokens {
    let v = ui.visuals();
    let (bg, text) = (v.panel_fill, v.text_color());
    let accent = crate::theme::accent(v);
    let luma = 0.299 * accent.r() as f32 + 0.587 * accent.g() as f32 + 0.114 * accent.b() as f32;
    let on_accent = if luma > 110.0 { Color32::from_rgb(11, 13, 18) } else { Color32::WHITE };
    let (muted, dim) = (mix(text, bg, 0.3), mix(text, bg, 0.45));
    if bg == Color32::BLACK {
        let a = |t: f32| mix(Color32::BLACK, accent, t);
        return Tokens {
            chrome: bg, bg, card: bg, line: a(0.3), line_strong: a(0.5), text, muted, dim, accent,
            accent_soft: v.selection.bg_fill, on_accent, raise: bg, hover: a(0.14), tag: a(0.18), track: a(0.2),
            panel: bg, dark: true, neon: true,
        };
    }
    Tokens {
        chrome: if v.dark_mode { mix(bg, Color32::BLACK, 0.3) } else { mix(bg, text, 0.035) },
        bg,
        card: if v.dark_mode { mix(bg, text, 0.03) } else { Color32::WHITE },
        line: mix(bg, text, 0.1),
        line_strong: mix(bg, text, 0.16),
        text,
        muted,
        dim,
        accent,
        accent_soft: v.selection.bg_fill,
        on_accent,
        raise: mix(bg, text, 0.06),
        hover: mix(bg, text, 0.07),
        tag: mix(bg, text, 0.08),
        track: mix(bg, text, 0.09),
        panel: mix(bg, text, 0.015),
        dark: v.dark_mode,
        neon: false,
    }
}

/// Status colors read better lighter on dark backgrounds and darker on light ones.
pub fn status_fg(ui: &Ui, c: Color32) -> Color32 {
    if ui.visuals().dark_mode { mix(c, Color32::WHITE, 0.3) } else { mix(c, Color32::BLACK, 0.15) }
}

// ---------------------------------------------------------------- font size per area

/// Font size of each part of the window, as a factor of its normal size (Settings › Appearance).
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Sizes {
    pub tabs: f32,
    pub sidebar: f32,
    pub lists: f32,
    pub details: f32,
    pub logs: f32,
    pub editor: f32,
    pub terminal: f32,
}

impl Default for Sizes {
    fn default() -> Self {
        Sizes { tabs: 1.0, sidebar: 1.0, lists: 1.0, details: 1.0, logs: 1.0, editor: 1.0, terminal: 1.0 }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Area {
    Tabs,
    Sidebar,
    Lists,
    Details,
    Logs,
    Editor,
    Terminal,
}

impl Area {
    /// Every area with its name and what it covers, for Settings.
    pub const ALL: [(Area, &'static str, &'static str); 7] = [
        (Area::Tabs, "Tabs", "Cluster, page, object and dock tabs"),
        (Area::Sidebar, "Sidebar", "Resources, cluster card and namespaces"),
        (Area::Lists, "Lists and pages", "Resource tables, overview, search, Helm"),
        (Area::Details, "Details", "The details panel and object tabs"),
        (Area::Logs, "Logs", "Log lines and their toolbar"),
        (Area::Editor, "YAML editor", "Edit YAML tabs"),
        (Area::Terminal, "Terminal", "Pod shells and local terminals"),
    ];
}

impl Sizes {
    pub fn get_mut(&mut self, a: Area) -> &mut f32 {
        match a {
            Area::Tabs => &mut self.tabs,
            Area::Sidebar => &mut self.sidebar,
            Area::Lists => &mut self.lists,
            Area::Details => &mut self.details,
            Area::Logs => &mut self.logs,
            Area::Editor => &mut self.editor,
            Area::Terminal => &mut self.terminal,
        }
    }
}

thread_local! {
    static SIZES: std::cell::Cell<Sizes> = std::cell::Cell::new(Sizes::default());
    /// Factor of the area being drawn; 1 outside any.
    static SCALE: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
}

/// The settings' sizes, set once a frame.
pub fn set_sizes(s: Sizes) {
    SIZES.with(|c| c.set(s));
}

pub fn factor(a: Area) -> f32 {
    let mut s = SIZES.with(|c| c.get());
    s.get_mut(a).clamp(0.5, 2.0)
}

/// `v` (a fixed font size, or a height around text) at the size of the area being drawn.
pub fn sz(v: f32) -> f32 {
    v * SCALE.with(|c| c.get())
}

/// Fixed-size fonts and heights (`sz`, `semibold`, `mono`, `prop`) follow `area` until dropped.
pub struct AreaScale(f32);

impl Drop for AreaScale {
    fn drop(&mut self) {
        SCALE.with(|c| c.set(self.0));
    }
}

pub fn area(a: Area) -> AreaScale {
    AreaScale(SCALE.with(|c| c.replace(factor(a))))
}

/// Draws `add` at `a`'s size: the fixed-size fonts, the style's text sizes and the row height. Nested
/// areas take their own size, not a multiple of the outer one.
pub fn scaled<R>(ui: &mut Ui, a: Area, add: impl FnOnce(&mut Ui) -> R) -> R {
    let _area = area(a);
    let f = factor(a);
    let base = ui.ctx().global_style();
    ui.scope(|ui| {
        let s = ui.style_mut();
        for (style, font) in s.text_styles.iter_mut() {
            if let Some(b) = base.text_styles.get(style) {
                font.size = b.size * f;
            }
        }
        s.spacing.interact_size.y = base.spacing.interact_size.y * f;
        add(ui)
    })
    .inner
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(sz(size), FontFamily::Name(crate::theme::SEMIBOLD.into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::new(sz(size), FontFamily::Monospace)
}

pub fn prop(size: f32) -> FontId {
    FontId::proportional(sz(size))
}

// ---------------------------------------------------------------- icons

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Icon {
    Kube,
    Search,
    Bell,
    Sliders,
    Contrast,
    Plus,
    X,
    Terminal,
    Doc,
    Edit,
    Trash,
    Grid,
    ChevronDown,
    ChevronRight,
    Folder,
    Wheel,
    Activity,
    Server,
    List,
    External,
    Check,
}

/// Paints `icon` into `rect`, drawn on a 24×24 grid like the design's SVGs.
pub fn paint_icon(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let p = ui.painter();
    let s = rect.width().min(rect.height()) / 24.0;
    let o = rect.center() - vec2(12.0, 12.0) * s;
    let at = |x: f32, y: f32| Pos2::new(o.x + x * s, o.y + y * s);
    let st = Stroke::new((2.0 * s).max(1.25), color);
    let line = |pts: &[(f32, f32)]| {
        p.add(Shape::line(pts.iter().map(|&(x, y)| at(x, y)).collect(), st));
    };
    let closed = |pts: &[(f32, f32)]| {
        p.add(Shape::closed_line(pts.iter().map(|&(x, y)| at(x, y)).collect(), st));
    };
    let circle = |x: f32, y: f32, r: f32| {
        p.circle_stroke(at(x, y), r * s, st);
    };
    let rrect = |x: f32, y: f32, w: f32, h: f32| {
        p.rect_stroke(Rect::from_min_size(at(x, y), vec2(w, h) * s), CornerRadius::same((1.5 * s) as u8), st, StrokeKind::Middle);
    };
    // Arc on a circle, angles in degrees (0 = right, 90 = down).
    let arc = |cx: f32, cy: f32, r: f32, a0: f32, a1: f32| -> Vec<(f32, f32)> {
        (0..=16).map(|i| {
            let a = (a0 + (a1 - a0) * i as f32 / 16.0).to_radians();
            (cx + r * a.cos(), cy + r * a.sin())
        }).collect()
    };
    match icon {
        Icon::Kube => {
            // The app mark: a heptagon around a watching eye (see icon.rs).
            closed(&[(12.0, 2.5), (20.0, 6.3), (22.0, 14.8), (16.5, 21.7), (7.5, 21.7), (2.0, 14.8), (4.0, 6.3)]);
            let mut lens = arc(12.0, 15.75, 6.25, 216.87, 323.13);
            lens.extend(arc(12.0, 8.25, 6.25, 36.87, 143.13));
            closed(&lens);
            p.circle_filled(at(12.0, 12.0), 1.8 * s, color);
        }
        Icon::Search => {
            circle(11.0, 11.0, 7.0);
            line(&[(16.2, 16.2), (20.5, 20.5)]);
        }
        Icon::Bell => {
            let mut pts = vec![(4.0, 17.0), (6.0, 15.0), (6.0, 9.0)];
            pts.extend(arc(12.0, 9.0, 6.0, 180.0, 360.0));
            pts.extend([(18.0, 15.0), (20.0, 17.0), (4.0, 17.0)]);
            line(&pts);
            line(&[(10.0, 20.5), (14.0, 20.5)]);
        }
        Icon::Sliders => {
            for (a, b) in [((4.0, 21.0), (4.0, 14.0)), ((4.0, 10.0), (4.0, 3.0)), ((12.0, 21.0), (12.0, 12.0)), ((12.0, 8.0), (12.0, 3.0)), ((20.0, 21.0), (20.0, 16.0)), ((20.0, 12.0), (20.0, 3.0)), ((1.0, 14.0), (7.0, 14.0)), ((9.0, 8.0), (15.0, 8.0)), ((17.0, 16.0), (23.0, 16.0))] {
                line(&[a, b]);
            }
        }
        Icon::Contrast => {
            circle(12.0, 12.0, 8.5);
            let half: Vec<Pos2> = arc(12.0, 12.0, 8.5, 90.0, 270.0).into_iter().map(|(x, y)| at(x, y)).collect();
            p.add(Shape::convex_polygon(half, color, Stroke::NONE));
        }
        Icon::Plus => {
            line(&[(12.0, 5.0), (12.0, 19.0)]);
            line(&[(5.0, 12.0), (19.0, 12.0)]);
        }
        Icon::X => {
            line(&[(6.0, 6.0), (18.0, 18.0)]);
            line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        Icon::Terminal => {
            line(&[(4.0, 17.0), (10.0, 11.0), (4.0, 5.0)]);
            line(&[(12.0, 19.0), (20.0, 19.0)]);
        }
        Icon::Doc => {
            closed(&[(14.0, 2.0), (6.0, 2.0), (4.0, 4.0), (4.0, 20.0), (6.0, 22.0), (18.0, 22.0), (20.0, 20.0), (20.0, 8.0)]);
            line(&[(14.0, 2.0), (14.0, 8.0), (20.0, 8.0)]);
            line(&[(8.0, 13.0), (16.0, 13.0)]);
            line(&[(8.0, 17.0), (13.0, 17.0)]);
        }
        Icon::Edit => {
            closed(&[(16.5, 3.5), (19.5, 6.5), (7.0, 19.0), (3.0, 20.0), (4.0, 16.0)]);
            line(&[(12.0, 20.0), (21.0, 20.0)]);
        }
        Icon::Trash => {
            line(&[(3.0, 6.0), (21.0, 6.0)]);
            line(&[(8.0, 6.0), (8.0, 4.0), (16.0, 4.0), (16.0, 6.0)]);
            line(&[(19.0, 6.0), (18.0, 20.0), (6.0, 20.0), (5.0, 6.0)]);
        }
        Icon::Grid => {
            rrect(3.0, 3.0, 7.0, 7.0);
            rrect(14.0, 3.0, 7.0, 7.0);
            rrect(3.0, 14.0, 7.0, 7.0);
            rrect(14.0, 14.0, 7.0, 7.0);
        }
        Icon::ChevronDown => line(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)]),
        Icon::ChevronRight => line(&[(9.0, 6.0), (15.0, 12.0), (9.0, 18.0)]),
        Icon::Folder => closed(&[(3.0, 6.0), (9.0, 6.0), (11.0, 8.0), (21.0, 8.0), (21.0, 19.0), (20.0, 20.0), (4.0, 20.0), (3.0, 19.0)]),
        Icon::Wheel => {
            circle(12.0, 12.0, 7.0);
            circle(12.0, 12.0, 2.0);
            for (a, b) in [((12.0, 2.0), (12.0, 7.0)), ((12.0, 17.0), (12.0, 22.0)), ((2.0, 12.0), (7.0, 12.0)), ((17.0, 12.0), (22.0, 12.0))] {
                line(&[a, b]);
            }
        }
        Icon::Activity => line(&[(22.0, 12.0), (18.0, 12.0), (15.0, 21.0), (9.0, 3.0), (6.0, 12.0), (2.0, 12.0)]),
        Icon::Server => {
            rrect(3.0, 3.5, 18.0, 7.0);
            rrect(3.0, 13.5, 18.0, 7.0);
            p.circle_filled(at(7.0, 7.0), 1.3 * s, color);
            p.circle_filled(at(7.0, 17.0), 1.3 * s, color);
        }
        Icon::List => {
            for y in [6.0, 12.0, 18.0] {
                line(&[(4.0, y), (20.0, y)]);
            }
        }
        Icon::External => {
            line(&[(14.0, 3.0), (21.0, 3.0), (21.0, 10.0)]);
            line(&[(21.0, 3.0), (12.0, 12.0)]);
            line(&[(19.0, 14.0), (19.0, 19.0), (17.0, 21.0), (5.0, 21.0), (3.0, 19.0), (3.0, 7.0), (5.0, 5.0), (10.0, 5.0)]);
        }
        Icon::Check => line(&[(5.0, 12.0), (10.0, 17.0), (20.0, 7.0)]),
    }
}

/// An icon laid out like a glyph (no interaction).
pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_icon(ui, rect, icon, color);
    r
}

/// Square icon-only button with a hover fill; `tip` is also its accessible name.
pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32, tip: &str) -> Response {
    let t = tokens(ui);
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if r.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(7), t.hover);
    }
    paint_icon(ui, rect.shrink(size * 0.26), icon, if r.hovered() { t.text } else { t.muted });
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tip));
    r.on_hover_text(tip)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Btn {
    /// Framed, raised: the default.
    Normal,
    /// Accent fill.
    Primary,
    /// Red text and border (destructive).
    Danger,
    /// Pressed toggle (accent tint).
    On,
    /// No frame until hovered.
    Ghost,
}

/// Text button with an optional icon, 30 px tall.
pub fn button(ui: &mut Ui, kind: Btn, icon_: Option<Icon>, text: &str) -> Response {
    let t = tokens(ui);
    let red = crate::watch::RED;
    let (fill, stroke, fg) = match kind {
        Btn::Normal => (t.raise, Stroke::new(1.0, t.line_strong), t.text),
        Btn::Primary => (t.accent, Stroke::NONE, t.on_accent),
        Btn::Danger => (Color32::TRANSPARENT, Stroke::new(1.0, mix(t.bg, red, 0.45)), status_fg(ui, red)),
        Btn::On => (t.accent_soft, Stroke::NONE, t.text),
        Btn::Ghost => (Color32::TRANSPARENT, Stroke::NONE, t.muted),
    };
    let font = if kind == Btn::Primary { semibold(13.0) } else { prop(13.0) };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let icon_w = if icon_.is_some() { 14.0 + if text.is_empty() { 0.0 } else { 6.0 } } else { 0.0 };
    let size = vec2(galley.size().x + icon_w + 20.0, sz(30.0));
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    let hover = r.hovered() && ui.is_enabled();
    let fill = match (hover, kind) {
        (true, Btn::Primary) => mix(t.accent, Color32::WHITE, 0.12),
        (true, Btn::Ghost | Btn::Danger) => t.hover,
        (true, Btn::On) => mix(fill, t.text, 0.06),
        (true, _) => t.hover,
        _ => fill,
    };
    ui.painter().rect(rect, CornerRadius::same(8), fill, stroke, StrokeKind::Inside);
    let mut x = rect.left() + 10.0;
    if let Some(i) = icon_ {
        paint_icon(ui, Rect::from_center_size(Pos2::new(x + 7.0, rect.center().y), Vec2::splat(14.0)), i, fg);
        x += icon_w;
    }
    ui.painter().galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    r
}

/// Rounded status pill: tinted fill, colored text.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let t = tokens(ui);
    let fg = status_fg(ui, color);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), semibold(12.0), fg);
    let (rect, r) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, sz(20.0)), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), mix(t.bg, color, if t.dark { 0.16 } else { 0.14 }));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
    r
}

/// Small rounded tag (namespaces, labels, counts).
pub fn chip(ui: &mut Ui, text: &str, font: FontId) -> Response {
    let t = tokens(ui);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, mix(t.text, t.bg, 0.2));
    let (rect, r) = ui.allocate_exact_size(vec2(galley.size().x + 12.0, galley.size().y + 4.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(5), t.tag);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, mix(t.text, t.bg, 0.2));
    r
}

/// Keyboard hint like `Ctrl K`.
pub fn kbd(ui: &mut Ui, text: &str) -> Response {
    let t = tokens(ui);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), mono(10.5), t.muted);
    let (rect, r) = ui.allocate_exact_size(vec2(galley.size().x + 10.0, sz(17.0)), Sense::hover());
    ui.painter().rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, t.line_strong), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, t.muted);
    r
}

/// Uppercase section heading (small, muted, tracked).
pub fn section_label(ui: &mut Ui, text: &str) -> Response {
    let t = tokens(ui);
    ui.label(RichText::new(text.to_uppercase()).font(semibold(11.0)).color(t.dim).extra_letter_spacing(0.8))
}

/// Horizontal bar `frac` full (0..1), `w` wide.
pub fn bar(ui: &mut Ui, frac: f32, w: f32, color: Color32) -> Response {
    let t = tokens(ui);
    let (rect, r) = ui.allocate_exact_size(vec2(w, 6.0), Sense::hover());
    let track = t.track;
    ui.painter().rect_filled(rect, CornerRadius::same(3), track);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2((rect.width() * f).max(3.0), rect.height())), CornerRadius::same(3), color);
    }
    r
}

/// Page tab: label with an accent underline when current; italic while it is a preview.
pub fn tab(ui: &mut Ui, text: &str, current: bool, italic: bool, mono_: bool) -> Response {
    let t = tokens(ui);
    let fg = if current { t.text } else { t.muted };
    let mut rt = RichText::new(text).color(fg).font(match (mono_, current) {
        (true, _) => mono(14.0),
        (false, true) => semibold(15.0),
        (false, false) => prop(15.0),
    });
    if italic {
        rt = rt.italics();
    }
    let under = ui.painter().add(Shape::Noop); // the lit background goes under the label
    let r = ui.add(egui::Label::new(rt).sense(Sense::click_and_drag()).selectable(false));
    if current {
        // A lit tab: a raised background and a thick accent underline.
        let bottom = ui.max_rect().bottom();
        let lit = Rect::from_min_max(Pos2::new(r.rect.left() - 12.0, ui.max_rect().top() + 4.0), Pos2::new(r.rect.right() + 28.0, bottom));
        ui.painter().set(under, egui::epaint::RectShape::filled(lit, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, t.hover.gamma_multiply(0.7)));
        ui.painter().line_segment([Pos2::new(lit.left(), bottom - 1.5), Pos2::new(lit.right(), bottom - 1.5)], Stroke::new(3.0, t.accent));
    }
    r
}

/// Sidebar row: optional icon, label, optional count; filled with the accent tint when current.
pub fn nav_item(ui: &mut Ui, icon_: Option<Icon>, label: &str, count: Option<&str>, current: bool, indent: f32) -> Response {
    let t = tokens(ui);
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), sz(30.0)), Sense::click());
    let fill = if current { t.accent_soft } else if r.hovered() { t.hover } else { Color32::TRANSPARENT };
    ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
    let fg = if current { t.text } else { mix(t.text, t.bg, 0.2) };
    let mut x = rect.left() + 10.0 + indent;
    if let Some(i) = icon_ {
        paint_icon(ui, Rect::from_center_size(Pos2::new(x + 7.5, rect.center().y), Vec2::splat(15.0)), i, if current { t.accent } else { t.muted });
        x += 25.0;
    }
    let font = if current { semibold(13.0) } else { prop(13.0) };
    let mut right = rect.right() - 10.0;
    if let Some(c) = count {
        let g = ui.painter().layout_no_wrap(c.to_owned(), mono(11.5), if current { t.accent } else { t.dim });
        right -= g.size().x;
        ui.painter().galley(Pos2::new(right, rect.center().y - g.size().y / 2.0), g, t.dim);
        right -= 8.0;
    }
    // Long labels end in "…" before the count.
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_owned(), font, fg);
    job.wrap = egui::text::TextWrapping { max_width: (right - x).max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let g = ui.painter().layout_job(job);
    ui.painter().galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g, fg);
    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, current, label));
    r
}

/// A card: raised surface with a hairline border, optional title bar.
pub fn card<R>(ui: &mut Ui, title: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    let t = tokens(ui);
    egui::Frame::new().fill(t.card).stroke(Stroke::new(1.0, t.line)).corner_radius(CornerRadius::same(12)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        if let Some(title) = title {
            egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 10 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(title).font(semibold(14.0)).color(t.text));
            });
            let y = ui.cursor().top();
            ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, t.line));
        }
        egui::Frame::new().inner_margin(Margin::symmetric(16, 12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        }).inner
    }).inner
}

/// Color-dot for status (the default fonts have no reliable bullet glyph).
pub fn dot(ui: &mut Ui, color: Color32, r: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(r * 2.0 + 2.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), r, color);
    resp
}

/// Count badge painted with its right edge at `right_center` (sidebar Events, the bell).
pub fn badge(ui: &Ui, right_center: Pos2, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), semibold(10.5), status_fg(ui, color));
    let rect = Rect::from_min_size(Pos2::new(right_center.x - galley.size().x - 12.0, right_center.y - 9.0), vec2(galley.size().x + 12.0, 18.0));
    ui.painter().rect_filled(rect, CornerRadius::same(9), mix(tokens(ui).bg, color, 0.18));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, status_fg(ui, color));
}

/// One option of a segmented control; `dot` colors a status.
pub fn segment(ui: &mut Ui, text: &str, dot_: Option<Color32>, on: bool) -> Response {
    let t = tokens(ui);
    let font = if on { semibold(12.5) } else { prop(12.5) };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, if on { t.text } else { t.muted });
    let extra = if dot_.is_some() { 12.0 } else { 0.0 };
    let (rect, r) = ui.allocate_exact_size(vec2(galley.size().x + extra + 18.0, sz(24.0)), Sense::click());
    let fill = if on { t.tag } else if r.hovered() { t.hover } else { Color32::TRANSPARENT };
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    let mut x = rect.left() + 9.0;
    if let Some(c) = dot_ {
        ui.painter().circle_filled(Pos2::new(x + 3.0, rect.center().y), 3.0, c);
        x += extra;
    }
    ui.painter().galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, if on { t.text } else { t.muted });
    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, text));
    r
}

/// Object tab as a pill (`kind/name`); returns (tab, close button).
pub fn pill_tab(ui: &mut Ui, icon_: Option<Icon>, text: &str, current: bool, mono_: bool, closable: bool) -> (Response, Option<Response>) {
    let t = tokens(ui);
    let fg = if current { t.text } else { t.muted };
    let font = match (mono_, current) {
        (true, _) => mono(13.0),
        (false, true) => semibold(14.0),
        (false, false) => prop(14.0),
    };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let lead = if icon_.is_some() { 22.0 } else { 0.0 };
    let trail = if closable { 22.0 } else { 0.0 };
    let (rect, r) = ui.allocate_exact_size(vec2(galley.size().x + lead + trail + 24.0, sz(34.0)), Sense::click_and_drag());
    let (fill, stroke) = if current {
        (t.accent_soft, Stroke::new(1.0, mix(t.bg, t.accent, 0.5)))
    } else {
        (if r.hovered() { t.hover } else { Color32::TRANSPARENT }, Stroke::new(1.0, t.line))
    };
    ui.painter().rect(rect, CornerRadius::same(8), fill, stroke, StrokeKind::Inside);
    let mut x = rect.left() + 12.0;
    if let Some(i) = icon_ {
        paint_icon(ui, Rect::from_center_size(Pos2::new(x + 7.5, rect.center().y), Vec2::splat(15.0)), i, fg);
        x += lead;
    }
    ui.painter().galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    let close = closable.then(|| {
        let xr = Rect::from_center_size(Pos2::new(rect.right() - 15.0, rect.center().y), vec2(20.0, 20.0));
        let x = ui.interact(xr, r.id.with("close"), Sense::click());
        if x.hovered() {
            ui.painter().rect_filled(xr, CornerRadius::same(4), t.tag);
        }
        paint_icon(ui, xr.shrink(5.0), Icon::X, if x.hovered() { t.text } else { t.dim });
        x.on_hover_text("Close")
    });
    (r, close)
}

/// Search box: icon, borderless text field and an optional shortcut hint, in a rounded frame.
/// Returns the text field's response (for focus).
pub fn search_field(ui: &mut Ui, text: &mut String, hint: &str, kbd_: Option<&str>, width: f32) -> Response {
    let t = tokens(ui);
    let fill = ui.visuals().extreme_bg_color;
    egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, t.line_strong)).corner_radius(CornerRadius::same(8)).inner_margin(Margin { left: 10, right: 6, top: 4, bottom: 4 }).show(ui, |ui| {
        ui.set_width(width - 16.0);
        ui.horizontal(|ui| {
            icon(ui, Icon::Search, 14.0, t.muted);
            let w = ui.available_width() - if kbd_.is_some() { 52.0 } else { 0.0 };
            let r = ui.add(egui::TextEdit::singleline(text).frame(egui::Frame::NONE).hint_text(hint).desired_width(w));
            if let Some(k) = kbd_ {
                kbd(ui, k);
            }
            r
        })
        .inner
    })
    .inner
}

/// Drag to reorder a row of tabs. `items` holds each tab's (rect, being dragged, drag just ended).
/// While dragging, marks where the tab will land; when dropped, returns (from, to) for `move_item`.
pub fn reorder(ui: &Ui, items: &[(Rect, bool, bool)]) -> Option<(usize, usize)> {
    let from = items.iter().position(|i| i.1 || i.2)?;
    let pos = ui.ctx().pointer_latest_pos()?;
    let slot = items.iter().position(|i| pos.x < i.0.center().x).unwrap_or(items.len());
    if slot == from || slot == from + 1 {
        return None; // dropped where it was
    }
    let r = items[from].0;
    let x = match items.get(slot) {
        Some(i) => i.0.left() - 6.0,
        None => items[items.len() - 1].0.right() + 6.0,
    };
    ui.painter().line_segment([Pos2::new(x, r.top() - 2.0), Pos2::new(x, r.bottom() + 2.0)], Stroke::new(2.0, tokens(ui).accent));
    items[from].2.then_some((from, if slot > from { slot - 1 } else { slot }))
}

/// Removes `v[i]`; `cur` stays on its item, or on the one that takes its place (the left one at the end).
pub fn remove_item<T>(v: &mut Vec<T>, i: usize, cur: &mut usize) {
    if i >= v.len() {
        return;
    }
    v.remove(i);
    if *cur > i || *cur >= v.len() {
        *cur = cur.saturating_sub(1);
    }
}

/// Moves `v[from]` to index `to`, keeping `cur` on the same item.
pub fn move_item<T>(v: &mut Vec<T>, from: usize, to: usize, cur: &mut usize) {
    if from >= v.len() || to >= v.len() {
        return;
    }
    let item = v.remove(from);
    v.insert(to, item);
    *cur = if *cur == from {
        to
    } else if from < *cur && to >= *cur {
        *cur - 1
    } else if from > *cur && to <= *cur {
        *cur + 1
    } else {
        *cur
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_keeps_the_current_item() {
        let (mut v, mut cur) = (vec!['a', 'b', 'c', 'd'], 1); // on 'b'
        move_item(&mut v, 3, 0, &mut cur);
        assert_eq!((v.iter().collect::<String>(), v[cur]), ("dabc".into(), 'b'));
        move_item(&mut v, 2, 3, &mut cur); // 'b' itself moves right
        assert_eq!((v.iter().collect::<String>(), v[cur]), ("dacb".into(), 'b'));
        move_item(&mut v, 0, 1, &mut cur);
        assert_eq!((v.iter().collect::<String>(), v[cur]), ("adcb".into(), 'b'));
    }

    #[test]
    fn areas_scale_fixed_fonts_and_restore() {
        set_sizes(Sizes { logs: 1.5, tabs: 0.8, ..Default::default() });
        assert_eq!(mono(12.0).size, 12.0); // outside any area
        {
            let _logs = area(Area::Logs);
            assert_eq!(mono(12.0).size, 18.0);
            {
                let _tabs = area(Area::Tabs); // nested: its own size, not 1.5 × 0.8
                assert!((sz(10.0) - 8.0).abs() < 1e-4);
            }
            assert_eq!(sz(10.0), 15.0);
        }
        assert_eq!(sz(10.0), 10.0);
        set_sizes(Sizes::default());
    }

    #[test]
    fn removing_keeps_the_current_item() {
        let (mut v, mut cur) = (vec!['a', 'b', 'c', 'd'], 2); // on 'c'
        remove_item(&mut v, 0, &mut cur);
        assert_eq!(v[cur], 'c');
        remove_item(&mut v, 1, &mut cur); // 'c' itself: its right neighbour takes the place
        assert_eq!((v.iter().collect::<String>(), v[cur]), ("bd".into(), 'd'));
        remove_item(&mut v, 1, &mut cur); // the last one: its left neighbour
        assert_eq!((v.iter().collect::<String>(), cur), ("b".into(), 0));
        remove_item(&mut v, 0, &mut cur);
        assert_eq!((v.len(), cur), (0, 0));
    }
}
