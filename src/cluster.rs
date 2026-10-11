//! One cluster tab: connect + discovery, sidebar navigation, resource lists with object sub-tabs,
//! overview pages, Helm releases and port forwards.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use kube::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::details::{self, Act, Details, Target};
use crate::find::Find;
use crate::kubeconfig::{self, Ctx};
use crate::list::{List, Pick};
use crate::ops::{self, fmt_bytes, fmt_cpu, Forward, Kind, Metrics, Res};
use crate::search::ContentSearch;
use crate::theme::mix;
use crate::ui_kit::{self, Btn, Icon};
use crate::tabs::{self, LogTab, YamlTab};
use crate::watch::{self, cell_color, take, Bg, Lock, ListData, Pending, Row, Shared, GREEN, ORANGE, RED};
use crate::{next_id, Body, Out};

mod actions;
mod forwards;
mod helm;
mod listing;
mod overview;
mod pages;
mod sidebar;
mod tabbar;
#[cfg(test)]
mod tests;

use actions::*;
use helm::*;
use pages::*;
use sidebar::*;
pub use pages::SavedTabs;

/// Whether `helm` is installed: probed on a thread at start (`probe_helm`), false until it is known.
static HELM: OnceLock<bool> = OnceLock::new();

pub fn has_helm() -> bool {
    HELM.get().copied().unwrap_or(false)
}

pub fn probe_helm(ctx: &egui::Context) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        HELM.set(ops::tool_available("helm")).ok();
        ctx.request_repaint(); // the Helm page appears
    });
}

type Job = Pin<Box<dyn Future<Output = Res> + Send>>;

pub fn label(kind: &Kind) -> String {
    for (_, entries) in NAV {
        if let Some((_, _, l)) = entries.iter().find(|(g, k, _)| kind.is(g, k)) {
            return l.to_string();
        }
    }
    match kind.ar.kind.as_str() {
        "Node" | "Namespace" | "Event" if kind.ar.group.is_empty() => format!("{}s", kind.ar.kind),
        "CustomResourceDefinition" => "Custom Resource Definitions".into(),
        k => k.into(),
    }
}

struct Conn {
    client: Client,
    kinds: Vec<Kind>,
    version: String,
    crds: Option<HashSet<String>>,
}

async fn connect(k: Ctx) -> Res<Conn> {
    let r: anyhow::Result<Conn> = async {
        crate::prof::trace("connect: start");
        let client = kubeconfig::client(&k).await?;
        crate::prof::trace("connect: client built");
        // Three independent requests: one round trip's time, not three. CRD names are
        // "<plural>.<group>"; a failure only means the custom kinds are not told apart.
        let (version, kinds, crds) = tokio::join!(
            async {
                let v = client.apiserver_version().await?.git_version;
                crate::prof::trace("connect: version");
                anyhow::Ok(v)
            },
            async {
                let k = ops::discover(&client).await?;
                crate::prof::trace("connect: discovery");
                anyhow::Ok(k)
            },
            async {
                let c = watch::names(&client, "/apis/apiextensions.k8s.io/v1/customresourcedefinitions").await.ok().map(|n| n.into_iter().collect::<HashSet<String>>());
                crate::prof::trace("connect: CRD names");
                c
            },
        );
        Ok(Conn { client, kinds: kinds?, version: version?, crds })
    }
    .await;
    r.map_err(|e| ops::err_text(&e))
}

pub struct ClusterTab {
    pub kctx: Ctx,
    /// Name given to the cluster (tab rename); the context name when `None`.
    pub alias: Option<String>,
    conn: Option<Pending<Res<Conn>>>,
    error: Option<String>,
    ready: Option<Box<Ready>>,
    /// Saved tabs to reopen once connected.
    restore: Option<SavedTabs>,
}

impl ClusterTab {
    pub fn new(ctx: &egui::Context, kctx: Ctx, restore: Option<SavedTabs>) -> Self {
        let conn = Some(Pending::spawn(ctx, connect(kctx.clone())));
        ClusterTab { kctx, alias: None, conn, error: None, ready: None, restore }
    }

    /// Open tabs to save; the pending restore while still connecting.
    pub fn saved(&self) -> Option<SavedTabs> {
        match &self.ready {
            Some(r) => Some(r.saved()),
            None => self.restore.clone(),
        }
    }

    /// Distinct warning events (repeats grouped), for the top bar's bell.
    pub fn warning_count(&self) -> Option<usize> {
        self.ready.as_ref()?.warnings.as_ref().map(|l| l.grouped_len())
    }

    /// Ctrl+T: a new tab.
    pub fn new_tab(&mut self) {
        if let Some(r) = &mut self.ready {
            r.new_tab();
        }
    }

    /// Connected: a saved log tab can be reopened (`restore_logs`).
    pub fn is_ready(&self) -> bool {
        self.ready.is_some()
    }

    /// Couldn't connect: saved log tabs of this cluster are dropped.
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// A log tab saved at exit, reopened; `None` when its kinds are gone from the cluster.
    pub fn restore_logs(&self, ctx: &egui::Context, s: &tabs::LogState) -> Option<LogTab> {
        let r = self.ready.as_ref()?;
        let targets: Vec<(Kind, String, String)> = s.targets.iter().filter_map(|(g, k, ns, name)| Some((r.kind(g, k)?, ns.clone(), name.clone()))).collect();
        if targets.is_empty() {
            return None;
        }
        let mut t = LogTab::new(ctx, r.client.clone(), targets, (!s.container.is_empty()).then(|| s.container.clone()));
        t.restore(s);
        Some(t)
    }

    /// Ctrl+W: close the object tab shown, else the page tab.
    pub fn close_tab(&mut self) {
        if let Some(r) = &mut self.ready {
            r.close_tab();
        }
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab: the next / previous page tab.
    pub fn step_tab(&mut self, by: isize) {
        if let Some(r) = self.ready.as_mut().filter(|r| !r.pages.is_empty()) {
            r.cur = (r.cur as isize + by).rem_euclid(r.pages.len() as isize) as usize;
        }
    }

    /// The bell: show this cluster's overview.
    pub fn show_overview(&mut self, ctx: &egui::Context) {
        if let Some(r) = &mut self.ready {
            r.show_page(ctx, Page::Overview, false);
        }
    }

    /// Ctrl+K: find in the view. Ctrl+F (`filter`): focus the list's filter box (find elsewhere).
    pub fn open_find(&mut self, filter: bool) {
        if let Some(r) = &mut self.ready {
            let page = r.pages.get_mut(r.cur).filter(|p| p.active == 0);
            let on_list = page.as_ref().is_some_and(|p| matches!(p.page, Page::Kind(..)));
            let panel_open = page.as_ref().is_some_and(|p| p.panel.is_some());
            if r.panel_hovered && panel_open {
                r.panel_find.open(); // over the details panel: find in it (keys, values, YAML)
            } else if page.as_ref().is_some_and(|p| p.page == Page::Launcher) {
                r.focus_launcher = true;
            } else if let Some(cs) = page.and_then(|p| p.content.as_mut()) {
                cs.focus = true; // the search page's own query box
            } else if filter && on_list {
                r.focus_filter = true;
            } else {
                r.find.open();
            }
        }
    }

    /// The details of the clicked row as a column on the right of the whole window, drawn before the
    /// bottom dock so the dock (logs) is narrower instead of running under it.
    pub fn details_panel(&mut self, ui: &mut Ui, out: &mut Out, accent: Option<egui::Color32>) {
        if let Some(r) = &mut self.ready {
            r.details_panel(ui, out, accent);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, out: &mut Out) {
        if let Some(r) = take(&mut self.conn) {
            match r {
                Ok(c) => self.ready = Some(Box::new(Ready::new(ui.ctx(), self.kctx.clone(), c, self.restore.take()))),
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(r) = &mut self.ready {
            r.alias = self.alias.clone();
            return r.ui(ui, out);
        }
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 3.0);
            match &self.error {
                None => {
                    crate::ui_kit::spinner(ui);
                    ui.label(format!("Connecting to {} …", self.kctx.name));
                    ui.label(RichText::new(&self.kctx.server).weak());
                }
                Some(e) => {
                    ui.colored_label(RED, format!("Cannot connect to {}", self.kctx.name));
                    ui.add(egui::Label::new(e).wrap());
                    if ui.button("⟳ Retry").clicked() {
                        self.error = None;
                        self.conn = Some(Pending::spawn(ui.ctx(), connect(self.kctx.clone())));
                    }
                }
            }
        });
    }
}

struct Ready {
    kctx: Ctx,
    client: Client,
    kinds: Arc<Vec<Kind>>,
    custom: BTreeMap<String, Vec<Kind>>,
    version: String,
    has_metrics: bool,
    namespaces: Shared,
    _ns_watch: Option<Bg>,
    ns_sel: BTreeSet<String>,
    ns_input: String,
    wide: bool,
    pages: Vec<PageTab>,
    cur: usize,
    /// Warning events, always watched: the bell, the Events badge and the overview.
    warnings: Option<List>,
    /// Objects per (group, kind) for the sidebar, refreshed in the background.
    counts: Arc<Mutex<HashMap<(String, String), usize>>>,
    _counts_bg: Option<Bg>,
    panel_find: Find,
    /// The pointer was over the details panel last frame: Ctrl+F / Ctrl+K find in it.
    panel_hovered: bool,
    find: Find,
    /// Ctrl+F: focus the filter box next frame.
    focus_filter: bool,
    /// Focus the new tab's filter next frame.
    focus_launcher: bool,
    /// A page tab being renamed: (tab id, text, focus requested).
    renaming: Option<ui_kit::Rename<u64>>,
    /// Name given to the cluster, shown in its sidebar card and overview.
    alias: Option<String>,
    /// Node shells being started: (node, pending (namespace, pod)).
    node_shells: Vec<(String, Pending<Res<(String, String)>>)>,
    scale_to: i64,
    metrics: Arc<Mutex<Metrics>>,
    metrics_bg: Option<Bg>,
    helm: Helm,
    forwards: Vec<Forward>,
    actions: Vec<Pending<Res>>,
    confirm: Option<(String, Confirm)>,
}

impl Ready {
    fn new(ctx: &egui::Context, kctx: Ctx, c: Conn, restore: Option<SavedTabs>) -> Self {
        crate::prof::trace("cluster ready");
        let mut custom: BTreeMap<String, Vec<Kind>> = BTreeMap::new();
        for k in &c.kinds {
            let is_custom = match &c.crds {
                Some(names) => names.contains(&format!("{}.{}", k.ar.plural, k.ar.group)),
                None => !BUILTIN_GROUPS.contains(&k.ar.group.as_str()),
            };
            if is_custom {
                custom.entry(k.ar.group.clone()).or_default().push(k.clone());
            }
        }
        custom.values_mut().for_each(|v| v.sort_by(|a, b| a.ar.kind.cmp(&b.ar.kind)));
        let namespaces: Shared = Default::default();
        let ns_kind = c.kinds.iter().find(|k| k.is("", "Namespace")).cloned();
        let ns_watch = ns_kind.map(|k| Bg::spawn(watch::run(c.client.clone(), k.ar, None, None, namespaces.clone(), ctx.clone())));
        let mut r = Ready {
            has_metrics: c.kinds.iter().any(|k| k.ar.group == "metrics.k8s.io"),
            kctx,
            client: c.client,
            kinds: Arc::new(c.kinds),
            custom,
            version: c.version,
            namespaces,
            _ns_watch: ns_watch,
            ns_sel: BTreeSet::new(),
            ns_input: String::new(),
            wide: false,
            pages: vec![],
            cur: 0,
            warnings: None,
            counts: Default::default(),
            _counts_bg: None,
            panel_find: Find::default(),
            panel_hovered: false,
            find: Find::default(),
            focus_filter: false,
            focus_launcher: false,
            renaming: None,
            alias: None,
            node_shells: vec![],
            scale_to: 1,
            metrics: Default::default(),
            metrics_bg: None,
            helm: Helm::default(),
            forwards: vec![],
            actions: vec![],
            confirm: None,
        };
        if let Some(s) = restore {
            r.ns_sel = s.ns.into_iter().collect();
            r.wide = s.wide;
            for sp in s.pages {
                let gone = match &sp.page {
                    Page::Kind(g, k) => r.kind(g, k).is_none(),
                    Page::Helm => !has_helm(),
                    _ => false,
                };
                if gone {
                    continue;
                }
                let mut t = r.new_page(ctx, sp.page, sp.preview);
                t.name = sp.name;
                t.set_view(sp.view);
                for (i, (g, k, ns, name)) in sp.objs.into_iter().enumerate() {
                    if let Some(kind) = r.kind(&g, &k) {
                        t.open_obj(ctx, &r.client, kind, ns, name, false);
                        if let (Some(o), Some(s)) = (t.subs.last_mut(), sp.sections.get(i)) {
                            o.details.section = *s;
                        }
                    }
                }
                t.active = sp.active.min(t.subs.len());
                r.pages.push(t);
            }
            r.cur = s.cur.min(r.pages.len().saturating_sub(1));
        }
        if r.pages.is_empty() {
            let t = r.new_page(ctx, Page::Overview, true);
            r.pages.push(t);
        }
        r.restart_watches(ctx);
        r.update_metrics(ctx);
        r
    }

    /// Cluster-wide helpers that follow the namespace filter: warning events and sidebar counts.
    fn restart_watches(&mut self, ctx: &egui::Context) {
        self.warnings = self.kind("", "Event").map(|k| List::new(ctx, &self.client, k, &self.ns_sel, Some("type=Warning")));
        let node = [("", "Node", ""), ("", "Namespace", "")];
        let kinds: Vec<Kind> = NAV.iter().flat_map(|(_, e)| e.iter()).chain(node.iter()).filter_map(|(g, k, _)| self.kind(g, k)).collect();
        self.counts.locked().clear();
        let ns = self.ns_sel.iter().cloned().collect();
        self._counts_bg = Some(Bg::spawn(ops::count_loop(self.client.clone(), kinds, ns, self.counts.clone(), ctx.clone())));
    }

    fn saved(&self) -> SavedTabs {
        let pages = self
            .pages
            .iter()
            .map(|t| SavedPage {
                page: t.page.clone(),
                preview: t.preview,
                name: t.name.clone(),
                active: t.active,
                objs: t.subs.iter().map(|o| (o.details.t.kind.ar.group.clone(), o.details.t.kind.ar.kind.clone(), o.details.t.ns.clone(), o.details.t.name.clone())).collect(),
                sections: t.subs.iter().map(|o| o.details.section).collect(),
                view: t.view(),
            })
            .collect();
        SavedTabs { pages, cur: self.cur, ns: self.ns_sel.iter().cloned().collect(), wide: self.wide }
    }

    fn kind(&self, g: &str, k: &str) -> Option<Kind> {
        self.kinds.iter().find(|x| x.is(g, k)).cloned()
    }

    /// Metrics are polled only while something shows them.
    fn update_metrics(&mut self, ctx: &egui::Context) {
        let pod_or_node = |k: &Kind| k.is("", "Pod") || k.is("", "Node");
        let want = self.pages.get(self.cur).is_some_and(|pt| match pt.active {
            0 => matches!(&pt.page, Page::Overview) || matches!(&pt.page, Page::Kind(g, k) if g.is_empty() && (k == "Pod" || k == "Node")),
            i => pt.subs.get(i - 1).is_some_and(|t| pod_or_node(&t.details.t.kind)),
        });
        if want != self.metrics_bg.is_some() {
            self.metrics_bg = want.then(|| Bg::spawn(ops::metrics_loop(self.client.clone(), self.has_metrics, self.metrics.clone(), ctx.clone())));
        }
    }

    fn default_ns(&self) -> String {
        match self.ns_sel.len() {
            1 => self.ns_sel.first().cloned().unwrap_or_default(),
            _ => self.kctx.namespace.clone().unwrap_or_else(|| "default".into()),
        }
    }

    fn ui(&mut self, ui: &mut Ui, out: &mut Out) {
        let ctx = ui.ctx().clone();
        self.actions.retain_mut(|p| match p.poll() {
            Some(r) => {
                if r.as_ref().map_or(true, |m| !m.is_empty()) {
                    out.toast(r); // (an empty message: a dialog that was cancelled)
                }
                false
            }
            None => true,
        });
        self.helm_poll(&ctx, out);
        for pt in &mut self.pages {
            for t in pt.subs.iter_mut().chain(pt.panel.iter_mut()) {
                t.sync(&ctx);
            }
        }
        let client = self.client.clone();
        self.node_shells.retain_mut(|(node, p)| match p.poll() {
            Some(Ok((ns, pod))) => {
                out.term(&ctx, format!("Node {node}"), tabs::node_exec(&self.kctx, &ns, &pod), Some((client.clone(), ns, pod)));
                false
            }
            Some(Err(e)) => {
                out.toast(Err(e));
                false
            }
            None => true,
        });

        let t = ui_kit::tokens(ui);
        let (mut nav, mut nav_ns) = (None, false);
        let nav_frame = egui::Frame::new().fill(t.chrome).inner_margin(egui::Margin::symmetric(8, 0)).stroke(egui::Stroke::new(1.0, t.line));
        egui::Panel::left(egui::Id::new(("nav", self.kctx.id()))).resizable(true).default_size(240.0).size_range(190.0..=400.0).frame(nav_frame).show(ui, |ui| (nav, nav_ns) = ui_kit::scaled(ui, ui_kit::Area::Sidebar, |ui| self.nav(ui, out)));
        if let Some((p, pin)) = nav {
            self.show_page(&ctx, p, pin);
        }

        // Page views need `&mut self` too: draw them with the page tabs taken out, put them back after.
        let (mut acts, mut ev) = (Vec::new(), PageEv::default());
        let mut pages = std::mem::take(&mut self.pages);
        egui::Panel::top(egui::Id::new(("tabs", self.kctx.id()))).frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin { left: 8, right: 12, top: 0, bottom: 2 })).show(ui, |ui| {
            {
                let _tabs = ui_kit::area(ui_kit::Area::Tabs);
                self.page_tabs_ui(ui, &mut pages);
                if let Some(pt) = pages.get_mut(self.cur) {
                    self.subtabs_ui(ui, pt);
                }
            }
            self.find.bar(ui, |_| false);
        });
        match pages.get_mut(self.cur) {
            Some(pt) => {
                egui::CentralPanel::default().frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin { left: 20, right: 20, top: 10, bottom: 8 })).show(ui, |ui| ui_kit::scaled(ui, ui_kit::Area::Lists, |ui| self.page_ui(ui, pt, out, &mut acts, &mut ev)));
            }
            None => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);
                        ui.label(RichText::new("Pick a resource on the left · double-click keeps it as a tab").weak());
                    });
                });
            }
        }
        self.pages = pages;

        if ev.ns_changed || nav_ns {
            for pt in &mut self.pages {
                pt.load(&ctx, &self.client, &self.kinds, &self.ns_sel);
            }
            self.restart_watches(&ctx);
        }
        if let Some(p) = ev.replace {
            self.replace_launcher(&ctx, p);
        }
        if let Some(p) = ev.goto {
            self.show_page(&ctx, p, false);
        }
        self.update_metrics(&ctx);
        for (targets, a) in acts {
            self.act(&ctx, targets, a, out);
        }
        self.confirm_ui(&ctx, out);
    }

    fn page_ui(&mut self, ui: &mut Ui, pt: &mut PageTab, out: &mut Out, acts: &mut Vec<(Vec<Target>, Act)>, ev: &mut PageEv) {
        if pt.active == 0 {
            match pt.page.clone() {
                Page::Kind(..) => ev.ns_changed |= self.list_page(ui, pt, out, acts),
                Page::Overview => ev.ns_changed |= self.overview(ui, acts),
                Page::Workloads => {
                    let (goto, changed) = self.workloads_ui(ui, pt);
                    ev.goto = goto;
                    ev.ns_changed |= changed;
                }
                Page::Forwards => self.forwards_ui(ui),
                Page::Helm => ev.ns_changed |= self.helm_ui(ui),
                Page::Search => ev.ns_changed |= self.search_page(ui, pt),
                Page::Launcher => ev.replace = self.launcher_ui(ui, pt),
            }
        } else if let Some(t) = pt.subs.get_mut(pt.active - 1) {
            let mut dacts = vec![];
            // Object tabs share widget ids (grids, collapsing headers): keep their state apart.
            ui.push_id(t.id, |ui| ui_kit::scaled(ui, ui_kit::Area::Details, |ui| t.details.ui(ui, &self.metrics.locked(), &mut dacts, &mut self.find, true)));
            acts.extend(dacts.into_iter().map(|a| (vec![t.details.t.clone()], a)));
        }
    }

    /// `panel_ui` for the current page, then what it asked for (see `ClusterTab::details_panel`).
    fn details_panel(&mut self, ui: &mut Ui, out: &mut Out, accent: Option<egui::Color32>) {
        let ctx = ui.ctx().clone();
        let (mut acts, mut pages) = (Vec::new(), std::mem::take(&mut self.pages));
        if let Some(pt) = pages.get_mut(self.cur) {
            if let Some(p) = &mut pt.panel {
                p.sync(&ctx); // it is drawn before `Ready::ui` syncs the tabs
            }
            self.panel_ui(ui, pt, &mut acts, accent);
        }
        self.pages = pages;
        for (targets, a) in acts {
            self.act(&ctx, targets, a, out);
        }
    }

    /// Details of the clicked row, a column on the right of the window (only on the page view).
    fn panel_ui(&mut self, ui: &mut Ui, pt: &mut PageTab, acts: &mut Vec<(Vec<Target>, Act)>, accent: Option<egui::Color32>) {
        self.panel_hovered = false;
        if pt.active != 0 {
            return;
        }
        let Some(p) = &mut pt.panel else { return };
        let (mut close, mut to_tab, mut dacts) = (false, false, vec![]);
        // Room left for the navigation and the list.
        let max = (ui.available_width() - 560.0).max(320.0);
        let t = ui_kit::tokens(ui);
        let frame = egui::Frame::new().fill(t.panel).inner_margin(egui::Margin { left: 18, right: 16, top: 12, bottom: 8 }).stroke(egui::Stroke::new(1.0, t.line));
        let shown = egui::Panel::right(egui::Id::new(("details-panel", self.kctx.id()))).resizable(true).default_size(460.0).size_range(320.0..=max).frame(frame).show(ui, |ui| ui_kit::scaled(ui, ui_kit::Area::Details, |ui| {
            if let Some(c) = accent {
                crate::theme::tint(ui.visuals_mut(), c); // the cluster's color, as the page has it
            }
            ui.horizontal(|ui| {
                ui_kit::section_label(ui, &p.details.t.kind.ar.kind);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = ui_kit::icon_button(ui, Icon::X, 26.0, "Close (Esc)").clicked();
                    if ui_kit::icon_button(ui, Icon::Search, 26.0, "Find in these details (Ctrl+F with the pointer here)").clicked() {
                        self.panel_find.open();
                    }
                    to_tab = ui_kit::button(ui, Btn::Ghost, Some(Icon::External), "Open in tab").on_hover_text("Open in its own tab (double-click a row does the same)").clicked();
                });
            });
            // Finds in everything the panel shows: keys, values (hidden secret values count as
            // "hidden matches" without being revealed), references, events and the YAML.
            self.panel_find.bar(ui, |_| false);
            ui.push_id(p.id, |ui| p.details.ui(ui, &self.metrics.locked(), &mut dacts, &mut self.panel_find, false));
        }));
        self.panel_hovered = ui.ctx().pointer_hover_pos().is_some_and(|pos| shown.response.rect.contains(pos));
        let t = p.details.t.clone();
        acts.extend(dacts.into_iter().map(|a| (vec![t.clone()], a)));
        if to_tab {
            acts.push((vec![t], Act::Details));
        }
        if close {
            pt.panel = None;
        }
    }

}

