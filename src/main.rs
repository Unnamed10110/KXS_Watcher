#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window in release

mod cluster;
mod details;
mod find;
mod kubeconfig;
mod list;
mod ops;
mod search;
mod tabs;
mod watch;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};
use egui_dock::{DockArea, DockState, NodeIndex, Style, TabStyle, TabViewer};
use egui_term::PtyEvent;
use serde::{Deserialize, Serialize};

use cluster::ClusterTab;
use kubeconfig::Ctx;
use tabs::{LogTab, TermTab, YamlTab};
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
    theme: egui::ThemePreference,
    pinned: Vec<String>,
    open: Vec<String>,
    zoom: f32,
    /// Image for node-shell pods (needs `sleep`; nsenter runs the node's own shell).
    node_shell_image: String,
    /// Open page and object tabs per cluster (context id), restored on reconnect.
    tabs: HashMap<String, cluster::SavedTabs>,
    /// Accent color per cluster (context id), handed out from `ACCENTS` on first use.
    accents: HashMap<String, Color32>,
}

/// Accent colors for clusters, readable on dark and light themes. Sky is late: it's
/// close to egui's own selection blue. Rose is last, so red stays a deliberate pick (prod).
const ACCENTS: [Color32; 10] = [
    Color32::from_rgb(16, 185, 129),  // emerald
    Color32::from_rgb(139, 92, 246),  // violet
    Color32::from_rgb(245, 158, 11),  // amber
    Color32::from_rgb(236, 72, 153),  // pink
    Color32::from_rgb(20, 184, 166),  // teal
    Color32::from_rgb(132, 204, 22),  // lime
    Color32::from_rgb(249, 115, 22),  // orange
    Color32::from_rgb(14, 165, 233),  // sky
    Color32::from_rgb(99, 102, 241),  // indigo
    Color32::from_rgb(244, 63, 94),   // rose
];

impl Settings {
    /// The cluster's accent; a new cluster gets the first color no other cluster uses.
    fn accent(&mut self, id: &str) -> Color32 {
        let n = self.accents.len();
        let free = ACCENTS.into_iter().find(|c| !self.accents.values().any(|u| u == c));
        *self.accents.entry(id.to_string()).or_insert_with(|| free.unwrap_or(ACCENTS[n % ACCENTS.len()]))
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings { paths: vec![kubeconfig::default_dir()], theme: egui::ThemePreference::Dark, pinned: vec![], open: vec![], zoom: 1.0, node_shell_image: "busybox:1.36".into(), tabs: HashMap::new(), accents: HashMap::new() }
    }
}

struct App {
    settings: Settings,
    contexts: Vec<Ctx>,
    dock: DockState<Tab>,
    term_rx: Receiver<(u64, PtyEvent)>,
    toasts: Vec<(String, bool, Instant)>,
    filter: String,
    sidebar: bool,
    show_settings: bool,
    paths_text: String,
    /// Pointer was over a terminal last frame: leave Ctrl+K/F to the shell.
    term_hovered: bool,
    /// Custom accent window open for this context: (id, name).
    custom_accent: Option<(String, String)>,
    /// Open file/folder dialog for kubeconfig sources.
    picking: Option<Pending<Vec<PathBuf>>>,
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

    /// The cluster's accent outlines its tabs and frames the open one.
    fn tab_style_override(&self, t: &Tab, global: &TabStyle) -> Option<TabStyle> {
        let c = self.accent(t)?;
        let mut s = global.clone();
        for i in [&mut s.active, &mut s.focused, &mut s.active_with_kb_focus, &mut s.focused_with_kb_focus] {
            i.outline_color = c;
        }
        s.tab_body.stroke = egui::Stroke::new(1.0, c);
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
            let v = ui.visuals_mut(); // selections (rows, nav, tabs) take the cluster's accent
            v.selection.bg_fill = c.gamma_multiply(if v.dark_mode { 0.55 } else { 0.35 });
        }
        let first_new = self.out.tabs.len();
        match &mut t.body {
            Body::Cluster(c) => {
                c.ui(ui, self.out);
                // Logs, terminals and editors opened from a cluster carry its accent.
                self.out.tabs[first_new..].iter_mut().for_each(|n| n.cluster = t.cluster.clone());
            }
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
        cc.egui_ctx.set_theme(settings.theme);
        cc.egui_ctx.set_zoom_factor(settings.zoom);
        let mut app = App {
            contexts: kubeconfig::discover(&settings.paths),
            paths_text: settings.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"),
            settings,
            dock: DockState::new(vec![]),
            term_rx,
            toasts: vec![],
            filter: String::new(),
            sidebar: true,
            show_settings: false,
            term_hovered: false,
            custom_accent: None,
            picking: None,
        };
        for id in app.settings.open.clone() {
            if let Some(c) = app.contexts.iter().find(|c| c.id() == id).cloned() {
                app.open_cluster(&cc.egui_ctx, c);
            }
        }
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

    fn open_ids(&self) -> HashSet<String> {
        self.dock
            .iter_all_tabs()
            .filter_map(|(_, t)| match &t.body {
                Body::Cluster(c) => Some(c.kctx.id()),
                _ => None,
            })
            .collect()
    }

    fn open_cluster(&mut self, ctx: &egui::Context, c: Ctx) {
        let id = c.id();
        if let Some(path) = self.dock.find_tab_from(|t| matches!(&t.body, Body::Cluster(x) if x.kctx.id() == id)) {
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(path.node_path());
            return;
        }
        let saved = self.settings.tabs.get(&id).cloned();
        self.settings.accent(&id);
        self.place(Tab { id: next_id(), body: Body::Cluster(Box::new(ClusterTab::new(ctx, c, saved))), cluster: Some(id) });
    }

    /// Clusters share the top area; logs / terminals / editors share a bottom dock, like Lens.
    fn place(&mut self, tab: Tab) {
        let cluster = matches!(tab.body, Body::Cluster(_));
        if let Some(path) = self.dock.find_tab_from(|t| matches!(t.body, Body::Cluster(_)) == cluster) {
            if let Ok(leaf) = self.dock.leaf_mut(path.node_path()) {
                leaf.append_tab(tab);
                // New tabs are the "current window" for Ctrl+K until the user clicks elsewhere.
                return self.dock.set_focused_node_and_surface(path.node_path());
            }
        }
        let tree = self.dock.main_surface_mut();
        // A fresh DockState holds one empty leaf, so test for tabs, not nodes.
        if tree.tabs().next().is_none() {
            self.dock.push_to_first_leaf(tab);
            return;
        }
        let [_, new] = if cluster { tree.split_above(NodeIndex::root(), 0.62, vec![tab]) } else { tree.split_below(NodeIndex::root(), 0.62, vec![tab]) };
        self.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: egui_dock::SurfaceIndex::main(), node: new });
    }

    fn catalog(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Clusters");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⚙").on_hover_text("Settings").clicked() {
                    self.show_settings = true;
                }
                if ui.small_button("⟳").on_hover_text("Rescan kubeconfig files").clicked() {
                    self.rescan();
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.button("➕ Add kubeconfig…").on_hover_text("Pick kubeconfig files on disk").clicked() {
                self.pick_kubeconfigs(&ctx, false);
            }
            if ui.button("📂 Folder…").on_hover_text("Add a folder: every kubeconfig in it (one level deep)").clicked() {
                self.pick_kubeconfigs(&ctx, true);
            }
            if self.picking.is_some() {
                ui.spinner();
            }
        });
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter contexts…").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let open = self.open_ids();
        let mut to_open = None;
        let mut toggle_pin = None;
        let mut terminal = None;
        let mut recolor = None;
        let mut custom = None;
        let visible: Vec<&Ctx> = self.contexts.iter().filter(|c| watch::contains_ci(&c.name, &self.filter) || watch::contains_ci(&c.server, &self.filter)).collect();
        let pinned: Vec<&Ctx> = visible.iter().copied().filter(|c| self.settings.pinned.contains(&c.id())).collect();
        let mut files: Vec<&PathBuf> = visible.iter().map(|c| &c.file).collect();
        files.dedup();

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut entry = |ui: &mut Ui, c: &Ctx| {
                let is_open = open.contains(&c.id());
                let is_pinned = self.settings.pinned.contains(&c.id());
                let r = ui
                    .horizontal(|ui| {
                        let accent = self.settings.accents.get(&c.id()).copied();
                        watch::dot(ui, accent.unwrap_or(ui.visuals().weak_text_color()));
                        ui.selectable_label(is_open, &c.name)
                    })
                    .inner
                    .on_hover_text(format!("{}\n{}", c.server, c.file.display()));
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
                    ui.label(RichText::new("Accent color").weak());
                    let cur = self.settings.accents.get(&c.id()).copied();
                    ui.horizontal(|ui| {
                        for col in ACCENTS {
                            let ring = if cur == Some(col) { egui::Stroke::new(2.0, ui.visuals().strong_text_color()) } else { egui::Stroke::NONE };
                            if ui.add(egui::Button::new("").fill(col).stroke(ring).min_size(egui::vec2(18.0, 18.0))).clicked() {
                                recolor = Some((c.id(), col));
                                ui.close();
                            }
                        }
                    });
                    // A window, not a submenu: context menus close on any click, even inside a picker.
                    if ui.button("🎨 Custom color…").clicked() {
                        custom = Some((c.id(), c.name.clone()));
                        ui.close();
                    }
                });
            };
            if !pinned.is_empty() {
                ui.label(RichText::new("📌 Pinned").weak());
                for c in &pinned {
                    entry(ui, c);
                }
                ui.separator();
            }
            for f in files {
                let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                egui::CollapsingHeader::new(RichText::new(&name).weak()).id_salt(f).default_open(true).show(ui, |ui| {
                    for c in visible.iter().filter(|c| &c.file == f) {
                        entry(ui, c);
                    }
                })
                .header_response
                .on_hover_text(f.display().to_string());
            }
            if self.contexts.is_empty() {
                ui.label("No contexts found. Add kubeconfig files or folders in ⚙ Settings.");
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
        if let Some(c) = to_open {
            self.open_cluster(&ctx, c);
        }
        if let Some(c) = terminal {
            let mut out = Out::default();
            out.term(&ctx, format!("Terminal {}", c.name), tabs::local_shell(&c), None);
            self.settings.accent(&c.id());
            out.tabs.iter_mut().for_each(|t| t.cluster = Some(c.id()));
            self.absorb(out);
        }
    }

    fn absorb(&mut self, out: Out) {
        for t in out.tabs {
            self.place(t);
        }
        let now = Instant::now();
        self.toasts.extend(out.toasts.into_iter().map(|(m, e)| (m, e, now)));
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 4.0);
            ui.label(RichText::new("☸").size(64.0));
            ui.heading("KXS Watcher");
            ui.label(RichText::new("Fast, native Kubernetes & k3s manager").weak());
            ui.add_space(12.0);
            let files: HashSet<&PathBuf> = self.contexts.iter().map(|c| &c.file).collect();
            ui.label(format!("{} contexts found in {} kubeconfig files.", self.contexts.len(), files.len()));
            ui.label("Pick a cluster on the left to connect; open several at once as tabs.");
            if ui.button("➕ Add kubeconfig files…").clicked() {
                self.pick_kubeconfigs(ui.ctx(), false);
            }
            if ui.button("⚙ Kubeconfig settings").clicked() {
                self.show_settings = true;
            }
        });
    }

    fn settings_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let (mut rescan, mut pick) = (false, None);
        egui::Window::new("Settings").open(&mut open).collapsible(false).default_width(560.0).show(ctx, |ui| {
            ui.label(RichText::new("Kubeconfig sources").strong());
            ui.label(RichText::new("One file or folder per line. ~/.kube/config and $KUBECONFIG are always included; folders are scanned one level deep.").weak());
            ui.add(egui::TextEdit::multiline(&mut self.paths_text).desired_rows(5).desired_width(f32::INFINITY).code_editor());
            ui.horizontal(|ui| {
                if ui.button("💾 Save & rescan").clicked() {
                    self.settings.paths = self.paths_text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect();
                    rescan = true;
                }
                if ui.button("➕ Add files…").on_hover_text("Pick kubeconfig files on disk").clicked() {
                    pick = Some(false);
                }
                if ui.button("📂 Add folder…").on_hover_text("Every kubeconfig in the folder (one level deep)").clicked() {
                    pick = Some(true);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Theme").strong());
                for (t, name) in [(egui::ThemePreference::Dark, "Dark"), (egui::ThemePreference::Light, "Light"), (egui::ThemePreference::System, "System")] {
                    if ui.selectable_value(&mut self.settings.theme, t, name).changed() {
                        ctx.set_theme(t);
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Zoom").strong());
                if ui.add(egui::Slider::new(&mut self.settings.zoom, 0.7..=2.0).step_by(0.05)).changed() {
                    ctx.set_zoom_factor(self.settings.zoom);
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Node shell image").strong()).on_hover_text("Used for node terminals: a privileged pod with this image runs on the node while the terminal is open");
                ui.add(egui::TextEdit::singleline(&mut self.settings.node_shell_image).desired_width(220.0));
            });
            ui.separator();
            ui.label(RichText::new(format!("Helm CLI: {}", if *cluster::HELM { "found" } else { "not found on PATH (Helm view hidden)" })).weak());
        });
        self.show_settings = open;
        if rescan {
            self.rescan();
        }
        if let Some(folder) = pick {
            self.pick_kubeconfigs(ctx, folder);
        }
    }

    /// Any color for a cluster; changes apply live.
    fn custom_accent_ui(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.custom_accent.clone() else { return };
        let (mut open, mut done) = (true, false);
        egui::Window::new(format!("Accent color · {name}")).id(egui::Id::new("custom-accent")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let mut col = self.settings.accent(&id);
            if egui::color_picker::color_picker_color32(ui, &mut col, egui::color_picker::Alpha::Opaque) {
                self.settings.accents.insert(id.clone(), col);
            }
            ui.add_space(4.0);
            done = ui.button("Done").clicked();
        });
        if !open || done {
            self.custom_accent = None;
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.2.elapsed() < Duration::from_secs(if t.1 { 10 } else { 5 }));
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::RIGHT_BOTTOM, [-14.0, -14.0]).order(egui::Order::Foreground).show(ctx, |ui| {
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // First thing: text fields would treat Ctrl+K as "delete to end of line".
        const FIND_K: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);
        const FIND_F: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::F);
        let find_req = match self.term_hovered {
            true => None,
            false => ui.input_mut(|i| if i.consume_shortcut(&FIND_K) { Some(false) } else { i.consume_shortcut(&FIND_F).then_some(true) }),
        };
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

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("➕ Add kubeconfig files…").clicked() {
                        self.pick_kubeconfigs(&ctx, false);
                    }
                    if ui.button("📂 Add kubeconfig folder…").clicked() {
                        self.pick_kubeconfigs(&ctx, true);
                    }
                    if ui.button("⚙ Settings…").clicked() {
                        self.show_settings = true;
                    }
                    if ui.button("⟳ Rescan kubeconfigs").clicked() {
                        self.rescan();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.sidebar, "Cluster catalog");
                    for (t, name) in [(egui::ThemePreference::Dark, "Dark theme"), (egui::ThemePreference::Light, "Light theme"), (egui::ThemePreference::System, "System theme")] {
                        if ui.selectable_value(&mut self.settings.theme, t, name).clicked() {
                            ctx.set_theme(t);
                        }
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{} contexts", self.contexts.len())).weak());
                });
            });
        });

        if self.sidebar {
            egui::Panel::left("catalog").resizable(true).default_size(240.0).size_range(160.0..=480.0).show(ui, |ui| self.catalog(ui));
        }

        let mut out = Out { node_image: self.settings.node_shell_image.clone(), ..Default::default() };
        let accents = self.settings.accents.clone(); // a few entries; `welcome` needs `&mut self`
        let mut viewer = Viewer { out: &mut out, find_req, claimed: false, term_hovered: false, accents: &accents };
        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.dock.iter_all_tabs().next().is_none() {
                self.welcome(ui);
            } else {
                DockArea::new(&mut self.dock).style(Style::from_egui(ui.style())).show_leaf_collapse_buttons(false).show_inside(ui, &mut viewer);
            }
        });
        let (claimed, term_hovered) = (viewer.claimed, viewer.term_hovered);
        self.term_hovered = term_hovered;
        if let (Some(filter), false) = (find_req, claimed) {
            if let Some((_, t)) = self.dock.find_active_focused() {
                t.open_find(filter); // pointer wasn't over a tab: the focused one ("current window")
            }
        }
        self.absorb(out);
        self.settings_ui(&ctx);
        self.custom_accent_ui(&ctx);
        self.toasts_ui(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let mut open: Vec<String> = self.open_ids().into_iter().collect();
        open.sort();
        self.settings.open = open;
        // Closed clusters keep their last tabs, so reopening one restores them too.
        for (_, t) in self.dock.iter_all_tabs() {
            if let Body::Cluster(c) = &t.body {
                if let Some(s) = c.saved() {
                    self.settings.tabs.insert(c.kctx.id(), s);
                }
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
        viewport: egui::ViewportBuilder::default().with_title("KXS Watcher").with_inner_size([1440.0, 900.0]).with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("KXS Watcher", options, Box::new(move |cc| Ok(Box::new(App::new(cc, rx)))))
}

use std::sync::LazyLock;

#[cfg(test)]
mod tests {
    use super::*;

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
