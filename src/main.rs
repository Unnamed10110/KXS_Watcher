#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window in release

mod cluster;
mod details;
mod dragout;
mod find;
mod icon;
mod kubeconfig;
mod keys;
mod list;
mod ops;
mod prof;
mod repaint;
mod search;
mod settings;
mod tab;
mod tabs;
mod tasks;
#[cfg(test)]
mod testkit;
mod theme;
mod topbar;
mod ui_kit;
mod units;
mod yamlhl;
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
use watch::{take, Lock, Pending, GREEN, RED};

pub use tab::{next_id, Body, Out, Tab};
use settings::{Docked, Settings, SettingsPage};

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
    /// A terminal on screen had the keyboard last frame: leave Ctrl+K/F/T/W/Tab to the shell.
    term_keys: bool,
    /// Custom accent window open for this context: (id, name).
    custom_accent: Option<(String, String)>,
    /// A cluster tab being renamed: (context id, text, focus requested).
    renaming: Option<ui_kit::Rename<String>>,
    /// Open file/folder dialog for kubeconfig sources.
    picking: Option<Pending<Vec<PathBuf>>>,
    /// Interface and monospace fonts in use.
    fonts: (String, String),
    /// The last window placement eframe saved while the window was on screen.
    good_window: Option<String>,
    /// Saved log tabs waiting for their cluster to connect.
    pending_docks: Vec<Docked>,
    /// Frame HUD and section timers (`KXS_HUD=1`, Ctrl+Shift+F12), see prof.rs.
    hud: prof::Hud,
    /// A text drag out of the window just ended: egui still thinks the button is down (OLE ate the
    /// release); the pointer was here when it started.
    drag_released: Option<Pos2>,
    /// Kubeconfig files and their modification times, checked every few seconds.
    kube_watch: kubeconfig::Watch,
}

/// eframe's saved window placement from while it was minimized (off screen, zero size):
/// restoring it opens the app where nobody can see it.
fn bad_window(saved: &str) -> bool {
    saved.contains("-32000") || saved.contains("inner_size_points:Some((x:0.0")
}

struct Viewer<'a> {
    out: &'a mut Out,
    /// Ctrl+K (`Some(false)`) / Ctrl+F (`Some(true)`) this frame: the tab under the pointer takes it.
    find_req: Option<bool>,
    claimed: bool,
    /// A terminal drawn this frame has the keyboard.
    term_keys: bool,
    /// The dock tab under the pointer (Ctrl+W closes it).
    hovered: Option<u64>,
    /// Tab title size (Settings › font size of tabs).
    tab_font: f32,
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
        let text = RichText::new(text).size(self.tab_font);
        match self.accent(t) {
            Some(c) => text.color(c).into(),
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
        self.term_keys |= matches!(&t.body, Body::Term(x) if x.has_keyboard());
        if hovered {
            self.hovered = Some(t.id);
        }
        if let (Some(filter), true, false) = (self.find_req, hovered, self.claimed) {
            self.claimed = t.open_find(filter);
        }
        if let Some(c) = self.accent(t) {
            theme::tint(ui.visuals_mut(), c);
        }
        match &mut t.body {
            Body::Cluster(c) => c.ui(ui, self.out),
            Body::Logs(l) => ui_kit::scaled(ui, ui_kit::Area::Logs, |ui| l.ui(ui)),
            Body::Term(x) => x.ui(ui, ui_kit::factor(ui_kit::Area::Terminal)),
            Body::Yaml(y) => ui_kit::scaled(ui, ui_kit::Area::Editor, |ui| y.ui(ui)),
        }
    }

    fn scroll_bars(&self, _: &Tab) -> [bool; 2] {
        [false, false]
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, term_rx: Receiver<(u64, PtyEvent)>) -> Self {
        prof::trace("App::new");
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let fonts = theme::install_fonts(&cc.egui_ctx, &settings.mono_font);
        keys::CTX.set(cc.egui_ctx.clone()).ok();
        cluster::probe_helm(&cc.egui_ctx); // off the UI thread
        settings.apply_look(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(settings.zoom);
        let window = cc.storage.and_then(|s| s.get_string("window"));
        if window.as_deref().is_some_and(bad_window) {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(1440.0, 900.0)));
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(80.0, 60.0)));
        }
        let mut app = App {
            contexts: kubeconfig::discover(&settings.paths),
            kube_watch: kubeconfig::Watch::new(&cc.egui_ctx, &settings.paths),
            pending_docks: vec![],
            hud: prof::Hud::new(),
            drag_released: None,
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
            term_keys: false,
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
        // Dock tabs of the last session: terminals now, logs once their cluster is connected.
        let mut out = Out::default();
        for d in std::mem::take(&mut app.settings.docked) {
            match &d {
                Docked::Terminal { context } => {
                    if let Some(c) = app.contexts.iter().find(|c| c.id() == *context) {
                        out.local_term(&cc.egui_ctx, c);
                        let open = app.open_ids().contains(context);
                        out.tabs.last_mut().filter(|_| open).into_iter().for_each(|t| t.cluster = Some(context.clone()));
                    }
                }
                Docked::Logs { .. } => app.pending_docks.push(d),
            }
        }
        app.absorb(out);
        prof::trace("App::new done");
        app
    }

    /// Reopens saved log tabs whose cluster has connected; drops those of clusters that are closed
    /// or couldn't connect.
    fn restore_docks(&mut self, ctx: &egui::Context) {
        let mut opened = vec![];
        for d in std::mem::take(&mut self.pending_docks) {
            let Docked::Logs { cluster, state } = &d else { continue };
            match self.clusters.iter().filter_map(Self::cluster_of).find(|c| c.kctx.id() == *cluster) {
                Some(c) if c.is_ready() => {
                    if let Some(t) = c.restore_logs(ctx, state) {
                        opened.push(Tab { id: next_id(), body: Body::Logs(Box::new(t)), cluster: Some(cluster.clone()) });
                    }
                }
                Some(c) if !c.failed() => self.pending_docks.push(d), // still connecting
                _ => {}
            }
        }
        for t in opened {
            self.place(t);
        }
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

    /// Kubeconfigs edited, added or removed on disk (kubectl config, other tools) update the
    /// cluster list; open clusters keep their connection.
    fn watch_kubeconfigs(&mut self) {
        self.kube_watch.set_paths(&self.settings.paths);
        if !self.kube_watch.changed() {
            return;
        }
        let before: HashSet<String> = self.contexts.iter().map(|c| c.id()).collect();
        self.contexts = kubeconfig::discover(&self.settings.paths);
        let after: HashSet<String> = self.contexts.iter().map(|c| c.id()).collect();
        let (added, removed) = (after.difference(&before).count(), before.difference(&after).count());
        if added + removed > 0 {
            let change = [(added, "+"), (removed, "−")].iter().filter(|(n, _)| *n > 0).map(|(n, s)| format!("{s}{n}")).collect::<Vec<_>>().join(" ");
            self.toasts.push((format!("Kubeconfig changed: {} contexts ({change})", self.contexts.len()), false, Instant::now()));
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

}

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let typed = std::mem::take(&mut *keys::TYPED.locked());
        if !typed.is_empty() {
            raw_input.events.push(egui::Event::Text(typed));
        }
        if let Some(pos) = self.drag_released.take() {
            raw_input.events.splice(0..0, [egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }, egui::Event::PointerGone]);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        repaint::begin_frame(&ctx);
        self.hud.begin(&ctx);
        static FIRST_FRAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        prof::trace_once(&FIRST_FRAME, "first frame");
        ui_kit::set_sizes(self.settings.sizes);
        tabs::set_scrollback(self.settings.terminal_scrollback);
        // First thing: text fields would treat Ctrl+K as "delete to end of line".
        const FIND_K: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);
        const FIND_F: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::F);
        const NEW_TAB: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::T);
        let mut find_req = match self.term_keys {
            true => None,
            false => ui.input_mut(|i| if i.consume_shortcut(&FIND_K) { Some(false) } else { i.consume_shortcut(&FIND_F).then_some(true) }),
        };
        // Ctrl+T: a new tab in the current cluster; with none open, the cluster list.
        if !self.term_keys && ui.input_mut(|i| i.consume_shortcut(&NEW_TAB)) {
            match self.clusters.get_mut(self.cur).map(|t| &mut t.body) {
                Some(Body::Cluster(c)) => c.new_tab(),
                _ => self.show_catalog = true,
            }
        }
        // Ctrl+W (after the dock is drawn: it may be the one under the pointer), Ctrl+Tab /
        // Ctrl+Shift+Tab for the next / previous tab, Ctrl+1…8 for cluster N and Ctrl+9 for the last.
        let mut close_req = false;
        if !self.term_keys {
            use egui::{Key, Modifiers};
            const NUMS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            let (close, step, num) = ui.input_mut(|i| {
                let close = i.consume_key(Modifiers::COMMAND, Key::W);
                let step = if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Tab) { -1 } else { i.consume_key(Modifiers::COMMAND, Key::Tab) as isize };
                (close, step, NUMS.iter().position(|k| i.consume_key(Modifiers::COMMAND, *k)))
            });
            close_req = close;
            if step != 0
                && let Some(Body::Cluster(c)) = self.clusters.get_mut(self.cur).map(|t| &mut t.body)
            {
                c.step_tab(step);
            }
            if let (Some(n), false) = (num, self.clusters.is_empty()) {
                self.cur = if n == 8 { self.clusters.len() - 1 } else { n.min(self.clusters.len() - 1) };
            }
        }
        self.watch_kubeconfigs();
        if !self.pending_docks.is_empty() {
            self.restore_docks(&ctx);
        }
        if let Some(picked) = take(&mut self.picking).filter(|p| !p.is_empty()) {
            self.add_sources(picked);
        }
        while let Ok((id, ev)) = self.term_rx.try_recv() {
            if let PtyEvent::Exit = ev
                && let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
                    self.dock.remove_tab(path);
                }
        }

        let t = ui_kit::tokens(ui);
        egui::Panel::top("topbar").exact_size(48.0).frame(egui::Frame::new().fill(t.chrome).stroke(Stroke::new(1.0, t.line))).show(ui, |ui| self.top_bar(ui, &mut find_req));

        let mut out = Out { node_image: self.settings.node_shell_image.clone(), ..Default::default() };
        let accents = self.settings.accents.clone();
        let tab_font = (self.settings.text_size + 1.5) * ui_kit::factor(ui_kit::Area::Tabs);
        // The details of the clicked row are a column on the right of the whole window, drawn before the
        // dock: the dock (logs) gets narrower instead of running under it.
        let first_new = out.tabs.len();
        let accent = self.clusters.get(self.cur).and_then(|x| x.cluster.as_ref()).and_then(|id| accents.get(id).copied());
        if let Some(Body::Cluster(c)) = self.clusters.get_mut(self.cur).map(|t| &mut t.body) {
            c.details_panel(ui, &mut out, accent);
        }
        let mut viewer = Viewer { out: &mut out, find_req, claimed: false, term_keys: false, hovered: None, tab_font, accents: &accents };
        let mut dock_hovered = false;
        if self.dock.iter_all_tabs().next().is_some() {
            let max = (ui.available_height() - 200.0).max(160.0);
            dock_hovered = egui::Panel::bottom("dock").resizable(true).default_size(280.0).size_range(120.0..=max).frame(egui::Frame::new().fill(t.chrome)).show(ui, |ui| {
                let mut style = Style::from_egui(ui.style());
                style.tab_bar.bg_fill = t.chrome;
                style.tab_bar.height = (40.0 * ui_kit::factor(ui_kit::Area::Tabs)).max(28.0);
                style.tab.hline_below_active_tab_name = true;
                style.tab.minimum_width = Some(120.0);
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
            })
            .response
            .contains_pointer();
        }
        let (claimed, term_keys, hovered_tab) = (viewer.claimed, viewer.term_keys, viewer.hovered);
        self.term_keys = term_keys;
        // Ctrl+W: the dock tab under the pointer (or the focused one), else the cluster's current tab.
        if close_req && dock_hovered {
            if let Some(path) = hovered_tab.or_else(|| self.dock.find_active_focused().map(|(_, t)| t.id)).and_then(|id| self.dock.find_tab_from(|t| t.id == id)) {
                self.dock.remove_tab(path);
            }
        } else if close_req && let Some(Body::Cluster(c)) = self.clusters.get_mut(self.cur).map(|t| &mut t.body) {
            c.close_tab();
        }

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
        if let (Some(filter), false, true) = (find_req, claimed, self.clusters.is_empty())
            && let Some((_, t)) = self.dock.find_active_focused() {
                t.open_find(filter);
            }
        self.absorb(out);
        self.catalog_ui(&ctx);
        self.settings_ui(&ctx);
        self.custom_accent_ui(&ctx);
        self.toasts_ui(&ctx);
        self.hud.end(&ctx);
        // A selection was dragged out of the window: block here until it is dropped (or not).
        if let Some(text) = ctx.kxs_take_drag_out() {
            let from = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.latest_pos()));
            dragout::start(&text);
            self.drag_released = from;
            ctx.request_repaint();
        }
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
        let mut docked: Vec<Docked> = self
            .dock
            .iter_all_tabs()
            .filter_map(|(_, t)| match &t.body {
                Body::Logs(l) => t.cluster.clone().map(|cluster| Docked::Logs { cluster, state: l.state() }),
                Body::Term(x) => x.local.clone().map(|context| Docked::Terminal { context }),
                _ => None,
            })
            .collect();
        docked.extend(self.pending_docks.iter().cloned()); // not reopened yet: keep them
        self.settings.docked = docked;
        // Closed clusters keep their last tabs, so reopening one restores them too.
        for c in self.clusters.iter().filter_map(App::cluster_of) {
            if let Some(s) = c.saved() {
                self.settings.tabs.insert(c.kctx.id(), s);
            }
        }
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

// Nothing can run without the async runtime and the terminal channel: stopping at start is right.
#[allow(clippy::expect_used)]
fn main() -> eframe::Result {
    prof::start();
    prof::trace("main");
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    let _guard = rt.enter();
    let (tx, rx) = std::sync::mpsc::channel();
    tabs::TERM_TX.set(tx).expect("terminal channel set once");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KXS Watcher")
            .with_icon(std::sync::Arc::new(egui::IconData { rgba: icon::render(128), width: 128, height: 128 }))
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 560.0])
            // The app takes no dropped files, and winit's drop target aborts if OLE asks it for more
            // than files while we drag text over our own window (see dragout.rs).
            .with_drag_and_drop(false),
        event_loop_builder: Some(Box::new(|b| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            b.with_msg_hook(keys::hook);
        })),
        ..Default::default()
    };
    prof::trace("run_native");
    eframe::run_native("KXS Watcher", options, Box::new(move |cc| Ok(Box::new(App::new(cc, rx)))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ACCENTS;

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
