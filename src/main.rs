#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window in release

mod cluster;
mod details;
mod find;
mod icon;
mod kubeconfig;
mod list;
mod ops;
mod search;
mod tabs;
mod theme;
mod ui_kit;
mod watch;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use egui::{vec2, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui};
use egui_dock::{DockArea, DockState, Style, TabStyle, TabViewer};
use egui_term::PtyEvent;
use serde::{Deserialize, Serialize};

use cluster::ClusterTab;
use kubeconfig::Ctx;
use tabs::{LogTab, TermTab, YamlTab};
use theme::mix;
use ui_kit::{Btn, Icon};
use watch::{take, Pending, GREEN, RED};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct Tab {
    pub id: u64,
    pub body: Body,
    /// Context id of the cluster the tab belongs to (for its accent color).
    pub cluster: Option<String>,
}

pub enum Body {
    Cluster(Box<ClusterTab>),
    Logs(Box<LogTab>),
    Term(Box<TermTab>),
    Yaml(Box<YamlTab>),
}

/// What tabs produce during a frame: new dock tabs and notifications.
#[derive(Default)]
pub struct Out {
    tabs: Vec<Tab>,
    toasts: Vec<(String, bool)>,
    /// Image for node-shell pods (from Settings).
    pub node_image: String,
}

impl Out {
    pub fn toast(&mut self, r: ops::Res) {
        self.toasts.push(match r {
            Ok(m) => (m, false),
            Err(e) => (e, true),
        });
    }

    pub fn tab(&mut self, body: Body) {
        self.tabs.push(Tab { id: next_id(), body, cluster: None });
    }

    /// Terminal tab; `cleanup` names a pod to delete when it closes (node shells).
    pub fn term(&mut self, ctx: &egui::Context, title: String, cmd: tabs::Command, cleanup: Option<tabs::Cleanup>) {
        let id = next_id();
        match TermTab::new(ctx, id, title, cmd) {
            Ok(mut t) => {
                t.cleanup = cleanup;
                self.tabs.push(Tab { id, body: Body::Term(Box::new(t)), cluster: None });
            }
            Err(e) => {
                self.toasts.push((format!("Cannot start terminal: {e}"), true));
                if let Some((client, ns, pod)) = cleanup {
                    tokio::spawn(ops::delete(client, kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), ns, pod));
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    /// Extra kubeconfig files or folders (folders are scanned one level deep).
    paths: Vec<PathBuf>,
    theme: theme::Theme,
    density: theme::Density,
    /// Base text size in points.
    text_size: f32,
    /// Font for names, YAML and logs (`theme::MONO_FONTS`).
    mono_font: String,
    pinned: Vec<String>,
    open: Vec<String>,
    zoom: f32,
    /// Image for node-shell pods (needs `sleep`; nsenter runs the node's own shell).
    node_shell_image: String,
    /// Open page and object tabs per cluster (context id), restored on reconnect.
    tabs: HashMap<String, cluster::SavedTabs>,
    /// Accent color per cluster (context id), handed out from `ACCENTS` on first use.
    accents: HashMap<String, Color32>,
    /// Names given to clusters (tab rename), by context id.
    aliases: HashMap<String, String>,
    /// The cluster shown at exit (context id).
    current: Option<String>,
}

/// Cluster colors, readable on dark and light themes. Rose is last, so red stays a deliberate pick (prod).
const ACCENTS: [Color32; 10] = [
    Color32::from_rgb(124, 131, 255), // indigo
    Color32::from_rgb(47, 191, 159),  // teal
    Color32::from_rgb(245, 165, 36),  // amber
    Color32::from_rgb(56, 189, 248),  // sky
    Color32::from_rgb(167, 139, 250), // violet
    Color32::from_rgb(76, 207, 138),  // green
    Color32::from_rgb(251, 146, 60),  // orange
    Color32::from_rgb(232, 121, 249), // orchid
    Color32::from_rgb(148, 163, 184), // slate
    Color32::from_rgb(240, 80, 122),  // rose
];

impl Settings {
    /// The cluster's accent; a new cluster gets the first color no other cluster uses
    /// (a neon one while an AMOLED theme is on).
    fn accent(&mut self, id: &str) -> Color32 {
        let pool: Vec<Color32> = if self.theme.is_amoled() { theme::NEON.iter().map(|n| n.0).collect() } else { ACCENTS.to_vec() };
        let n = self.accents.len();
        let free = pool.iter().copied().find(|c| !self.accents.values().any(|u| u == c));
        *self.accents.entry(id.to_string()).or_insert_with(|| free.unwrap_or(pool[n % pool.len()]))
    }

    fn apply_look(&self, ctx: &egui::Context) {
        theme::apply(ctx, self.theme, self.density, self.text_size);
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            paths: vec![kubeconfig::default_dir()],
            theme: theme::Theme::Dark,
            density: theme::Density::Comfortable,
            text_size: 13.0,
            mono_font: "Auto".into(),
            pinned: vec![],
            open: vec![],
            zoom: 1.0,
            node_shell_image: "busybox:1.36".into(),
            tabs: HashMap::new(),
            accents: HashMap::new(),
            aliases: HashMap::new(),
            current: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsPage {
    General,
    Appearance,
    Kubeconfig,
    Terminal,
    Shortcuts,
}

struct App {
    settings: Settings,
    contexts: Vec<Ctx>,
    /// Open clusters (`Body::Cluster`), one shown at a time under the top bar.
    clusters: Vec<Tab>,
    cur: usize,
    /// Logs, terminals and editors, docked at the bottom.
    dock: DockState<Tab>,
    term_rx: Receiver<(u64, PtyEvent)>,
    toasts: Vec<(String, bool, Instant)>,
    filter: String,
    /// The cluster list ("+" in the top bar).
    show_catalog: bool,
    show_settings: bool,
    settings_page: SettingsPage,
    paths_text: String,
    /// Pointer was over a terminal last frame: leave Ctrl+K/F to the shell.
    term_hovered: bool,
    /// Custom accent window open for this context: (id, name).
    custom_accent: Option<(String, String)>,
    /// A cluster tab being renamed: (context id, text, focus requested).
    renaming: Option<(String, String, bool)>,
    /// Open file/folder dialog for kubeconfig sources.
    picking: Option<Pending<Vec<PathBuf>>>,
    /// Interface and monospace fonts in use.
    fonts: (String, String),
    /// The last window placement eframe saved while the window was on screen.
    good_window: Option<String>,
}

/// eframe's saved window placement from while it was minimized (off screen, zero size):
/// restoring it opens the app where nobody can see it.
fn bad_window(saved: &str) -> bool {
    saved.contains("-32000") || saved.contains("inner_size_points:Some((x:0.0")
}

impl Tab {
    /// Ctrl+K opens the find bar; Ctrl+F (`filter`) focuses a list's filter box. False for terminals.
    fn open_find(&mut self, filter: bool) -> bool {
        match &mut self.body {
            Body::Cluster(c) => c.open_find(filter),
            Body::Logs(l) => l.find.open(),
            Body::Yaml(y) => y.find.open(),
            Body::Term(_) => return false,
        }
        true
    }
}

struct Viewer<'a> {
    out: &'a mut Out,
    /// Ctrl+K (`Some(false)`) / Ctrl+F (`Some(true)`) this frame: the tab under the pointer takes it.
    find_req: Option<bool>,
    claimed: bool,
    term_hovered: bool,
    accents: &'a HashMap<String, Color32>,
}

impl Viewer<'_> {
    fn accent(&self, t: &Tab) -> Option<Color32> {
        self.accents.get(t.cluster.as_ref()?).copied()
    }
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn title(&mut self, t: &mut Tab) -> egui::WidgetText {
        let text = match &t.body {
            Body::Cluster(c) => format!("☸ {}", c.kctx.name),
            Body::Logs(l) => format!("📜 {}", l.title),
            Body::Term(x) => format!("⌨ {}", x.title),
            Body::Yaml(y) => format!("📝 {}", y.title),
        };
        match self.accent(t) {
            Some(c) => RichText::new(text).color(c).into(),
            None => text.into(),
        }
    }

    /// The cluster's accent outlines its tabs.
    fn tab_style_override(&self, t: &Tab, global: &TabStyle) -> Option<TabStyle> {
        let c = self.accent(t)?;
        let mut s = global.clone();
        for i in [&mut s.active, &mut s.focused, &mut s.active_with_kb_focus, &mut s.focused_with_kb_focus] {
            i.outline_color = c;
        }
        Some(s)
    }

    fn id(&mut self, t: &mut Tab) -> egui::Id {
        egui::Id::new(("tab", t.id))
    }

    fn ui(&mut self, ui: &mut Ui, t: &mut Tab) {
        let hovered = ui.rect_contains_pointer(ui.max_rect());
        self.term_hovered |= hovered && matches!(t.body, Body::Term(_));
        if let (Some(filter), true, false) = (self.find_req, hovered, self.claimed) {
            self.claimed = t.open_find(filter);
        }
        if let Some(c) = self.accent(t) {
            theme::tint(ui.visuals_mut(), c);
        }
        match &mut t.body {
            Body::Cluster(c) => c.ui(ui, self.out),
            Body::Logs(l) => l.ui(ui),
            Body::Term(x) => x.ui(ui),
            Body::Yaml(y) => y.ui(ui),
        }
    }

    fn scroll_bars(&self, _: &Tab) -> [bool; 2] {
        [false, false]
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, term_rx: Receiver<(u64, PtyEvent)>) -> Self {
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let fonts = theme::install_fonts(&cc.egui_ctx, &settings.mono_font);
        settings.apply_look(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(settings.zoom);
        let window = cc.storage.and_then(|s| s.get_string("window"));
        if window.as_deref().is_some_and(bad_window) {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(1440.0, 900.0)));
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(80.0, 60.0)));
        }
        let mut app = App {
            contexts: kubeconfig::discover(&settings.paths),
            paths_text: settings.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"),
            settings,
            clusters: vec![],
            cur: 0,
            dock: DockState::new(vec![]),
            term_rx,
            toasts: vec![],
            filter: String::new(),
            show_catalog: false,
            show_settings: false,
            settings_page: SettingsPage::Appearance,
            term_hovered: false,
            custom_accent: None,
            renaming: None,
            picking: None,
            fonts,
            good_window: window.filter(|w| !bad_window(w)),
        };
        for id in app.settings.open.clone() {
            if let Some(c) = app.contexts.iter().find(|c| c.id() == id).cloned() {
                app.open_cluster(&cc.egui_ctx, c);
            }
        }
        app.cur = app.clusters.iter().position(|t| t.cluster.is_some() && t.cluster == app.settings.current).unwrap_or(0);
        app
    }

    /// Native dialog for kubeconfig files (several at once) or a folder; runs off the UI thread.
    fn pick_kubeconfigs(&mut self, ctx: &egui::Context, folder: bool) {
        if self.picking.is_some() {
            return; // one dialog at a time
        }
        let dialog = rfd::AsyncFileDialog::new().set_directory(kubeconfig::default_dir());
        self.picking = Some(Pending::spawn(ctx, async move {
            let picked = if folder { dialog.set_title("Add a folder of kubeconfigs").pick_folder().await.map(|f| vec![f]) } else { dialog.set_title("Add kubeconfig files").pick_files().await };
            picked.unwrap_or_default().iter().map(|f| f.path().to_path_buf()).collect()
        }));
    }

    /// New sources go into Settings and are scanned right away.
    fn add_sources(&mut self, picked: Vec<PathBuf>) {
        let before = self.contexts.len();
        let mut rejected = vec![];
        for p in picked {
            if p.is_file() && kubeconfig::contexts_in(&p).is_empty() {
                rejected.push(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            } else if !self.settings.paths.contains(&p) {
                self.settings.paths.push(p);
            }
        }
        self.paths_text = self.settings.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        self.contexts = kubeconfig::discover(&self.settings.paths);
        let now = Instant::now();
        if !rejected.is_empty() {
            self.toasts.push((format!("Not a kubeconfig (no contexts): {}", rejected.join(", ")), true, now));
        }
        match self.contexts.len().saturating_sub(before) {
            0 if rejected.is_empty() => self.toasts.push(("No new contexts: already listed".into(), false, now)),
            0 => {}
            n => self.toasts.push((format!("Added {n} context{}", if n == 1 { "" } else { "s" }), false, now)),
        }
    }

    fn rescan(&mut self) {
        self.contexts = kubeconfig::discover(&self.settings.paths);
        self.toasts.push((format!("Found {} contexts", self.contexts.len()), false, Instant::now()));
    }

    fn cluster_of(t: &Tab) -> Option<&ClusterTab> {
        match &t.body {
            Body::Cluster(c) => Some(c),
            _ => None,
        }
    }

    fn open_ids(&self) -> HashSet<String> {
        self.clusters.iter().filter_map(Self::cluster_of).map(|c| c.kctx.id()).collect()
    }

    fn open_cluster(&mut self, ctx: &egui::Context, c: Ctx) {
        let id = c.id();
        if let Some(i) = self.clusters.iter().position(|t| Self::cluster_of(t).is_some_and(|x| x.kctx.id() == id)) {
            self.cur = i;
            return;
        }
        let saved = self.settings.tabs.get(&id).cloned();
        self.settings.accent(&id);
        self.place(Tab { id: next_id(), body: Body::Cluster(Box::new(ClusterTab::new(ctx, c, saved))), cluster: Some(id) });
    }

    fn close_cluster(&mut self, i: usize) {
        let t = self.clusters.remove(i);
        if let Some(c) = Self::cluster_of(&t).and_then(|c| c.saved().map(|s| (c.kctx.id(), s))) {
            self.settings.tabs.insert(c.0, c.1); // reopening it restores its tabs
        }
        if self.cur > i || self.cur >= self.clusters.len() {
            self.cur = self.cur.saturating_sub(1);
        }
    }

    /// Clusters go to the top bar; logs / terminals / editors to the bottom dock, like Lens.
    fn place(&mut self, tab: Tab) {
        if matches!(tab.body, Body::Cluster(_)) {
            self.clusters.push(tab);
            self.cur = self.clusters.len() - 1;
        } else {
            self.dock.push_to_focused_leaf(tab);
        }
    }

    fn absorb(&mut self, out: Out) {
        for t in out.tabs {
            self.place(t);
        }
        let now = Instant::now();
        self.toasts.extend(out.toasts.into_iter().map(|(m, e)| (m, e, now)));
    }

    /// The cluster list: pinned, then per kubeconfig file. Returns true when a cluster was opened.
    fn catalog(&mut self, ui: &mut Ui) -> bool {
        let ctx = ui.ctx().clone();
        let t = ui_kit::tokens(ui);
        ui.horizontal(|ui| {
            if ui_kit::button(ui, Btn::Normal, Some(Icon::Plus), "Add kubeconfig…").on_hover_text("Pick kubeconfig files on disk").clicked() {
                self.pick_kubeconfigs(&ctx, false);
            }
            if ui_kit::button(ui, Btn::Normal, Some(Icon::Folder), "Folder…").on_hover_text("Add a folder: every kubeconfig in it (one level deep)").clicked() {
                self.pick_kubeconfigs(&ctx, true);
            }
            if self.picking.is_some() {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui_kit::icon_button(ui, Icon::Activity, 30.0, "Rescan kubeconfig files").clicked() {
                    self.rescan();
                }
            });
        });
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter contexts…").desired_width(f32::INFINITY));
        ui.add_space(2.0);
        let open = self.open_ids();
        let (mut to_open, mut toggle_pin, mut terminal, mut recolor, mut custom) = (None, None, None, None, None);
        let visible: Vec<&Ctx> = self.contexts.iter().filter(|c| watch::contains_ci(&c.name, &self.filter) || watch::contains_ci(&c.server, &self.filter)).collect();
        let pinned: Vec<&Ctx> = visible.iter().copied().filter(|c| self.settings.pinned.contains(&c.id())).collect();
        let mut files: Vec<&PathBuf> = visible.iter().map(|c| &c.file).collect();
        files.dedup();

        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(420.0).show(ui, |ui| {
            let mut entry = |ui: &mut Ui, c: &Ctx| {
                let is_open = open.contains(&c.id());
                let is_pinned = self.settings.pinned.contains(&c.id());
                let accent = self.settings.accents.get(&c.id()).copied().unwrap_or(t.dim);
                let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
                let fill = if is_open { mix(t.bg, accent, 0.12) } else if r.hovered() { t.hover } else { Color32::TRANSPARENT };
                ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
                ui.painter().circle_filled(Pos2::new(rect.left() + 14.0, rect.center().y), 4.5, accent);
                let shown = self.settings.aliases.get(&c.id()).map_or(c.name.as_str(), String::as_str);
                ui.painter().text(Pos2::new(rect.left() + 28.0, rect.top() + 13.0), Align2::LEFT_CENTER, shown, ui_kit::semibold(13.0), t.text);
                ui.painter().text(Pos2::new(rect.left() + 28.0, rect.top() + 28.0), Align2::LEFT_CENTER, &c.server, FontId::proportional(11.5), t.dim);
                if is_open {
                    ui.painter().text(Pos2::new(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, "open", FontId::proportional(11.5), t.muted);
                }
                let r = r.on_hover_text(format!("{}\n{}", c.server, c.file.display()));
                if r.clicked() {
                    to_open = Some(c.clone());
                }
                r.context_menu(|ui| {
                    if ui.button("☸ Connect").clicked() {
                        to_open = Some(c.clone());
                        ui.close();
                    }
                    if ui.button("⌨ Open terminal").clicked() {
                        terminal = Some(c.clone());
                        ui.close();
                    }
                    if ui.button(if is_pinned { "📌 Unpin" } else { "📌 Pin to top" }).clicked() {
                        toggle_pin = Some(c.id());
                        ui.close();
                    }
                    if ui.button("Copy context name").clicked() {
                        ui.ctx().copy_text(c.name.clone());
                        ui.close();
                    }
                    ui.separator();
                    ui.label(RichText::new("Cluster color").weak());
                    if let Some(col) = color_choices(ui, self.settings.accents.get(&c.id()).copied()) {
                        recolor = Some((c.id(), col));
                        ui.close();
                    }
                    // A window, not a submenu: context menus close on any click, even inside a picker.
                    if ui.button("🎨 Custom color…").clicked() {
                        custom = Some((c.id(), c.name.clone()));
                        ui.close();
                    }
                });
            };
            if !pinned.is_empty() {
                ui_kit::section_label(ui, "Pinned");
                for c in &pinned {
                    entry(ui, c);
                }
                ui.add_space(4.0);
            }
            for f in files {
                let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ui_kit::section_label(ui, &name).on_hover_text(f.display().to_string());
                for c in visible.iter().filter(|c| &c.file == f) {
                    entry(ui, c);
                }
                ui.add_space(4.0);
            }
            if self.contexts.is_empty() {
                ui.label(RichText::new("No contexts found. Add kubeconfig files or folders above.").color(t.muted));
            }
        });

        if let Some(id) = toggle_pin {
            if let Some(i) = self.settings.pinned.iter().position(|p| *p == id) {
                self.settings.pinned.remove(i);
            } else {
                self.settings.pinned.push(id);
            }
        }
        if let Some((id, col)) = recolor {
            self.settings.accents.insert(id, col);
        }
        if custom.is_some() {
            self.custom_accent = custom;
        }
        if let Some(c) = terminal {
            let mut out = Out::default();
            out.term(&ctx, format!("Terminal {}", c.name), tabs::local_shell(&c), None);
            self.settings.accent(&c.id());
            out.tabs.iter_mut().for_each(|t| t.cluster = Some(c.id()));
            self.absorb(out);
        }
        match to_open {
            Some(c) => {
                self.open_cluster(&ctx, c);
                true
            }
            None => false,
        }
    }

    /// Brand, open clusters as pills, the search box (Ctrl+K), warnings, theme and settings.
    fn top_bar(&mut self, ui: &mut Ui, find_req: &mut Option<bool>) {
        let ctx = ui.ctx().clone();
        let t = ui_kit::tokens(ui);
        let bar = ui.max_rect();
        let cur_accent = self.clusters.get(self.cur).and_then(|x| x.cluster.as_ref()).and_then(|id| self.settings.accents.get(id)).copied().unwrap_or(t.accent);
        let (mut select, mut close, mut recolor, mut custom) = (None, None, None, None);
        let (mut moved, mut rename, mut unalias, mut renamed) = (None, None, None, false);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add_space(6.0);
            ui_kit::icon(ui, Icon::Kube, 20.0, cur_accent);
            ui.label(RichText::new("KXS").font(ui_kit::semibold(14.0)).color(t.text).extra_letter_spacing(1.2));
            ui.add_space(6.0);
            let (sep, _) = ui.allocate_exact_size(vec2(1.0, 20.0), Sense::hover());
            ui.painter().rect_filled(sep, CornerRadius::ZERO, t.line_strong);
            ui.add_space(4.0);
            let n = self.clusters.len();
            let mut items = vec![];
            for (i, tab) in self.clusters.iter().enumerate() {
                let Some(c) = Self::cluster_of(tab) else { continue };
                let id = c.kctx.id();
                let accent = self.settings.accents.get(&id).copied().unwrap_or(t.accent);
                if let Some((rid, text, focused)) = &mut self.renaming
                    && *rid == id
                {
                    // Enter keeps the name, Esc the old one; empty goes back to the context name.
                    let r = ui.add(egui::TextEdit::singleline(text).desired_width(150.0).font(ui_kit::semibold(13.0)));
                    if !std::mem::replace(focused, true) {
                        r.request_focus();
                    }
                    items.push((r.rect, false, false));
                    if r.lost_focus() {
                        if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            let name = text.trim();
                            if name.is_empty() || name == c.kctx.name {
                                self.settings.aliases.remove(&id);
                            } else {
                                self.settings.aliases.insert(id.clone(), name.to_string());
                            }
                        }
                        renamed = true;
                    }
                    continue;
                }
                let label = self.settings.aliases.get(&id).cloned().unwrap_or_else(|| c.kctx.name.clone());
                let (main, x) = cluster_pill(ui, &label, accent, i == self.cur);
                items.push((main.rect, main.dragged(), main.drag_stopped()));
                if main.clicked() {
                    select = Some(i);
                }
                if main.double_clicked() {
                    rename = Some((id.clone(), label.clone()));
                }
                if x.clicked() || main.middle_clicked() {
                    close = Some(i);
                }
                main.context_menu(|ui| {
                    if ui.button("Rename…").clicked() {
                        rename = Some((id.clone(), label.clone()));
                        ui.close();
                    }
                    if self.settings.aliases.contains_key(&id) && ui.button("Reset name").clicked() {
                        unalias = Some(id.clone());
                        ui.close();
                    }
                    if i > 0 && ui.button("Move left").clicked() {
                        moved = Some((i, i - 1));
                        ui.close();
                    }
                    if i + 1 < n && ui.button("Move right").clicked() {
                        moved = Some((i, i + 1));
                        ui.close();
                    }
                    ui.separator();
                    ui.label(RichText::new("Cluster color").weak());
                    if let Some(col) = color_choices(ui, Some(accent)) {
                        recolor = Some((c.kctx.id(), col));
                        ui.close();
                    }
                    if ui.button("🎨 Custom color…").clicked() {
                        custom = Some((c.kctx.id(), c.kctx.name.clone()));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Close").clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
            }
            if let Some(m) = ui_kit::reorder(ui, &items) {
                moved = Some(m);
            }
            if ui_kit::icon_button(ui, Icon::Plus, 30.0, "Connect a cluster").clicked() {
                self.show_catalog = !self.show_catalog;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(6.0);
                if ui_kit::icon_button(ui, Icon::Sliders, 34.0, "Settings").clicked() {
                    self.show_settings = true;
                }
                let r = ui_kit::icon_button(ui, Icon::Contrast, 34.0, "Theme");
                egui::Popup::menu(&r).show(|ui| {
                    for th in theme::ALL {
                        if ui.selectable_label(self.settings.theme == th, th.name()).clicked() {
                            self.settings.theme = th;
                            self.settings.apply_look(&ctx);
                        }
                    }
                });
                let warnings = self.clusters.get(self.cur).and_then(Self::cluster_of).and_then(|c| c.warning_count()).unwrap_or(0);
                let r = ui_kit::icon_button(ui, Icon::Bell, 34.0, &format!("Warnings: {warnings}"));
                if warnings > 0 {
                    let pos = r.rect.right_top() + vec2(-8.0, 9.0);
                    let galley = ui.painter().layout_no_wrap(warnings.to_string(), ui_kit::semibold(10.0), Color32::from_rgb(26, 18, 4));
                    let pill = Rect::from_center_size(pos, vec2((galley.size().x + 8.0).max(16.0), 16.0));
                    ui.painter().rect_filled(pill, CornerRadius::same(8), watch::ORANGE);
                    ui.painter().galley(pill.center() - galley.size() / 2.0, galley, Color32::BLACK);
                }
                if r.clicked() {
                    if let Some(Body::Cluster(c)) = self.clusters.get_mut(self.cur).map(|t| &mut t.body) {
                        c.show_overview(&ctx);
                    }
                }
            });
        });
        // The search box sits in the middle of the bar.
        let w = (bar.width() * 0.3).clamp(220.0, 440.0);
        let rect = Rect::from_center_size(bar.center(), vec2(w, 30.0));
        let r = ui.interact(rect, egui::Id::new("top-search"), Sense::click());
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(8), if r.hovered() { t.hover } else { t.raise }, Stroke::new(1.0, t.line_strong), StrokeKind::Inside);
        ui_kit::paint_icon(ui, Rect::from_center_size(Pos2::new(rect.left() + 18.0, rect.center().y), vec2(15.0, 15.0)), Icon::Search, t.muted);
        p.text(Pos2::new(rect.left() + 34.0, rect.center().y), Align2::LEFT_CENTER, "Search in this view", FontId::proportional(13.0), t.muted);
        let k = Rect::from_center_size(Pos2::new(rect.right() - 30.0, rect.center().y), vec2(42.0, 18.0));
        p.rect_stroke(k, CornerRadius::same(4), Stroke::new(1.0, t.line_strong), StrokeKind::Inside);
        p.text(k.center(), Align2::CENTER_CENTER, "Ctrl K", ui_kit::mono(10.5), t.muted);
        if r.on_hover_text("Find text in the current view (Ctrl+K)").clicked() {
            *find_req = Some(false);
        }
        if let Some(i) = select {
            self.cur = i;
        }
        if renamed {
            self.renaming = None;
        }
        if let Some((id, text)) = rename {
            self.renaming = Some((id, text, false));
        }
        if let Some(id) = unalias {
            self.settings.aliases.remove(&id);
        }
        if let Some((from, to)) = moved {
            ui_kit::move_item(&mut self.clusters, from, to, &mut self.cur);
        }
        if let Some((id, col)) = recolor {
            self.settings.accents.insert(id, col);
        }
        if custom.is_some() {
            self.custom_accent = custom;
        }
        if let Some(i) = close {
            self.close_cluster(i);
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        let t = ui_kit::tokens(ui);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() / 7.0).max(24.0));
            ui_kit::icon(ui, Icon::Kube, 48.0, t.accent);
            ui.add_space(8.0);
            ui.label(RichText::new("KXS Watcher").font(ui_kit::semibold(26.0)).color(t.text));
            ui.label(RichText::new("Fast, native Kubernetes & k3s manager").color(t.muted));
            ui.add_space(18.0);
        });
        let w = 560.0_f32.min(ui.available_width() - 32.0);
        ui.horizontal(|ui| {
            ui.add_space((ui.available_width() - w) / 2.0);
            ui.vertical(|ui| {
                ui.set_width(w);
                let mut opened = false;
                ui_kit::card(ui, Some("Connect a cluster"), |ui| opened = self.catalog(ui));
                let _ = opened;
            });
        });
    }

    fn settings_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let (mut rescan, mut pick, mut fonts) = (false, None, false);
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(false).fixed_size([900.0, 560.0]).show(ctx, |ui| {
            let t = ui_kit::tokens(ui);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(190.0);
                    for (p, name) in [(SettingsPage::General, "General"), (SettingsPage::Appearance, "Appearance"), (SettingsPage::Kubeconfig, "Kubeconfig sources"), (SettingsPage::Terminal, "Terminal & node shell"), (SettingsPage::Shortcuts, "Keyboard shortcuts")] {
                        if ui_kit::nav_item(ui, None, name, None, self.settings_page == p, 0.0).clicked() {
                            self.settings_page = p;
                        }
                    }
                });
                let (sep, _) = ui.allocate_exact_size(vec2(1.0, 540.0), Sense::hover());
                ui.painter().rect_filled(sep, CornerRadius::ZERO, t.line);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.set_width(680.0);
                    egui::ScrollArea::vertical().id_salt("settings-page").max_height(540.0).show(ui, |ui| {
                        ui.set_width(660.0);
                        let heading = |ui: &mut Ui, title: &str, sub: &str| {
                            ui.label(RichText::new(title).font(ui_kit::semibold(20.0)).color(t.text));
                            ui.label(RichText::new(sub).color(t.muted));
                            ui.add_space(10.0);
                        };
                        match self.settings_page {
                            SettingsPage::General => {
                                heading(ui, "General", "How KXS Watcher behaves.");
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Zoom").font(ui_kit::semibold(13.0)));
                                    if ui.add(egui::Slider::new(&mut self.settings.zoom, 0.7..=2.0).step_by(0.05)).changed() {
                                        ctx.set_zoom_factor(self.settings.zoom);
                                    }
                                });
                                ui.add_space(8.0);
                                ui.label(RichText::new("Open clusters, their tabs and the namespace filter come back on the next start.").color(t.muted));
                                ui.label(RichText::new(format!("Helm CLI: {}", if *cluster::HELM { "found" } else { "not found on PATH (Helm view hidden)" })).color(t.muted));
                            }
                            SettingsPage::Appearance => {
                                heading(ui, "Appearance", "Theme, density and cluster colors. Changes apply right away and are saved.");
                                ui.label(RichText::new("Theme").font(ui_kit::semibold(14.0)));
                                ui.add_space(4.0);
                                if theme::tiles(ui, &mut self.settings.theme) {
                                    self.settings.apply_look(ctx);
                                }
                                ui.add_space(8.0);
                                ui.horizontal_top(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new("Density").font(ui_kit::semibold(14.0)));
                                        ui.horizontal(|ui| {
                                            for (d, name) in [(theme::Density::Comfortable, "Comfortable"), (theme::Density::Compact, "Compact")] {
                                                let kind = if self.settings.density == d { Btn::On } else { Btn::Ghost };
                                                if ui_kit::button(ui, kind, None, name).clicked() {
                                                    self.settings.density = d;
                                                    self.settings.apply_look(ctx);
                                                }
                                            }
                                        });
                                    });
                                    ui.add_space(28.0);
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new("Text size").font(ui_kit::semibold(14.0)));
                                        if ui.add(egui::Slider::new(&mut self.settings.text_size, 11.0..=16.0).step_by(0.5).suffix(" px")).changed() {
                                            self.settings.apply_look(ctx);
                                        }
                                    });
                                    ui.add_space(28.0);
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new("Names and logs").font(ui_kit::semibold(14.0)));
                                        egui::ComboBox::from_id_salt("mono-font").selected_text(if self.settings.mono_font == "Auto" { format!("Auto ({})", self.fonts.1) } else { self.settings.mono_font.clone() }).show_ui(ui, |ui| {
                                            for f in theme::MONO_FONTS {
                                                fonts |= ui.selectable_value(&mut self.settings.mono_font, f.to_string(), f).changed();
                                            }
                                        });
                                    });
                                });
                                ui.label(RichText::new(format!("Interface font: {} · install Geist or JetBrains Mono and they are picked up on the next start.", self.fonts.0)).small().color(t.dim));
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Cluster colors").font(ui_kit::semibold(14.0)));
                                    ui.label(RichText::new("Tabs, selections, logs and terminals of a cluster take its color.").color(t.muted));
                                });
                                ui.add_space(4.0);
                                let mut ids: Vec<Ctx> = self.contexts.iter().filter(|c| self.settings.accents.contains_key(&c.id())).cloned().collect();
                                ids.sort_by(|a, b| a.name.cmp(&b.name));
                                if ids.is_empty() {
                                    ui.label(RichText::new("Open a cluster to give it a color.").color(t.muted));
                                }
                                let (mut recolor, mut custom) = (None, None);
                                for c in &ids {
                                    let cur = self.settings.accents.get(&c.id()).copied();
                                    ui.horizontal_top(|ui| {
                                        ui_kit::dot(ui, cur.unwrap_or(t.dim), 5.0);
                                        ui.vertical(|ui| {
                                            ui.set_width(220.0);
                                            ui.label(RichText::new(&c.name).font(ui_kit::semibold(13.0)));
                                            ui.label(RichText::new(c.file.display().to_string()).font(ui_kit::mono(11.0)).color(t.dim));
                                        });
                                        ui.vertical(|ui| {
                                            if let Some(col) = color_choices(ui, cur) {
                                                recolor = Some((c.id(), col));
                                            }
                                        });
                                        if ui_kit::button(ui, Btn::Normal, None, "Custom…").clicked() {
                                            custom = Some((c.id(), c.name.clone()));
                                        }
                                    });
                                    ui.add_space(6.0);
                                }
                                if let Some((id, col)) = recolor {
                                    self.settings.accents.insert(id, col);
                                }
                                if custom.is_some() {
                                    self.custom_accent = custom;
                                }
                            }
                            SettingsPage::Kubeconfig => {
                                heading(ui, "Kubeconfig sources", "One file or folder per line. ~/.kube/config and $KUBECONFIG are always included; folders are scanned one level deep.");
                                ui.add(egui::TextEdit::multiline(&mut self.paths_text).desired_rows(8).desired_width(f32::INFINITY).code_editor());
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if ui_kit::button(ui, Btn::Primary, None, "Save & rescan").clicked() {
                                        self.settings.paths = self.paths_text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect();
                                        rescan = true;
                                    }
                                    if ui_kit::button(ui, Btn::Normal, Some(Icon::Plus), "Add files…").on_hover_text("Pick kubeconfig files on disk").clicked() {
                                        pick = Some(false);
                                    }
                                    if ui_kit::button(ui, Btn::Normal, Some(Icon::Folder), "Add folder…").on_hover_text("Every kubeconfig in the folder (one level deep)").clicked() {
                                        pick = Some(true);
                                    }
                                });
                            }
                            SettingsPage::Terminal => {
                                heading(ui, "Terminal & node shell", "Terminals run kubectl pinned to their cluster; no credentials are copied.");
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Node shell image").font(ui_kit::semibold(13.0))).on_hover_text("Used for node terminals: a privileged pod with this image runs on the node while the terminal is open");
                                    ui.add(egui::TextEdit::singleline(&mut self.settings.node_shell_image).desired_width(260.0));
                                });
                                ui.label(RichText::new("The image needs `sleep`; nsenter runs the node's own shell.").color(t.muted));
                            }
                            SettingsPage::Shortcuts => {
                                heading(ui, "Keyboard shortcuts", "");
                                egui::Grid::new("shortcuts").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
                                    for (k, what) in [
                                        ("Ctrl K", "Find text in the current view"),
                                        ("Ctrl F", "Focus the list filter (find in logs and YAML)"),
                                        ("Ctrl A", "Select every row shown"),
                                        ("Esc", "Clear the selection and hide the details"),
                                        ("Ctrl S", "Save in the YAML editor"),
                                        ("Enter / Shift Enter", "Next / previous match"),
                                        ("Double-click", "Open as a tab (sidebar: keep the page tab)"),
                                        ("Middle-click", "Open in the background, or close a tab"),
                                    ] {
                                        ui_kit::kbd(ui, k);
                                        ui.label(what);
                                        ui.end_row();
                                    }
                                });
                            }
                        }
                    });
                });
            });
        });
        self.show_settings = open;
        if rescan {
            self.rescan();
        }
        if let Some(folder) = pick {
            self.pick_kubeconfigs(ctx, folder);
        }
        if fonts {
            self.fonts = theme::install_fonts(ctx, &self.settings.mono_font);
        }
    }

    /// Any color for a cluster; changes apply live.
    fn custom_accent_ui(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.custom_accent.clone() else { return };
        let (mut open, mut done) = (true, false);
        egui::Window::new(format!("Cluster color · {name}")).id(egui::Id::new("custom-accent")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let mut col = self.settings.accent(&id);
            if egui::color_picker::color_picker_color32(ui, &mut col, egui::color_picker::Alpha::Opaque) {
                self.settings.accents.insert(id.clone(), col);
            }
            ui.add_space(4.0);
            done = ui_kit::button(ui, Btn::Primary, None, "Done").clicked();
        });
        if !open || done {
            self.custom_accent = None;
        }
    }

    fn catalog_ui(&mut self, ctx: &egui::Context) {
        if !self.show_catalog {
            return;
        }
        let mut open = true;
        let mut opened = false;
        egui::Window::new("Clusters").open(&mut open).collapsible(false).resizable(false).default_width(420.0).anchor(Align2::LEFT_TOP, [16.0, 56.0]).show(ctx, |ui| {
            opened = self.catalog(ui);
        });
        self.show_catalog = open && !opened;
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.2.elapsed() < Duration::from_secs(if t.1 { 10 } else { 5 }));
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts")).anchor(Align2::RIGHT_BOTTOM, [-14.0, -14.0]).order(egui::Order::Foreground).show(ctx, |ui| {
            for (msg, err, _) in &self.toasts {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(460.0);
                    ui.colored_label(if *err { RED } else { GREEN }, msg);
                });
            }
        });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

/// A cluster in the top bar: color dot, name, close button. Returns (pill, close).
fn cluster_pill(ui: &mut Ui, name: &str, accent: Color32, current: bool) -> (egui::Response, egui::Response) {
    let t = ui_kit::tokens(ui);
    let font = if current { ui_kit::semibold(13.0) } else { FontId::proportional(13.0) };
    let fg = if current { t.text } else { t.muted };
    let galley = ui.painter().layout_no_wrap(name.to_owned(), font, fg);
    let (rect, main) = ui.allocate_exact_size(vec2(galley.size().x + 50.0, 30.0), Sense::click_and_drag());
    let fill = if current { mix(t.chrome, accent, if t.neon { 0.1 } else { 0.16 }) } else if main.hovered() { t.hover } else { Color32::TRANSPARENT };
    let stroke = if current { Stroke::new(1.0, if t.neon { accent } else { mix(t.chrome, accent, 0.5) }) } else { Stroke::NONE };
    ui.painter().rect(rect, CornerRadius::same(8), fill, stroke, StrokeKind::Inside);
    ui.painter().circle_filled(Pos2::new(rect.left() + 13.0, rect.center().y), 4.0, accent);
    ui.painter().galley(Pos2::new(rect.left() + 24.0, rect.center().y - galley.size().y / 2.0), galley, fg);
    let xr = Rect::from_center_size(Pos2::new(rect.right() - 14.0, rect.center().y), vec2(20.0, 20.0));
    let x = ui.interact(xr, main.id.with("close"), Sense::click());
    if x.hovered() {
        ui.painter().rect_filled(xr, CornerRadius::same(5), t.tag);
    }
    ui_kit::paint_icon(ui, xr.shrink(5.0), Icon::X, if x.hovered() { t.text } else { t.dim });
    (main.on_hover_text("Drag to move · double-click to rename · right-click for color · middle-click to close"), x.on_hover_text(format!("Close {name}")))
}

/// Soft and neon swatches, one row each; returns the color clicked.
fn color_choices(ui: &mut Ui, cur: Option<Color32>) -> Option<Color32> {
    let dim = ui_kit::tokens(ui).dim;
    let mut picked = None;
    let neon: Vec<Color32> = theme::NEON.iter().map(|n| n.0).collect();
    for (label, colors) in [("Soft", ACCENTS.to_vec()), ("Neon", neon)] {
        ui.horizontal(|ui| {
            ui.add_sized([34.0, 22.0], egui::Label::new(RichText::new(label).size(11.5).color(dim)));
            for c in colors {
                if swatch(ui, c, cur == Some(c)).clicked() {
                    picked = Some(c);
                }
            }
        });
    }
    picked
}

/// A color choice; ringed when selected.
fn swatch(ui: &mut Ui, color: Color32, on: bool) -> egui::Response {
    let t = ui_kit::tokens(ui);
    let (rect, r) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    ui.painter().rect_filled(rect.shrink(2.0), CornerRadius::same(5), color);
    if on {
        ui.painter().rect_stroke(rect, CornerRadius::same(7), Stroke::new(2.0, t.text), StrokeKind::Inside);
    }
    r
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // First thing: text fields would treat Ctrl+K as "delete to end of line".
        const FIND_K: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);
        const FIND_F: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::F);
        const NEW_TAB: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::T);
        let mut find_req = match self.term_hovered {
            true => None,
            false => ui.input_mut(|i| if i.consume_shortcut(&FIND_K) { Some(false) } else { i.consume_shortcut(&FIND_F).then_some(true) }),
        };
        // Ctrl+T: a new tab in the current cluster; with none open, the cluster list.
        if !self.term_hovered && ui.input_mut(|i| i.consume_shortcut(&NEW_TAB)) {
            match self.clusters.get_mut(self.cur).map(|t| &mut t.body) {
                Some(Body::Cluster(c)) => c.new_tab(),
                _ => self.show_catalog = true,
            }
        }
        if let Some(picked) = take(&mut self.picking).filter(|p| !p.is_empty()) {
            self.add_sources(picked);
        }
        while let Ok((id, ev)) = self.term_rx.try_recv() {
            if let PtyEvent::Exit = ev {
                if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
                    self.dock.remove_tab(path);
                }
            }
        }

        let t = ui_kit::tokens(ui);
        egui::Panel::top("topbar").exact_size(48.0).frame(egui::Frame::new().fill(t.chrome).stroke(Stroke::new(1.0, t.line))).show(ui, |ui| self.top_bar(ui, &mut find_req));

        let mut out = Out { node_image: self.settings.node_shell_image.clone(), ..Default::default() };
        let accents = self.settings.accents.clone();
        let mut viewer = Viewer { out: &mut out, find_req, claimed: false, term_hovered: false, accents: &accents };
        if self.dock.iter_all_tabs().next().is_some() {
            let max = (ui.available_height() - 200.0).max(160.0);
            egui::Panel::bottom("dock").resizable(true).default_size(280.0).size_range(120.0..=max).frame(egui::Frame::new().fill(t.chrome)).show(ui, |ui| {
                let mut style = Style::from_egui(ui.style());
                style.tab_bar.bg_fill = t.chrome;
                style.tab_bar.hline_color = t.line;
                style.tab.tab_body.bg_fill = t.chrome;
                style.tab.tab_body.stroke = Stroke::NONE;
                for s in [&mut style.tab.active, &mut style.tab.focused, &mut style.tab.active_with_kb_focus, &mut style.tab.focused_with_kb_focus] {
                    s.bg_fill = t.bg;
                    s.text_color = t.text;
                }
                for s in [&mut style.tab.inactive, &mut style.tab.hovered, &mut style.tab.inactive_with_kb_focus] {
                    s.bg_fill = t.chrome;
                    s.text_color = t.muted;
                }
                DockArea::new(&mut self.dock).id(egui::Id::new("tool-dock")).style(style).show_leaf_collapse_buttons(false).show_inside(ui, &mut viewer);
            });
        }
        let (claimed, term_hovered) = (viewer.claimed, viewer.term_hovered);
        self.term_hovered = term_hovered;

        let first_new = out.tabs.len();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(t.bg)).show(ui, |ui| {
            let accent = self.clusters.get(self.cur).and_then(|x| x.cluster.clone()).and_then(|id| self.settings.accents.get(&id).copied());
            match self.clusters.get_mut(self.cur) {
                None => self.welcome(ui),
                Some(tab) => {
                    if let (Some(filter), false) = (find_req, claimed) {
                        tab.open_find(filter); // the pointer wasn't over the dock: the cluster view takes it
                    }
                    if let Some(c) = accent {
                        theme::tint(ui.visuals_mut(), c);
                    }
                    if let Body::Cluster(c) = &mut tab.body {
                        c.alias = self.settings.aliases.get(&c.kctx.id()).cloned();
                        c.ui(ui, &mut out);
                    }
                    // Logs, terminals and editors opened from a cluster carry its color.
                    let id = tab.cluster.clone();
                    out.tabs[first_new..].iter_mut().for_each(|n| n.cluster = id.clone());
                }
            }
        });
        if let (Some(filter), false, true) = (find_req, claimed, self.clusters.is_empty()) {
            if let Some((_, t)) = self.dock.find_active_focused() {
                t.open_find(filter);
            }
        }
        self.absorb(out);
        self.catalog_ui(&ctx);
        self.settings_ui(&ctx);
        self.custom_accent_ui(&ctx);
        self.toasts_ui(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // eframe has just stored the window placement; never keep a minimized one.
        match storage.get_string("window") {
            Some(w) if bad_window(&w) => {
                if let Some(good) = &self.good_window {
                    storage.set_string("window", good.clone());
                }
            }
            w => self.good_window = w.or(self.good_window.take()),
        }
        // Open clusters, in tab order, come back on the next start.
        self.settings.open = self.clusters.iter().filter_map(Self::cluster_of).map(|c| c.kctx.id()).collect();
        self.settings.current = self.clusters.get(self.cur).and_then(|t| t.cluster.clone());
        // Closed clusters keep their last tabs, so reopening one restores them too.
        for c in self.clusters.iter().filter_map(App::cluster_of) {
            if let Some(s) = c.saved() {
                self.settings.tabs.insert(c.kctx.id(), s);
            }
        }
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

fn main() -> eframe::Result {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    let _guard = rt.enter();
    std::thread::spawn(|| LazyLock::force(&cluster::HELM)); // probe helm off the UI thread
    let (tx, rx) = std::sync::mpsc::channel();
    tabs::TERM_TX.set(tx).expect("terminal channel set once");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KXS Watcher")
            .with_icon(std::sync::Arc::new(egui::IconData { rgba: icon::render(256), width: 256, height: 256 }))
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("KXS Watcher", options, Box::new(move |cc| Ok(Box::new(App::new(cc, rx)))))
}

use std::sync::LazyLock;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimized_window_placement_is_not_restored() {
        assert!(bad_window("(inner_position_pixels:Some((x:-32000.0,y:-32000.0)),maximized:false,inner_size_points:Some((x:0.0,y:0.0)))"));
        assert!(!bad_window("(inner_position_pixels:Some((x:112.0,y:135.0)),maximized:false,inner_size_points:Some((x:1440.0,y:900.0)))"));
    }

    #[test]
    fn accents_are_distinct_and_sticky() {
        let mut s = Settings::default();
        let (a, b) = (s.accent("qa"), s.accent("prod"));
        assert_ne!(a, b);
        assert_eq!(s.accent("qa"), a); // same cluster keeps its color
        s.accents.insert("qa".into(), ACCENTS[3]); // picked from the menu
        assert_eq!(s.accent("qa"), ACCENTS[3]);
        for i in 0..20 {
            s.accent(&format!("c{i}")); // more clusters than colors: reuse instead of panicking
        }
    }
}
