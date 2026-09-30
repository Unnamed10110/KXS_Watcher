//! One cluster tab: connect + discovery, sidebar navigation, resource lists with object sub-tabs,
//! overview pages, Helm releases and port forwards.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, ListParams};
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
use crate::watch::{self, cell_color, contains_ci, take, Bg, ListData, Pending, Row, Shared, GREEN, ORANGE, RED};
use crate::{next_id, Body, Out};

pub static HELM: LazyLock<bool> = LazyLock::new(|| ops::tool_available("helm"));

type Job = Pin<Box<dyn Future<Output = Res> + Send>>;

/// Sidebar sections: (title, [(group, kind, label)]). Entries missing from discovery are hidden.
const NAV: &[(&str, &[(&str, &str, &str)])] = &[
    ("Workloads", &[("", "Pod", "Pods"), ("apps", "Deployment", "Deployments"), ("apps", "DaemonSet", "Daemon Sets"), ("apps", "StatefulSet", "Stateful Sets"), ("apps", "ReplicaSet", "Replica Sets"), ("batch", "Job", "Jobs"), ("batch", "CronJob", "Cron Jobs")]),
    ("Config", &[
        ("", "ConfigMap", "Config Maps"), ("", "Secret", "Secrets"), ("", "ResourceQuota", "Resource Quotas"), ("", "LimitRange", "Limit Ranges"),
        ("autoscaling", "HorizontalPodAutoscaler", "Horizontal Pod Autoscalers"), ("policy", "PodDisruptionBudget", "Pod Disruption Budgets"),
        ("scheduling.k8s.io", "PriorityClass", "Priority Classes"), ("node.k8s.io", "RuntimeClass", "Runtime Classes"), ("coordination.k8s.io", "Lease", "Leases"),
        ("admissionregistration.k8s.io", "MutatingWebhookConfiguration", "Mutating Webhooks"), ("admissionregistration.k8s.io", "ValidatingWebhookConfiguration", "Validating Webhooks"),
    ]),
    ("Network", &[("", "Service", "Services"), ("", "Endpoints", "Endpoints"), ("networking.k8s.io", "Ingress", "Ingresses"), ("networking.k8s.io", "IngressClass", "Ingress Classes"), ("networking.k8s.io", "NetworkPolicy", "Network Policies")]),
    ("Storage", &[("", "PersistentVolumeClaim", "Persistent Volume Claims"), ("", "PersistentVolume", "Persistent Volumes"), ("storage.k8s.io", "StorageClass", "Storage Classes")]),
    ("Access Control", &[
        ("", "ServiceAccount", "Service Accounts"), ("rbac.authorization.k8s.io", "ClusterRole", "Cluster Roles"), ("rbac.authorization.k8s.io", "Role", "Roles"),
        ("rbac.authorization.k8s.io", "ClusterRoleBinding", "Cluster Role Bindings"), ("rbac.authorization.k8s.io", "RoleBinding", "Role Bindings"),
    ]),
];

const WORKLOADS: &[(&str, &str)] = &[("", "Pod"), ("apps", "Deployment"), ("apps", "DaemonSet"), ("apps", "StatefulSet"), ("apps", "ReplicaSet"), ("batch", "Job"), ("batch", "CronJob")];

/// Fallback for spotting custom resources when CRDs can't be listed.
const BUILTIN_GROUPS: &[&str] = &[
    "", "apps", "batch", "autoscaling", "policy", "networking.k8s.io", "storage.k8s.io", "rbac.authorization.k8s.io", "scheduling.k8s.io", "node.k8s.io",
    "coordination.k8s.io", "admissionregistration.k8s.io", "apiextensions.k8s.io", "discovery.k8s.io", "events.k8s.io", "authentication.k8s.io",
    "authorization.k8s.io", "certificates.k8s.io", "flowcontrol.apiserver.k8s.io", "apiregistration.k8s.io", "metrics.k8s.io", "resource.k8s.io",
    "storagemigration.k8s.io", "internal.apiserver.k8s.io",
];

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

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
enum Page {
    Overview,
    Workloads,
    Kind(String, String),
    Forwards,
    Helm,
    /// Text search inside Config Maps and Secrets.
    Search,
    /// A new tab (Ctrl+T): pick what it shows.
    Launcher,
}

fn kind_page(g: &str, k: &str) -> Page {
    Page::Kind(g.into(), k.into())
}

/// Two letters for the cluster avatar: "k8s-qa" → "QA", "rancher-desktop" → "RD".
fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty() && !matches!(w.to_ascii_lowercase().as_str(), "k8s" | "k3s" | "kube" | "cluster" | "context")).collect();
    let s: String = match words.as_slice() {
        [] => name.chars().filter(char::is_ascii_alphanumeric).take(2).collect(),
        [w] => w.chars().take(2).collect(),
        [a, b, ..] => a.chars().take(1).chain(b.chars().take(1)).collect(),
    };
    s.to_uppercase()
}

/// Collapsible sidebar group header; returns whether it is open (kept per title).
fn nav_section(ui: &mut Ui, title: &str, default_open: bool) -> bool {
    let id = ui.make_persistent_id(("nav-section", title));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    let t = ui_kit::tokens(ui);
    ui.add_space(4.0);
    let (rect, r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::click());
    let open = state.is_open();
    let color = if r.hovered() { t.muted } else { t.dim };
    ui_kit::paint_icon(ui, egui::Rect::from_center_size(egui::pos2(rect.left() + 16.0, rect.center().y), egui::vec2(11.0, 11.0)), if open { Icon::ChevronDown } else { Icon::ChevronRight }, color);
    ui.painter().text(egui::pos2(rect.left() + 28.0, rect.center().y), egui::Align2::LEFT_CENTER, title.to_uppercase(), ui_kit::semibold(11.0), color);
    if r.clicked() {
        state.toggle(ui);
    }
    state.store(ui.ctx());
    open
}

/// One overview number: title, value, a usage bar and a footnote.
fn stat_card(ui: &mut Ui, title: &str, value: &str, sub: &str, frac: Option<f32>, foot: &str, warn: bool) {
    let t = ui_kit::tokens(ui);
    let (fill, line) = if warn { (mix(t.card, ORANGE, 0.05), mix(t.card, ORANGE, 0.3)) } else { (t.card, t.line) };
    egui::Frame::new().fill(fill).stroke(egui::Stroke::new(1.0, line)).corner_radius(egui::CornerRadius::same(12)).inner_margin(egui::Margin::symmetric(18, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        ui_kit::section_label(ui, title);
        ui.horizontal(|ui| {
            ui.label(RichText::new(value).font(ui_kit::semibold(28.0)).color(if warn { ui_kit::status_fg(ui, ORANGE) } else { t.text }));
            ui.label(RichText::new(sub).color(t.muted));
        });
        let w = ui.available_width();
        match frac {
            Some(f) => ui_kit::bar(ui, f, w, if f > 0.85 { RED } else { t.accent }),
            None => ui.allocate_exact_size(egui::vec2(w, 6.0), egui::Sense::hover()).1,
        };
        ui.add(egui::Label::new(RichText::new(foot).size(ui_kit::sz(12.0)).color(t.dim)).truncate());
    });
}

struct Conn {
    client: Client,
    kinds: Vec<Kind>,
    version: String,
    crds: Option<HashSet<String>>,
}

async fn connect(k: Ctx) -> Res<Conn> {
    let r: anyhow::Result<Conn> = async {
        let client = kubeconfig::client(&k).await?;
        let version = client.apiserver_version().await?.git_version;
        let kinds = ops::discover(&client).await?;
        // CRD names are "<plural>.<group>"; metadata-only keeps this cheap on CRD-heavy clusters.
        let crds = Api::<CustomResourceDefinition>::all(client.clone())
            .list_metadata(&ListParams::default())
            .await
            .ok()
            .map(|l| l.items.into_iter().filter_map(|c| c.metadata.name).collect());
        Ok(Conn { client, kinds, version, crds })
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
                    ui.spinner();
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

/// Status breakdown for the workloads overview cards.
fn breakdown(d: &ListData) -> Vec<(String, usize)> {
    let idx = |n: &str| d.cols.iter().position(|c| c.name == n);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in d.rows.values() {
        let cell = |i: usize| r.cells.get(i).map_or("", |s| s.as_str());
        let key = if let Some(i) = idx("Status") {
            cell(i).to_string()
        } else if let (Some(des), Some(rdy)) = (idx("Desired"), idx("Ready")) {
            if cell(des) == cell(rdy) { "Ready" } else { "Not ready" }.into()
        } else if let Some(i) = idx("Ready").or(idx("Completions")) {
            match cell(i).split_once('/') {
                Some((a, b)) if a.trim() == b.trim() => "Ready".into(),
                Some(_) => "Not ready".into(),
                None => cell(i).to_string(),
            }
        } else if let Some(i) = idx("Suspend") {
            if cell(i) == "True" { "Suspended" } else { "Active" }.into()
        } else {
            "Total".into()
        };
        *counts.entry(key).or_default() += 1;
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

#[derive(Default)]
struct Helm {
    rows: Vec<Value>,
    load: Option<Pending<Res>>,
    error: Option<String>,
    search: String,
    /// (namespace, release)
    sel: Option<(String, String)>,
    history: Vec<Value>,
    hist_load: Option<Pending<Res>>,
    view: Option<(String, Pending<Res>)>,
    busy: Option<Pending<Res>>,
}

enum Confirm {
    Act(Vec<Target>, Act),
    Helm(String, Vec<String>),
}

/// An object shown in an object tab (own single-object watch, `uid: None`) or in the details panel
/// (follows the list's row `uid`, no extra watch). Either way the data only says when to refresh.
struct ObjTab {
    id: u64,
    title: String,
    details: Details,
    data: Shared,
    uid: Option<String>,
    _watch: Option<Bg>,
}

impl ObjTab {
    fn sync(&mut self, ctx: &egui::Context) {
        let d = self.data.lock().unwrap();
        if d.synced {
            let row = match &self.uid {
                Some(u) => d.rows.get(u),
                None => d.rows.values().next(),
            };
            let rv = row.map(|r| r.rv.clone());
            drop(d);
            self.details.sync(ctx, rv.as_deref());
        }
    }
}

/// Open tabs of a cluster, saved across restarts.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTabs {
    pages: Vec<SavedPage>,
    cur: usize,
    ns: Vec<String>,
    #[serde(default)]
    wide: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SavedPage {
    page: Page,
    preview: bool,
    /// Name given to the tab.
    #[serde(default)]
    name: Option<String>,
    /// Object tabs: (group, kind, namespace, name).
    objs: Vec<(String, String, String, String)>,
    active: usize,
    /// What each object tab shows, in `objs` order.
    #[serde(default)]
    sections: Vec<details::Section>,
    #[serde(default)]
    view: SavedView,
}

/// A page's view: the list's filter, sort and pods status, and the row whose details are open;
/// the Search page's query.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct SavedView {
    search: String,
    re: bool,
    sort: Option<(crate::list::DCol, bool)>,
    status: Option<String>,
    /// Details panel: (namespace, name).
    panel: Option<(String, String)>,
}

/// A page tab (Pods, Deployments, Overview…) with its own list and the object tabs opened from it.
struct PageTab {
    id: u64,
    page: Page,
    /// Replaced by the next single click in the sidebar (shown in italics).
    preview: bool,
    list: Option<List>,
    workloads: Vec<List>,
    /// Object tabs; `active` 0 is the page view, i > 0 is `subs[i - 1]`.
    subs: Vec<ObjTab>,
    active: usize,
    /// Details of the clicked row, right of the list.
    panel: Option<ObjTab>,
    content: Option<ContentSearch>,
    /// New tab: its filter.
    launch_query: String,
    /// Name given to the tab (rename); the page's own title when `None`.
    name: Option<String>,
    /// Details panel open at exit: (namespace, name), shown once the list has loaded.
    panel_restore: Option<(String, String)>,
}

impl PageTab {
    fn new(page: Page, preview: bool) -> Self {
        PageTab { id: next_id(), page, preview, list: None, workloads: vec![], subs: vec![], active: 0, panel: None, content: None, launch_query: String::new(), name: None, panel_restore: None }
    }

    fn view(&self) -> SavedView {
        let mut v = SavedView::default();
        if let Some(l) = &self.list {
            (v.search, v.re, v.sort, v.status) = (l.search.clone(), l.filter_re, l.sort, l.status.map(String::from));
            v.panel = self.panel.as_ref().map(|p| (p.details.t.ns.clone(), p.details.t.name.clone())).or_else(|| self.panel_restore.clone());
        }
        if let Some(c) = &self.content {
            (v.search, v.re) = (c.find.query.clone(), c.find.regex);
        }
        v
    }

    fn set_view(&mut self, v: SavedView) {
        if let Some(l) = &mut self.list {
            (l.search, l.filter_re, l.sort) = (v.search.clone(), v.re, v.sort);
            l.status = v.status.and_then(|s| crate::list::POD_BUCKETS.into_iter().find(|b| *b == s));
            self.panel_restore = v.panel;
        }
        if let Some(c) = &mut self.content {
            (c.find.query, c.find.regex) = (v.search, v.re);
        }
    }

    /// (Re)start the watches this page needs; also after a namespace change.
    fn load(&mut self, ctx: &egui::Context, client: &Client, kinds: &[Kind], ns_sel: &BTreeSet<String>) {
        let kind = |g: &str, k: &str| kinds.iter().find(|x| x.is(g, k)).cloned();
        let keep = self.list.take().map(|l| (l.kind.ar.clone(), l.search, l.filter_re, l.sort));
        self.panel = None;
        self.workloads.clear();
        match &self.page {
            Page::Kind(g, k) => {
                if let Some(kd) = kind(g, k) {
                    let mut l = List::new(ctx, client, kd, ns_sel, None);
                    if let Some((_, search, re, sort)) = keep.filter(|(ar, ..)| *ar == l.kind.ar) {
                        (l.search, l.filter_re, l.sort) = (search, re, sort); // namespace change: keep the user's view
                    }
                    self.list = Some(l);
                }
            }
            Page::Overview => {} // uses the cluster's warning watch
            Page::Workloads => self.workloads = WORKLOADS.iter().filter_map(|(g, k)| kind(g, k)).map(|k| List::new(ctx, client, k, ns_sel, None)).collect(),
            Page::Search => {
                let ns = ns_sel.iter().cloned().collect();
                match &mut self.content {
                    Some(c) => c.reload(ctx, Some(ns)), // keeps the query
                    None => self.content = Some(ContentSearch::new(ctx, client.clone(), ns)),
                }
            }
            Page::Helm | Page::Forwards | Page::Launcher => {}
        }
    }

    fn title(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match &self.page {
            Page::Overview => "Overview".into(),
            Page::Workloads => "Workloads".into(),
            Page::Forwards => "Port Forwarding".into(),
            Page::Helm => "Helm releases".into(),
            Page::Search => "Search".into(),
            Page::Launcher => "New tab".into(),
            Page::Kind(_, k) => self.list.as_ref().map_or(k.clone(), |l| label(&l.kind)),
        }
    }

    /// Open (or focus) an object tab here; content makes the page tab permanent.
    fn open_obj(&mut self, ctx: &egui::Context, client: &Client, kind: Kind, ns: String, name: String, focus: bool) {
        self.preview = false;
        let ns = if kind.namespaced { ns } else { String::new() };
        if let Some(i) = self.subs.iter().position(|t| t.details.t.kind.ar == kind.ar && t.details.t.ns == ns && t.details.t.name == name) {
            if focus {
                self.active = i + 1;
            }
            return;
        }
        let data: Shared = Default::default();
        let watch = (kind.can("watch") && kind.can("list")).then(|| {
            let scope = (!ns.is_empty()).then(|| ns.clone());
            Bg::spawn(watch::run(client.clone(), kind.ar.clone(), scope, Some(format!("metadata.name={name}")), data.clone(), ctx.clone()))
        });
        let title = format!("{}/{name}", kind.ar.kind.to_lowercase());
        let details = Details::new(ctx, client.clone(), kind, ns, name);
        self.subs.push(ObjTab { id: next_id(), title, details, data, uid: None, _watch: watch });
        if focus {
            self.active = self.subs.len();
        }
    }
}

#[derive(Debug, PartialEq)]
enum NavTo {
    Focus(usize),
    Replace(usize),
    Push,
}

/// Where a sidebar click goes: an open page is focused; a single click reuses the preview tab.
fn nav_target(tabs: &[(&Page, bool)], page: &Page, pin: bool) -> NavTo {
    if let Some(i) = tabs.iter().position(|(p, _)| *p == page) {
        return NavTo::Focus(i);
    }
    match tabs.iter().position(|(_, preview)| *preview) {
        Some(i) if !pin => NavTo::Replace(i),
        _ => NavTo::Push,
    }
}

/// What a page view asks for after drawing.
#[derive(Default)]
struct PageEv {
    ns_changed: bool,
    goto: Option<Page>,
    /// The new tab picked what it shows.
    replace: Option<Page>,
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
    renaming: Option<(u64, String, bool)>,
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
                    Page::Helm => !*HELM,
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
        self.counts.lock().unwrap().clear();
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

    fn new_page(&mut self, ctx: &egui::Context, page: Page, preview: bool) -> PageTab {
        let mut t = PageTab::new(page, preview);
        t.load(ctx, &self.client, &self.kinds, &self.ns_sel);
        if t.page == Page::Helm && self.helm.rows.is_empty() && self.helm.load.is_none() {
            self.helm_refresh(ctx);
        }
        t
    }

    /// Ctrl+T or "+": a new tab that asks what to show.
    fn new_tab(&mut self) {
        self.pages.push(PageTab::new(Page::Launcher, false));
        self.cur = self.pages.len() - 1;
        self.focus_launcher = true;
    }

    fn close_tab(&mut self) {
        match self.pages.get_mut(self.cur) {
            Some(pt) if pt.active > 0 => {
                pt.subs.remove(pt.active - 1);
                pt.active -= 1; // its left neighbour, or the list
            }
            Some(_) => {
                let i = self.cur;
                ui_kit::remove_item(&mut self.pages, i, &mut self.cur);
            }
            None => {}
        }
    }

    /// The current new tab becomes `page` (or, when that page is already open, goes away for it).
    fn replace_launcher(&mut self, ctx: &egui::Context, page: Page) {
        match self.pages.iter().position(|t| t.page == page) {
            Some(i) => {
                self.pages.remove(self.cur);
                self.cur = if i > self.cur { i - 1 } else { i };
            }
            None => {
                let t = self.new_page(ctx, page, false);
                self.pages[self.cur] = t;
            }
        }
        self.pages[self.cur].active = 0;
    }

    /// Sidebar click: focus an open page, else preview it (single) or open its own tab (double).
    fn show_page(&mut self, ctx: &egui::Context, page: Page, pin: bool) {
        if self.pages.get(self.cur).is_some_and(|t| t.page == Page::Launcher) {
            return self.replace_launcher(ctx, page); // the new tab takes what the sidebar opens
        }
        let tabs: Vec<(&Page, bool)> = self.pages.iter().map(|t| (&t.page, t.preview)).collect();
        match nav_target(&tabs, &page, pin) {
            NavTo::Focus(i) => {
                self.cur = i;
                self.pages[i].preview &= !pin;
            }
            NavTo::Replace(i) => {
                self.pages[i] = self.new_page(ctx, page, true);
                self.cur = i;
            }
            NavTo::Push => {
                let t = self.new_page(ctx, page, !pin);
                self.pages.push(t);
                self.cur = self.pages.len() - 1;
            }
        }
        self.pages[self.cur].active = 0;
    }

    /// Object tab inside page tab `pi`.
    fn open_obj(&mut self, ctx: &egui::Context, pi: usize, kind: Kind, ns: String, name: String, focus: bool) {
        let client = self.client.clone();
        if let Some(pt) = self.pages.get_mut(pi) {
            pt.open_obj(ctx, &client, kind, ns, name, focus);
            if focus {
                self.cur = pi;
            }
        }
    }

    /// Links (pod → node, pod → secret): the object opens inside its kind's tab.
    fn open_in_kind_tab(&mut self, ctx: &egui::Context, kind: Kind, ns: String, name: String) {
        let page = kind_page(&kind.ar.group, &kind.ar.kind);
        let pi = match self.pages.iter().position(|t| t.page == page) {
            Some(i) => i,
            None => {
                let t = self.new_page(ctx, page, false);
                self.pages.push(t);
                self.pages.len() - 1
            }
        };
        self.open_obj(ctx, pi, kind, ns, name, true);
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
                out.toast(r);
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
                self.panel_ui(ui, pt, &mut acts);
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
            ui.push_id(t.id, |ui| ui_kit::scaled(ui, ui_kit::Area::Details, |ui| t.details.ui(ui, &self.metrics.lock().unwrap(), &mut dacts, &mut self.find, true)));
            acts.extend(dacts.into_iter().map(|a| (vec![t.details.t.clone()], a)));
        }
    }

    /// Details of the clicked row, right of the list (only on the page view).
    fn panel_ui(&mut self, ui: &mut Ui, pt: &mut PageTab, acts: &mut Vec<(Vec<Target>, Act)>) {
        self.panel_hovered = false;
        if pt.active != 0 {
            return;
        }
        let Some(p) = &mut pt.panel else { return };
        let (mut close, mut to_tab, mut dacts) = (false, false, vec![]);
        let max = (ui.available_width() - 360.0).max(320.0);
        let t = ui_kit::tokens(ui);
        let frame = egui::Frame::new().fill(t.panel).inner_margin(egui::Margin { left: 18, right: 16, top: 12, bottom: 8 }).stroke(egui::Stroke::new(1.0, t.line));
        let shown = egui::Panel::right(egui::Id::new(("details-panel", self.kctx.id(), pt.id))).resizable(true).default_size(460.0).size_range(320.0..=max).frame(frame).show(ui, |ui| ui_kit::scaled(ui, ui_kit::Area::Details, |ui| {
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
            ui.push_id(p.id, |ui| p.details.ui(ui, &self.metrics.lock().unwrap(), &mut dacts, &mut self.panel_find, false));
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

    /// First tab row: one tab per page (Pods, Deployments…); the preview tab is in italics.
    fn page_tabs_ui(&mut self, ui: &mut Ui, pages: &mut Vec<PageTab>) {
        let (mut close, mut keep_only) = (None, None);
        let (mut moved, mut rename, mut renamed) = (None, None, false);
        let t = ui_kit::tokens(ui);
        let n = pages.len();
        ui.scope(|ui| {
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal().id_salt(("pagetabs", self.kctx.id())).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(ui_kit::sz(48.0));
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let mut items = vec![];
                    for (i, pt) in pages.iter_mut().enumerate() {
                        ui.add_space(if i == 0 { 10.0 } else { 18.0 });
                        if let Some((id, text, focused)) = &mut self.renaming
                            && *id == pt.id
                        {
                            // Enter keeps the name, Esc keeps the old one; empty goes back to the default.
                            let r = ui.add(egui::TextEdit::singleline(text).desired_width(160.0).font(ui_kit::semibold(13.0)));
                            if !std::mem::replace(focused, true) {
                                r.request_focus();
                            }
                            items.push((r.rect, false, false));
                            if r.lost_focus() {
                                if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    let name = text.trim();
                                    pt.name = (!name.is_empty()).then(|| name.to_string());
                                }
                                renamed = true;
                            }
                            continue;
                        }
                        let tip = if pt.preview { "Preview: the next single click in the sidebar replaces it. Double-click to keep it." } else { "Drag to move · double-click to rename · middle-click to close" };
                        let r = ui_kit::tab(ui, &pt.title(), i == self.cur, pt.preview, false).on_hover_text(tip);
                        items.push((r.rect, r.dragged(), r.drag_stopped()));
                        if r.double_clicked() {
                            if pt.preview {
                                pt.preview = false;
                            } else {
                                rename = Some((pt.id, pt.title()));
                            }
                        }
                        if r.clicked() {
                            self.cur = i;
                        }
                        if r.middle_clicked() || ui_kit::icon_button(ui, Icon::X, 20.0, "Close").clicked() {
                            close = Some(i);
                        }
                        r.context_menu(|ui| {
                            if ui.button("Rename…").clicked() {
                                rename = Some((pt.id, pt.title()));
                                ui.close();
                            }
                            if pt.name.is_some() && ui.button("Reset name").clicked() {
                                pt.name = None;
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
                            if pt.preview && ui.button("Keep open").clicked() {
                                pt.preview = false;
                                ui.close();
                            }
                            if ui.button("Close").clicked() {
                                close = Some(i);
                                ui.close();
                            }
                            if ui.button("Close others").clicked() {
                                keep_only = Some(i);
                                ui.close();
                            }
                            if ui.button("Close all").clicked() {
                                keep_only = Some(usize::MAX);
                                ui.close();
                            }
                        });
                    }
                    if let Some(m) = ui_kit::reorder(ui, &items) {
                        moved = Some(m);
                    }
                    ui.add_space(10.0);
                    if ui_kit::icon_button(ui, Icon::Plus, 24.0, "New tab (Ctrl+T)").clicked() {
                        pages.push(PageTab::new(Page::Launcher, false));
                        self.cur = pages.len() - 1;
                        self.focus_launcher = true;
                    }
                });
            });
        });
        ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top() - 1.0, egui::Stroke::new(1.0, t.line));
        if renamed {
            self.renaming = None;
        }
        if let Some((id, text)) = rename {
            self.renaming = Some((id, text, false));
        }
        if let Some((from, to)) = moved {
            ui_kit::move_item(pages, from, to, &mut self.cur);
        }
        if let Some(i) = close {
            ui_kit::remove_item(pages, i, &mut self.cur);
        }
        if let Some(i) = keep_only {
            let keep = (i < pages.len()).then(|| pages.swap_remove(i));
            pages.clear();
            pages.extend(keep);
            self.cur = 0;
        }
    }

    /// Second tab row: the page's list plus the objects opened from it, as pills.
    fn subtabs_ui(&mut self, ui: &mut Ui, pt: &mut PageTab) {
        if pt.subs.is_empty() {
            return;
        }
        let (mut close, mut keep_only, mut moved) = (None, None, None);
        ui.add_space(6.0);
        ui.scope(|ui| {
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal().id_salt(("subtabs", self.kctx.id(), pt.id)).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if ui_kit::pill_tab(ui, Some(Icon::List), "List", pt.active == 0, false, false).0.clicked() {
                        pt.active = 0;
                    }
                    let n = pt.subs.len();
                    let mut items = vec![];
                    for (i, t) in pt.subs.iter().enumerate() {
                        let (r, x) = ui_kit::pill_tab(ui, None, &t.title, pt.active == i + 1, true, true);
                        let r = r.on_hover_text("Drag to move · middle-click to close");
                        items.push((r.rect, r.dragged(), r.drag_stopped()));
                        if r.clicked() {
                            pt.active = i + 1;
                        }
                        if r.middle_clicked() || x.is_some_and(|x| x.clicked()) {
                            close = Some(i);
                        }
                        r.context_menu(|ui| {
                            if i > 0 && ui.button("Move left").clicked() {
                                moved = Some((i, i - 1));
                                ui.close();
                            }
                            if i + 1 < n && ui.button("Move right").clicked() {
                                moved = Some((i, i + 1));
                                ui.close();
                            }
                            if ui.button("Close").clicked() {
                                close = Some(i);
                                ui.close();
                            }
                            if ui.button("Close others").clicked() {
                                keep_only = Some(i);
                                ui.close();
                            }
                            if ui.button("Close all").clicked() {
                                keep_only = Some(usize::MAX);
                                ui.close();
                            }
                        });
                    }
                    if let Some(m) = ui_kit::reorder(ui, &items) {
                        moved = Some(m);
                    }
                });
            });
        });
        ui.add_space(4.0);
        if let Some((from, to)) = moved {
            // `active` counts the list as 0; keep it on the same object tab.
            let mut cur = pt.active.checked_sub(1).unwrap_or(usize::MAX);
            ui_kit::move_item(&mut pt.subs, from, to, &mut cur);
            if pt.active > 0 {
                pt.active = cur + 1;
            }
        }
        if let Some(i) = close {
            pt.subs.remove(i);
            if pt.active > i {
                pt.active -= 1; // closing the active tab shows its left neighbour
            }
        }
        if let Some(i) = keep_only {
            let keep = (i < pt.subs.len()).then(|| pt.subs.swap_remove(i));
            pt.subs.clear();
            pt.subs.extend(keep);
            pt.active = pt.subs.len().min(pt.active);
        }
    }

    /// Sidebar: the cluster card, pages with object counts, and the namespace filter at the bottom.
    /// Returns ((page, pin), namespace filter changed); a click previews a page, a double-click keeps it.
    fn nav(&mut self, ui: &mut Ui, out: &mut Out) -> (Option<(Page, bool)>, bool) {
        let t = ui_kit::tokens(ui);
        let mut goto = None;
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, egui::CornerRadius::same(9), t.accent_soft);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, initials(&self.kctx.name), ui_kit::semibold(13.0), t.accent);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(egui::Label::new(RichText::new(self.alias.as_deref().unwrap_or(&self.kctx.name)).font(ui_kit::semibold(14.0)).color(t.text)).truncate()).on_hover_text(&self.kctx.name);
                ui.horizontal(|ui| {
                    ui_kit::dot(ui, GREEN, 3.0);
                    ui.add(egui::Label::new(RichText::new(format!("Connected · {}", self.version)).size(ui_kit::sz(11.5)).color(t.muted)).truncate());
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui_kit::icon_button(ui, Icon::Terminal, 30.0, "Open a terminal for this context").clicked() {
                    out.term(ui.ctx(), format!("Terminal {}", self.kctx.name), tabs::local_shell(&self.kctx), None);
                }
            });
        });
        ui.add_space(10.0);
        ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), egui::Stroke::new(1.0, t.line));
        let mut ns_changed = false;
        egui::Panel::bottom(egui::Id::new(("nav-ns", self.kctx.id()))).frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(2, 8))).show(ui, |ui| {
            ns_changed = self.ns_picker(ui);
        });

        let cur = self.pages.get(self.cur).map(|t| t.page.clone());
        let has = |g: &str, k: &str| self.kinds.iter().any(|x| x.is(g, k));
        let counts = self.counts.lock().unwrap().clone();
        // Pages that list a kind count it live; the rest come from the background counts.
        let live: HashMap<(String, String), usize> = self
            .pages
            .iter()
            .filter_map(|p| p.list.as_ref())
            .filter_map(|l| {
                let d = l.data.lock().unwrap();
                d.synced.then(|| ((l.kind.ar.group.clone(), l.kind.ar.kind.clone()), d.rows.len()))
            })
            .collect();
        let count = |g: &str, k: &str| {
            let key = (g.to_string(), k.to_string());
            live.get(&key).or(counts.get(&key)).map(|n| n.to_string())
        };
        let warnings = self.warnings.as_ref().map_or(0, |l| l.grouped_len());
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.y = 1.0;
            let mut item = |ui: &mut Ui, icon: Option<Icon>, text: &str, n: Option<String>, p: Page, indent: f32| {
                let r = ui_kit::nav_item(ui, icon, text, n.as_deref(), cur.as_ref() == Some(&p), indent).on_hover_text("Click: preview · double-click: open as a tab");
                if r.double_clicked() {
                    goto = Some((p, true));
                } else if r.clicked() {
                    goto = Some((p, false));
                }
                r
            };
            item(ui, Some(Icon::Grid), "Overview", None, Page::Overview, 0.0);
            if has("", "Node") {
                item(ui, Some(Icon::Server), "Nodes", count("", "Node"), kind_page("", "Node"), 0.0);
            }
            if has("", "ConfigMap") || has("", "Secret") {
                item(ui, Some(Icon::Search), "Search configs & secrets", None, Page::Search, 0.0);
            }
            for (section, entries) in NAV {
                if nav_section(ui, section, matches!(*section, "Workloads" | "Config" | "Network")) {
                    if *section == "Workloads" {
                        item(ui, None, "Overview", None, Page::Workloads, 18.0);
                    }
                    for (g, k, text) in entries.iter().filter(|(g, k, _)| has(g, k)) {
                        item(ui, None, text, count(g, k), kind_page(g, k), 18.0);
                    }
                    if *section == "Network" {
                        item(ui, None, "Port Forwarding", Some(self.forwards.len().to_string()), Page::Forwards, 18.0);
                    }
                }
            }
            if nav_section(ui, "Custom Resources", false) {
                item(ui, None, "Definitions", None, kind_page("apiextensions.k8s.io", "CustomResourceDefinition"), 18.0);
                for (group, kinds) in &self.custom {
                    ui.horizontal(|ui| {
                        ui.add_space(28.0);
                        ui.label(RichText::new(group).size(ui_kit::sz(11.5)).color(t.dim));
                    });
                    for k in kinds {
                        item(ui, None, &k.ar.kind, None, kind_page(group, &k.ar.kind), 28.0);
                    }
                }
            }
            ui.add_space(6.0);
            ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), egui::Stroke::new(1.0, t.line));
            ui.add_space(6.0);
            item(ui, Some(Icon::Folder), "Namespaces", count("", "Namespace"), kind_page("", "Namespace"), 0.0);
            let r = item(ui, Some(Icon::Activity), "Events", None, kind_page("", "Event"), 0.0);
            if warnings > 0 {
                ui_kit::badge(ui, egui::pos2(r.rect.right() - 8.0, r.rect.center().y), &warnings.to_string(), ORANGE);
            }
            if *HELM {
                item(ui, Some(Icon::Wheel), "Helm releases", (!self.helm.rows.is_empty()).then(|| self.helm.rows.len().to_string()), Page::Helm, 0.0);
            }
            ui.add_space(8.0);
        });
        (goto, ns_changed)
    }

    /// A new tab: every view and resource kind, filtered by typing; Enter opens the first.
    fn launcher_ui(&mut self, ui: &mut Ui, pt: &mut PageTab) -> Option<Page> {
        let t = ui_kit::tokens(ui);
        let has = |g: &str, k: &str| self.kinds.iter().any(|x| x.is(g, k));
        let mut entries: Vec<(&str, String, Page)> = vec![("Cluster", "Overview".into(), Page::Overview)];
        if has("", "Node") {
            entries.push(("Cluster", "Nodes".into(), kind_page("", "Node")));
        }
        if has("", "ConfigMap") || has("", "Secret") {
            entries.push(("Cluster", "Search configs & secrets".into(), Page::Search));
        }
        entries.push(("Cluster", "Namespaces".into(), kind_page("", "Namespace")));
        entries.push(("Cluster", "Events".into(), kind_page("", "Event")));
        if *HELM {
            entries.push(("Cluster", "Helm releases".into(), Page::Helm));
        }
        entries.push(("Workloads", "Workloads overview".into(), Page::Workloads));
        for (section, items) in NAV {
            for (g, k, text) in items.iter().filter(|(g, k, _)| has(g, k)) {
                entries.push((section, text.to_string(), kind_page(g, k)));
            }
        }
        entries.push(("Network", "Port Forwarding".into(), Page::Forwards));
        for (group, kinds) in &self.custom {
            for k in kinds {
                entries.push(("Custom resources", format!("{} · {group}", k.ar.kind), kind_page(group, &k.ar.kind)));
            }
        }
        let q = pt.launch_query.trim().to_string();
        entries.retain(|(sec, name, _)| q.is_empty() || contains_ci(name, &q) || contains_ci(sec, &q));

        let mut pick = None;
        let w = 520.0_f32.min(ui.available_width());
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(w);
                ui.label(RichText::new("New tab").font(ui_kit::semibold(22.0)).color(t.text));
                ui.label(RichText::new("Open a view or a resource list here. Type to filter, Enter opens the first.").color(t.muted));
                ui.add_space(10.0);
                let r = ui_kit::search_field(ui, &mut pt.launch_query, "Pods, secrets, helm…", None, w);
                if std::mem::take(&mut self.focus_launcher) {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    pick = entries.first().map(|e| e.2.clone());
                }
                ui.add_space(8.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let mut last = "";
                    for (sec, name, page) in &entries {
                        if *sec != last {
                            ui.add_space(8.0);
                            ui_kit::section_label(ui, sec);
                            last = sec;
                        }
                        if ui_kit::nav_item(ui, None, name, None, false, 6.0).clicked() {
                            pick = Some(page.clone());
                        }
                    }
                    if entries.is_empty() {
                        ui.label(RichText::new("Nothing matches.").color(t.muted));
                    }
                });
            });
        });
        pick
    }

    /// Search inside Config Maps and Secrets; true when the namespace filter changed.
    /// A click shows the object in the side panel, a double-click opens its tab here;
    /// either way the query is carried over, so the details highlight the same text.
    fn search_page(&mut self, ui: &mut Ui, pt: &mut PageTab) -> bool {
        let ctx = ui.ctx().clone();
        let mut ns_changed = false;
        ui.horizontal(|ui| {
            ui.heading("Search in Config Maps & Secrets");
            ns_changed = self.ns_picker(ui);
        });
        let Some(cs) = &mut pt.content else { return ns_changed };
        let Some(p) = cs.ui(ui) else { return ns_changed };
        let Some(kind) = self.kind("", if p.secret { "Secret" } else { "ConfigMap" }) else { return ns_changed };
        let f = if p.tab { &mut self.find } else { &mut self.panel_find };
        (f.query, f.regex, f.case, f.open) = (cs.find.query.clone(), cs.find.regex, cs.find.case, true);
        if p.tab {
            pt.open_obj(&ctx, &self.client, kind, p.ns, p.name, true);
        } else if pt.panel.as_ref().is_none_or(|o| (&o.details.t.kind.ar, &o.details.t.ns, &o.details.t.name) != (&kind.ar, &p.ns, &p.name)) {
            let details = Details::new(&ctx, self.client.clone(), kind, p.ns, p.name.clone());
            pt.panel = Some(ObjTab { id: next_id(), title: p.name, details, data: Default::default(), uid: None, _watch: None });
            ctx.request_repaint(); // the panel is laid out before this view: show it next frame
        }
        ns_changed
    }

    /// Multi-select namespace filter; true when the selection changed.
    fn ns_picker(&mut self, ui: &mut Ui) -> bool {
        use egui::containers::menu::{MenuButton, MenuConfig};
        let text = match self.ns_sel.len() {
            0 => "All namespaces".to_string(),
            1 => self.ns_sel.first().cloned().unwrap_or_default(),
            n => format!("{n} namespaces"),
        };
        let mut changed = false;
        let config = MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        MenuButton::new(format!("📂 {text} ⏷")).config(config).ui(ui, |ui| {
            ui.set_min_width(240.0);
            if ui.selectable_label(self.ns_sel.is_empty(), "All namespaces").clicked() {
                self.ns_sel.clear();
                changed = true;
            }
            ui.separator();
            let d = self.namespaces.lock().unwrap();
            let mut names: Vec<String> = d.rows.values().map(|r| r.name.clone()).collect();
            if let Some(e) = d.visible_error() {
                ui.colored_label(RED, e).on_hover_text("Type namespaces below instead");
            }
            drop(d);
            names.extend(self.ns_sel.iter().cloned());
            names.sort();
            names.dedup();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for n in names {
                    let mut on = self.ns_sel.contains(&n);
                    if ui.checkbox(&mut on, &n).changed() {
                        if on { self.ns_sel.insert(n) } else { self.ns_sel.remove(&n) };
                        changed = true;
                    }
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.ns_input).hint_text("add namespace…").desired_width(160.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button("Add").clicked() || enter) && !self.ns_input.trim().is_empty() {
                    self.ns_sel.insert(self.ns_input.trim().to_string());
                    self.ns_input.clear();
                    changed = true;
                }
            });
        });
        changed
    }

    /// A resource list page; returns true when the namespace filter changed.
    fn list_page(&mut self, ui: &mut Ui, pt: &mut PageTab, out: &mut Out, acts: &mut Vec<(Vec<Target>, Act)>) -> bool {
        let Some(kind) = pt.list.as_ref().map(|l| l.kind.clone()) else {
            ui.label("This resource type is not available on this cluster.");
            return false;
        };
        if let Some(l) = &pt.list
            && pt.panel_restore.is_some()
            && l.data.lock().unwrap().synced
        {
            let (ns, name) = pt.panel_restore.take().unwrap_or_default();
            let data = l.data.clone();
            let row = data.lock().unwrap().rows.values().find(|r| r.namespace == ns && r.name == name).cloned();
            self.pick(ui.ctx(), pt, kind.clone(), data, row.map(Pick::Show), acts);
        }
        let t = ui_kit::tokens(ui);
        let mut ns_changed = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(label(&kind)).font(ui_kit::semibold(20.0)).color(t.text));
            if let Some(l) = &pt.list {
                let d = l.data.lock().unwrap();
                let syncing = !d.synced && d.error.is_none();
                let n = if l.view.len() == d.rows.len() { d.rows.len().to_string() } else { format!("{} / {}", l.view.len(), d.rows.len()) };
                drop(d);
                ui_kit::chip(ui, &n, ui_kit::mono(12.0));
                if syncing {
                    ui.spinner();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kind.can("create") && ui_kit::button(ui, Btn::Primary, Some(Icon::Plus), "Create").on_hover_text("Create resource from YAML").clicked() {
                    out.tab(Body::Yaml(Box::new(YamlTab::create(self.client.clone(), self.kinds.clone(), self.default_ns()))));
                }
                if ui_kit::button(ui, if self.wide { Btn::On } else { Btn::Normal }, None, "Wide").on_hover_text("Show extra columns (kubectl -o wide)").clicked() {
                    self.wide = !self.wide;
                }
                if kind.namespaced {
                    ns_changed = self.ns_picker(ui);
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if let Some(l) = &mut pt.list {
                let r = ui_kit::search_field(ui, &mut l.search, "Filter by name, label or node", Some("Ctrl F"), 280.0)
                    .on_hover_text("jpts*|ingenico*|as400* keeps whatever contains jpts, ingenico or as400 (| or, * anything)");
                if std::mem::take(&mut self.focus_filter) {
                    r.request_focus();
                }
                if ui_kit::button(ui, if l.filter_re { Btn::On } else { Btn::Normal }, None, ".*").on_hover_text("Filter with a regular expression").clicked() {
                    l.filter_re = !l.filter_re;
                }
                if let Some(e) = &l.filter_error {
                    ui.colored_label(RED, "invalid regex").on_hover_text(e);
                } else if !l.search.trim().is_empty() {
                    let (n, rows) = (l.filter_hits, l.view.len());
                    let text = format!(" {n} match{} in {rows} row{} ", if n == 1 { "" } else { "es" }, if rows == 1 { "" } else { "s" });
                    ui.label(RichText::new(text).strong().color(crate::find::HIT).background_color(crate::find::hit_bg(false).gamma_multiply(0.6)));
                }
            }
            ui.add_space(8.0);
            // A selection swaps the status filter for its actions: same row, nothing jumps.
            if !self.bulk_actions(ui, pt, acts) {
                if let Some(l) = &mut pt.list {
                    if let Some(counts) = l.status_counts() {
                        let total: usize = counts.iter().map(|c| c.1).sum();
                        egui::Frame::new().fill(t.chrome).stroke(egui::Stroke::new(1.0, t.line)).corner_radius(egui::CornerRadius::same(8)).inner_margin(egui::Margin::same(2)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if ui_kit::segment(ui, &format!("All {total}"), None, l.status.is_none()).clicked() {
                                    l.status = None;
                                }
                                for (b, n) in counts {
                                    if n == 0 && l.status != Some(b) {
                                        continue;
                                    }
                                    if ui_kit::segment(ui, &format!("{b} {n}"), Some(crate::list::bucket_color(b)), l.status == Some(b)).clicked() {
                                        l.status = if l.status == Some(b) { None } else { Some(b) };
                                    }
                                }
                            });
                        });
                    }
                }
            }
        });
        ui.add_space(6.0);
        let Some(l) = &mut pt.list else { return ns_changed };
        if let Some(e) = l.data.lock().unwrap().visible_error() {
            let hint = if e.contains("forbidden") && kind.namespaced && self.ns_sel.is_empty() { " — try selecting a namespace" } else { "" };
            ui.colored_label(RED, format!("{e}{hint}"));
        }
        let (m, data) = (self.metrics.lock().unwrap(), l.data.clone());
        let pick = l.table(ui, &m, self.wide, &mut self.find);
        drop(m);
        self.pick(ui.ctx(), pt, kind, data, pick, acts);
        ns_changed
    }

    /// Actions on the selected rows (in place of the status filter); false when nothing is selected.
    fn bulk_actions(&mut self, ui: &mut Ui, pt: &mut PageTab, acts: &mut Vec<(Vec<Target>, Act)>) -> bool {
        let Some(l) = &mut pt.list else { return false };
        let sel = l.selected();
        if sel.is_empty() {
            return false;
        }
        let kind = l.kind.clone();
        let targets: Vec<Target> = sel.iter().map(|r| Target { kind: kind.clone(), ns: r.namespace.clone(), name: r.name.clone() }).collect();
        let t = ui_kit::tokens(ui);
        ui.label(RichText::new(format!("{} selected", sel.len())).font(ui_kit::semibold(13.0)).color(t.text));
        let mut act = |a: Act| acts.push((targets.clone(), a));
        if details::has_logs(&kind) && ui_kit::button(ui, Btn::Normal, Some(Icon::Doc), "Logs").on_hover_text("One tab, lines interleaved by time").clicked() {
            act(Act::Logs(None));
        }
        if details::can_restart(&kind) && ui_kit::button(ui, Btn::Normal, None, "Restart").clicked() {
            act(Act::Restart);
        }
        if details::can_scale(&kind) {
            ui.menu_button("Scale ⏷", |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.scale_to).range(0..=1000));
                    if ui.button("Apply").clicked() {
                        act(Act::Scale(self.scale_to));
                        ui.close();
                    }
                });
            });
        }
        if ui_kit::button(ui, Btn::Normal, Some(Icon::Edit), "Edit YAML").clicked() {
            act(Act::Edit);
        }
        if ui_kit::button(ui, Btn::Normal, None, "Copy names").clicked() {
            ui.ctx().copy_text(sel.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join("\n"));
        }
        if kind.can("delete") && ui_kit::button(ui, Btn::Danger, Some(Icon::Trash), "Delete").clicked() {
            act(Act::Delete);
        }
        if ui_kit::button(ui, Btn::Ghost, Some(Icon::X), "Clear").on_hover_text("Esc").clicked() {
            l.sel.clear();
        }
        true
    }

    fn pick(&mut self, ctx: &egui::Context, pt: &mut PageTab, kind: Kind, data: Shared, pick: Option<Pick>, acts: &mut Vec<(Vec<Target>, Act)>) {
        match pick {
            // Double-click: the object's tab opens inside this page's tab.
            Some(Pick::Open(r, focus)) => pt.open_obj(ctx, &self.client, kind, r.namespace.clone(), r.name.clone(), focus),
            Some(Pick::Show(r)) => {
                if pt.panel.as_ref().is_none_or(|p| p.uid.as_ref() != Some(&r.uid)) {
                    let details = Details::new(ctx, self.client.clone(), kind, r.namespace.clone(), r.name.clone());
                    pt.panel = Some(ObjTab { id: next_id(), title: r.name.clone(), details, data, uid: Some(r.uid.clone()), _watch: None });
                }
            }
            Some(Pick::Hide) => pt.panel = None,
            Some(Pick::Menu(rows, a)) => acts.push((rows.iter().map(|r: &Arc<Row>| Target { kind: kind.clone(), ns: r.namespace.clone(), name: r.name.clone() }).collect(), a)),
            None => {}
        }
    }

    /// Cluster overview: usage cards, nodes, the busiest pods and warning events.
    /// Returns true when the namespace filter changed.
    fn overview(&mut self, ui: &mut Ui, acts: &mut Vec<(Vec<Target>, Act)>) -> bool {
        let t = ui_kit::tokens(ui);
        let mut ns_changed = false;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(self.alias.as_deref().unwrap_or(&self.kctx.name)).font(ui_kit::semibold(24.0)).color(t.text));
                    let nodes = self.metrics.lock().unwrap().alloc.len();
                    let nss = self.namespaces.lock().unwrap().rows.len();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} · {nodes} nodes · {nss} namespaces · {}", self.version, self.kctx.server)).color(t.muted));
                        if self.has_metrics {
                            ui_kit::dot(ui, GREEN, 3.0);
                            ui.label(RichText::new("metrics-server live").color(t.muted));
                        }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ns_changed = self.ns_picker(ui));
            });
            ui.add_space(14.0);

            let m = self.metrics.lock().unwrap();
            if let Some(e) = &m.error {
                ui.colored_label(RED, e);
            }
            if m.at.is_none() && m.error.is_none() {
                ui.spinner();
            }
            let (mut a, mut u) = ((0.0, 0.0, 0.0), (0.0, 0.0));
            for v in m.alloc.values() {
                a = (a.0 + v.0, a.1 + v.1, a.2 + v.2);
            }
            for v in m.nodes.values() {
                u = (u.0 + v.0, u.1 + v.1);
            }
            let (warn_rows, warn_cols) = self.warnings.as_ref().map(|l| l.grouped_rows()).unwrap_or_default();
            let col = |name: &str| warn_cols.iter().position(|c| c.name == name);
            let cell = |r: &Row, i: Option<usize>| i.and_then(|i| r.cells.get(i)).cloned().unwrap_or_default();
            let (reason_i, obj_i, msg_i, seen_i) = (col("Reason"), col("Object"), col("Message"), col("Last Seen"));
            let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
            for (r, _) in &warn_rows {
                *reasons.entry(cell(r, reason_i)).or_default() += 1;
            }
            let mut reasons: Vec<(String, usize)> = reasons.into_iter().collect();
            reasons.sort_by(|x, y| y.1.cmp(&x.1));
            let top_reasons = reasons.iter().take(3).map(|(r, n)| format!("{r} {n}")).collect::<Vec<_>>().join(" · ");

            ui.columns(4, |c| {
                if self.has_metrics {
                    let pct = |x: f64, y: f64| if y > 0.0 { (x / y) as f32 } else { 0.0 };
                    stat_card(&mut c[0], "CPU", &format!("{:.0}%", 100.0 * pct(u.0, a.0)), &format!("{} of {} cores", fmt_cpu(u.0), fmt_cpu(a.0)), Some(pct(u.0, a.0)), &format!("Allocatable on {} nodes", m.alloc.len()), false);
                    stat_card(&mut c[1], "Memory", &format!("{:.0}%", 100.0 * pct(u.1, a.1)), &format!("{} of {}", fmt_bytes(u.1), fmt_bytes(a.1)), Some(pct(u.1, a.1)), "Working set, all nodes", false);
                    stat_card(&mut c[2], "Pods", &m.pods.len().to_string(), &format!("of {} capacity", a.2), Some(pct(m.pods.len() as f64, a.2)), "Pods reporting metrics", false);
                } else {
                    stat_card(&mut c[0], "CPU", &fmt_cpu(a.0), "cores allocatable", None, "metrics-server not available", false);
                    stat_card(&mut c[1], "Memory", &fmt_bytes(a.1), "allocatable", None, "metrics-server not available", false);
                    stat_card(&mut c[2], "Pods", &format!("{}", a.2), "capacity", None, "Usage needs metrics-server", false);
                }
                let n = warn_rows.len();
                stat_card(&mut c[3], "Warnings", &n.to_string(), if n == 1 { "active warning" } else { "active warnings" }, None, if top_reasons.is_empty() { "All quiet" } else { &top_reasons }, n > 0);
            });
            ui.add_space(16.0);

            let right = (ui.available_width() * 0.36).clamp(320.0, 440.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width() - right - 16.0);
                    ui_kit::card(ui, Some(&format!("Nodes · {}", m.alloc.len())), |ui| {
                        egui::Grid::new("ov-nodes").num_columns(4).spacing([22.0, 10.0]).show(ui, |ui| {
                            for h in ["NODE", "CPU", "MEMORY", "PODS"] {
                                ui.label(RichText::new(h).font(ui_kit::semibold(11.0)).color(t.dim));
                            }
                            ui.end_row();
                            let mut names: Vec<&String> = m.alloc.keys().collect();
                            names.sort();
                            for n in names {
                                let (al, us) = (m.alloc[n], m.nodes.get(n).copied());
                                if ui.link(RichText::new(n).font(ui_kit::mono(12.5))).on_hover_text("Open node").clicked() {
                                    acts.push((vec![], Act::Open { group: String::new(), kind: "Node".into(), ns: String::new(), name: n.clone() }));
                                }
                                for (used, total) in [(us.map(|u| u.0), al.0), (us.map(|u| u.1), al.1)] {
                                    ui.horizontal(|ui| match used {
                                        Some(v) => {
                                            let f = (v / total.max(1e-9)) as f32;
                                            ui_kit::bar(ui, f, 90.0, if f > 0.85 { RED } else if f > 0.7 { ORANGE } else { t.accent });
                                            ui.label(RichText::new(format!("{:.0}%", 100.0 * f)).font(ui_kit::mono(12.0)).color(t.muted));
                                        }
                                        None => {
                                            ui.label(RichText::new("—").color(t.dim));
                                        }
                                    });
                                }
                                ui.label(RichText::new(format!("{}", al.2)).font(ui_kit::mono(12.0)).color(t.muted));
                                ui.end_row();
                            }
                        });
                    });
                    ui.add_space(16.0);
                    let mut top: Vec<(&(String, String), &(f64, f64))> = m.pods.iter().collect();
                    top.sort_by(|x, y| y.1.0.total_cmp(&x.1.0));
                    let max = top.first().map_or(1.0, |p| p.1.0.max(1e-9));
                    ui_kit::card(ui, Some("Top pods by CPU"), |ui| {
                        if top.is_empty() {
                            ui.label(RichText::new("No pod metrics yet.").color(t.muted));
                        }
                        egui::Grid::new("ov-top").num_columns(4).spacing([18.0, 8.0]).show(ui, |ui| {
                            for ((ns, name), (cpu, _)) in top.into_iter().take(6) {
                                if ui.link(RichText::new(name).font(ui_kit::mono(12.5))).on_hover_text("Open pod").clicked() {
                                    acts.push((vec![], Act::Open { group: String::new(), kind: "Pod".into(), ns: ns.clone(), name: name.clone() }));
                                }
                                ui.label(RichText::new(ns).color(t.dim));
                                ui_kit::bar(ui, (*cpu / max) as f32, 120.0, t.accent);
                                ui.label(RichText::new(fmt_cpu(*cpu)).font(ui_kit::mono(12.0)).color(t.muted));
                                ui.end_row();
                            }
                        });
                    });
                });
                ui.vertical(|ui| {
                    ui.set_width(right);
                    ui_kit::card(ui, Some("Warning events"), |ui| {
                        if warn_rows.is_empty() {
                            ui.horizontal(|ui| {
                                ui_kit::icon(ui, Icon::Check, 15.0, GREEN);
                                ui.label(RichText::new("No warning events").color(t.muted));
                            });
                        }
                        for (i, (r, n)) in warn_rows.iter().take(8).enumerate() {
                            if i > 0 {
                                ui.add_space(4.0);
                                ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), egui::Stroke::new(1.0, t.line));
                                ui.add_space(6.0);
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(cell(r, reason_i)).font(ui_kit::semibold(12.5)).color(ui_kit::status_fg(ui, ORANGE)));
                                if *n > 1 {
                                    ui_kit::chip(ui, &format!("×{n}"), ui_kit::mono(11.0));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(watch::date_cell(&cell(r, seen_i))).font(ui_kit::mono(11.5)).color(t.dim));
                                });
                            });
                            let obj = cell(r, obj_i);
                            if ui.link(RichText::new(&obj).font(ui_kit::mono(12.0))).clicked() {
                                if let Some((k, name)) = obj.split_once('/') {
                                    let kd = self.kinds.iter().find(|x| x.ar.group.is_empty() && x.ar.kind.eq_ignore_ascii_case(k)).or_else(|| self.kinds.iter().find(|x| x.ar.kind.eq_ignore_ascii_case(k)));
                                    if let Some(kd) = kd {
                                        acts.push((vec![], Act::Open { group: kd.ar.group.clone(), kind: kd.ar.kind.clone(), ns: r.namespace.clone(), name: name.to_string() }));
                                    }
                                }
                            }
                            ui.add(egui::Label::new(RichText::new(cell(r, msg_i)).size(ui_kit::sz(12.5)).color(t.muted)).wrap());
                        }
                        if warn_rows.len() > 8 {
                            ui.label(RichText::new(format!("{} more under Events", warn_rows.len() - 8)).color(t.dim));
                        }
                    });
                });
            });
        });
        ns_changed
    }

    /// Returns (page to show, namespace filter changed).
    fn workloads_ui(&mut self, ui: &mut Ui, pt: &mut PageTab) -> (Option<Page>, bool) {
        let mut goto = None;
        let mut ns_changed = false;
        ui.horizontal(|ui| {
            ui.heading("Workloads");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ns_changed = self.ns_picker(ui));
        });
        ui.add_space(6.0);
        // Frames can't be pre-measured for wrapping, so lay the cards out in rows ourselves.
        let per_row = ((ui.available_width() / 262.0) as usize).max(1);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in pt.workloads.chunks(per_row) {
                ui.horizontal_top(|ui| {
                    for l in row {
                        let d = l.data.lock().unwrap();
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(236.0);
                            ui.set_min_height(130.0);
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    if ui.link(RichText::new(format!("{} ({})", label(&l.kind), d.rows.len())).strong().size(ui_kit::sz(16.0))).clicked() {
                                        goto = Some(kind_page(&l.kind.ar.group, &l.kind.ar.kind));
                                    }
                                    if !d.synced && d.error.is_none() {
                                        ui.spinner();
                                    }
                                });
                                if let Some(e) = d.visible_error() {
                                    ui.colored_label(RED, e);
                                }
                                for (status, n) in breakdown(&d) {
                                    ui.horizontal(|ui| {
                                        watch::dot(ui, cell_color("Status", &status).unwrap_or(ui.visuals().weak_text_color()));
                                        ui.label(format!("{status}: {n}"));
                                    });
                                }
                            });
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });
        (goto, ns_changed)
    }

    fn forwards_ui(&mut self, ui: &mut Ui) {
        ui.heading("Port Forwarding");
        if self.forwards.is_empty() {
            ui.label("No active port forwards. Start one from the ports of a Pod or Service in its details.");
            return;
        }
        let mut stop = None;
        egui::Grid::new("forwards").num_columns(6).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
            for h in ["Target", "Namespace", "Remote", "Local", "Status", ""] {
                ui.label(RichText::new(h).weak());
            }
            ui.end_row();
            for (i, f) in self.forwards.iter().enumerate() {
                let local = *f.local.lock().unwrap();
                ui.label(&f.target);
                ui.label(&f.ns);
                ui.label(f.remote.to_string());
                match local {
                    Some(p) => ui.hyperlink_to(format!("localhost:{p}"), format!("http://localhost:{p}")),
                    None => ui.label("—"),
                };
                let status = f.status.lock().unwrap().clone();
                ui.colored_label(if local.is_some() { GREEN } else { RED }, status);
                if ui.button("■ Stop").clicked() {
                    stop = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = stop {
            self.forwards.remove(i); // drop kills kubectl
        }
    }

    fn helm_refresh(&mut self, ctx: &egui::Context) {
        let args = ["list", "--all-namespaces", "--output", "json"].map(String::from).to_vec();
        self.helm.load = Some(Pending::spawn(ctx, ops::helm(self.kctx.clone(), args)));
    }

    fn helm_args(&self, ns: &str, args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).chain(["--namespace".into(), ns.into()]).collect()
    }

    fn helm_poll(&mut self, ctx: &egui::Context, out: &mut Out) {
        if let Some(r) = take(&mut self.helm.load) {
            match r.and_then(|s| serde_json::from_str::<Vec<Value>>(&s).map_err(|e| e.to_string())) {
                Ok(rows) => (self.helm.rows, self.helm.error) = (rows, None),
                Err(e) => self.helm.error = Some(e),
            }
        }
        if let Some(r) = take(&mut self.helm.hist_load) {
            self.helm.history = r.ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            self.helm.history.reverse();
        }
        if let Some((title, p)) = &mut self.helm.view {
            if let Some(r) = p.poll() {
                match r {
                    Ok(text) => out.tab(Body::Yaml(Box::new(YamlTab::view(title.clone(), text)))),
                    Err(e) => out.toast(Err(e)),
                }
                self.helm.view = None;
            }
        }
        if let Some(r) = take(&mut self.helm.busy) {
            out.toast(r);
            self.helm_refresh(ctx);
            self.helm.history.clear();
        }
    }

    /// Returns true when the namespace filter changed.
    fn helm_ui(&mut self, ui: &mut Ui) -> bool {
        let ctx = ui.ctx().clone();
        let mut ns_changed = false;
        ui.horizontal(|ui| {
            ui.heading("Helm Releases");
            if ui.button("⟳ Refresh").clicked() {
                self.helm_refresh(&ctx);
            }
            if self.helm.load.is_some() || self.helm.busy.is_some() {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.helm.search).hint_text("🔍 Filter…").desired_width(200.0));
                ns_changed = self.ns_picker(ui); // releases are listed cluster-wide and filtered here
            });
        });
        if let Some(e) = &self.helm.error {
            ui.colored_label(RED, e);
        }
        let s = |v: &Value, k: &str| v[k].as_str().map(String::from).unwrap_or_else(|| v[k].to_string());
        let rows: Vec<&Value> = self
            .helm
            .rows
            .iter()
            .filter(|r| self.ns_sel.is_empty() || self.ns_sel.contains(&s(r, "namespace")))
            .filter(|r| contains_ci(&s(r, "name"), &self.helm.search) || contains_ci(&s(r, "chart"), &self.helm.search))
            .collect();
        let mut clicked = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let cols = ["name", "namespace", "chart", "app_version", "revision", "status", "updated"];
        let table_h = if self.helm.sel.is_some() { ui.available_height() * 0.5 } else { ui.available_height() };
        TableBuilder::new(ui)
            .id_salt("helm")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .max_scroll_height(table_h)
            .columns(Column::initial(140.0).clip(true), cols.len() - 1)
            .column(Column::remainder())
            .header(row_h, |mut h| {
                for c in ["Name", "Namespace", "Chart", "App version", "Revision", "Status", "Updated"] {
                    h.col(|ui| {
                        ui.strong(c);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let r = rows[row.index()];
                    let key = (s(r, "namespace"), s(r, "name"));
                    row.set_selected(self.helm.sel.as_ref() == Some(&key));
                    for c in cols {
                        row.col(|ui| {
                            let mut t = s(r, c);
                            if c == "updated" {
                                t.truncate(19);
                            }
                            let color = if c == "status" { cell_color("Status", &t) } else { None };
                            ui.add(egui::Label::new(color.map_or(RichText::new(&t), |col| RichText::new(&t).color(col))).truncate().selectable(false));
                        });
                    }
                    if row.response().clicked() {
                        clicked = Some(key);
                    }
                });
            });
        if let Some(key) = clicked {
            self.helm.history.clear();
            self.helm.hist_load = Some(Pending::spawn(&ctx, ops::helm(self.kctx.clone(), self.helm_args(&key.0, &["history", &key.1, "--output", "json"]))));
            self.helm.sel = Some(key);
        }

        let Some((ns, name)) = self.helm.sel.clone() else { return ns_changed };
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("⎈ {name}")).strong());
            for (text, args) in [("Values", vec!["get", "values", &name]), ("All values", vec!["get", "values", &name, "--all"]), ("Manifest", vec!["get", "manifest", &name]), ("Notes", vec!["get", "notes", &name])] {
                if ui.button(text).clicked() {
                    let p = Pending::spawn(&ctx, ops::helm(self.kctx.clone(), self.helm_args(&ns, &args)));
                    self.helm.view = Some((format!("{text} {name}"), p));
                }
            }
            if ui.button(RichText::new("🗑 Uninstall").color(RED)).clicked() {
                self.confirm = Some((format!("Uninstall Helm release {name} from {ns}?"), Confirm::Helm(format!("Uninstalled {name}"), self.helm_args(&ns, &["uninstall", &name]))));
            }
            if ui.button("×").clicked() {
                self.helm.sel = None;
            }
        });
        ui.label(RichText::new("History").weak());
        egui::ScrollArea::vertical().id_salt("helm-hist").show(ui, |ui| {
            egui::Grid::new("helm-history").num_columns(6).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                for h in ["Revision", "Updated", "Status", "Chart", "Description", ""] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                let latest = self.helm.history.first().map(|h| h["revision"].clone());
                for h in self.helm.history.clone() {
                    let rev = h["revision"].to_string();
                    ui.label(&rev);
                    ui.label(s(&h, "updated").chars().take(19).collect::<String>());
                    ui.colored_label(cell_color("Status", &s(&h, "status")).unwrap_or(ui.visuals().text_color()), s(&h, "status"));
                    ui.label(s(&h, "chart"));
                    ui.label(s(&h, "description"));
                    if Some(&h["revision"]) != latest.as_ref() && ui.small_button("↺ Rollback").clicked() {
                        self.confirm = Some((format!("Roll back {name} to revision {rev}?"), Confirm::Helm(format!("Rolled back {name} to {rev}"), self.helm_args(&ns, &["rollback", &name, &rev]))));
                    }
                    ui.end_row();
                }
            });
        });
        ns_changed
    }

    /// Entry point for every action: confirmation when destructive, fan-out for several targets.
    fn act(&mut self, ctx: &egui::Context, targets: Vec<Target>, a: Act, out: &mut Out) {
        if let Act::Open { group, kind, ns, name } = &a {
            match self.kind(group, kind) {
                Some(k) => self.open_in_kind_tab(ctx, k, ns.clone(), name.clone()),
                None => out.toast(Err(format!("{kind} is not available on this cluster"))),
            }
            return;
        }
        let Some(first) = targets.first() else { return };
        if a == Act::NodeShell {
            let msg = format!("Open a terminal on node {}?\n\nA privileged pod is created in kube-system on that node and deleted when the terminal closes.", first.name);
            self.confirm = Some((msg, Confirm::Act(targets, a)));
        } else if a.needs_confirm() {
            let what = match targets.as_slice() {
                [t] => format!("{} {}{}", t.kind.ar.kind, t.name, if t.ns.is_empty() { String::new() } else { format!(" in {}", t.ns) }),
                ts => {
                    let names: Vec<&str> = ts.iter().take(10).map(|t| t.name.as_str()).collect();
                    format!("{} {}s: {}{}", ts.len(), first.kind.ar.kind, names.join(", "), if ts.len() > 10 { ", …" } else { "" })
                }
            };
            let verb = a.label().trim_start_matches(|c: char| !c.is_alphabetic()).to_string();
            self.confirm = Some((format!("{verb} {what}?"), Confirm::Act(targets, a)));
        } else {
            self.exec(ctx, targets, a, out);
        }
    }

    fn exec(&mut self, ctx: &egui::Context, targets: Vec<Target>, a: Act, out: &mut Out) {
        let c = self.client.clone();
        match &a {
            Act::Details => {
                let focus = targets.len() == 1;
                for t in targets.into_iter().take(20) {
                    self.open_obj(ctx, self.cur, t.kind, t.ns, t.name, focus);
                }
            }
            Act::Edit => {
                for t in targets.into_iter().take(20) {
                    out.tab(Body::Yaml(Box::new(YamlTab::edit(ctx, c.clone(), t.kind.ar, t.ns, t.name))));
                }
            }
            Act::Logs(container) => {
                let t: Vec<_> = targets.into_iter().map(|t| (t.kind, t.ns, t.name)).collect();
                out.tab(Body::Logs(Box::new(LogTab::new(ctx, c, t, container.clone()))));
            }
            Act::Shell(container) => {
                for t in targets.iter().take(1) {
                    out.term(ctx, format!("Shell {}", t.name), tabs::pod_shell(&self.kctx, &t.ns, &t.name, container.as_deref()), None);
                }
            }
            Act::Drain => {
                for t in targets.iter().take(1) {
                    out.term(ctx, format!("Drain {}", t.name), tabs::drain(&self.kctx, &t.name), None);
                }
            }
            Act::NodeShell => {
                for t in targets.iter().take(1) {
                    let job = ops::node_shell(c.clone(), t.name.clone(), out.node_image.clone());
                    self.node_shells.push((t.name.clone(), Pending::spawn(ctx, job)));
                    out.toast(Ok(format!("Starting a shell on {}…", t.name)));
                }
            }
            Act::Forward(target, port) => {
                for t in targets.iter().take(1) {
                    self.forwards.push(ops::port_forward(&self.kctx, &t.ns, target, *port, ctx));
                    out.toast(Ok(format!("Forwarding {target}:{port}; see Network › Port Forwarding")));
                }
            }
            _ => {
                let mut jobs: Vec<Job> = targets.iter().filter_map(|t| self.job(t, &a)).collect();
                let job = match jobs.len() {
                    0 => return,
                    1 => jobs.remove(0),
                    n => {
                        // One summary toast instead of one per object.
                        let verb = a.label().trim_start_matches(|c: char| !c.is_alphabetic()).to_string();
                        Box::pin(ops::all(jobs, format!("{verb}: {n} {}s", targets[0].kind.ar.kind)))
                    }
                };
                self.actions.push(Pending::spawn(ctx, job));
            }
        }
    }

    /// The API call behind one API action on one object.
    fn job(&self, t: &Target, a: &Act) -> Option<Job> {
        let (c, ar, ns, name) = (self.client.clone(), t.kind.ar.clone(), t.ns.clone(), t.name.clone());
        let patch = |p: Value, done: String| -> Job { Box::pin(ops::merge_patch(c.clone(), ar.clone(), ns.clone(), name.clone(), p, done)) };
        Some(match a {
            Act::Delete => Box::pin(ops::delete(c.clone(), ar.clone(), ns.clone(), name.clone())),
            Act::Restart => patch(ops::restart_patch(), format!("Restarting {name}")),
            Act::Scale(n) => patch(ops::scale_patch(*n), format!("Scaled {name} to {n}")),
            Act::Cordon(b) => patch(json!({"spec": {"unschedulable": b}}), format!("{} {name}", if *b { "Cordoned" } else { "Uncordoned" })),
            Act::Suspend(b) => patch(json!({"spec": {"suspend": b}}), format!("{} {name}", if *b { "Suspended" } else { "Resumed" })),
            Act::Trigger => Box::pin(ops::trigger_cronjob(c.clone(), ns.clone(), name.clone())),
            _ => return None,
        })
    }

    fn confirm_ui(&mut self, ctx: &egui::Context, out: &mut Out) {
        let Some((msg, _)) = &self.confirm else { return };
        let mut answer = None;
        let resp = egui::Modal::new(egui::Id::new(("confirm", self.kctx.id()))).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading("Please confirm");
            ui.add(egui::Label::new(msg).wrap());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Confirm").color(RED).strong()).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if resp.should_close() && answer.is_none() {
            answer = Some(false);
        }
        let Some(ok) = answer else { return };
        let Some((_, c)) = self.confirm.take() else { return };
        if ok {
            match c {
                Confirm::Act(t, a) => self.exec(ctx, t, a, out),
                Confirm::Helm(done, args) => {
                    let kctx = self.kctx.clone();
                    self.helm.busy = Some(Pending::spawn(ctx, async move { ops::helm(kctx, args).await.map(|_| done) }));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_initials() {
        assert_eq!(initials("K8S-QA"), "QA");
        assert_eq!(initials("rancher-desktop"), "RD");
        assert_eq!(initials("prod"), "PR");
        assert_eq!(initials("k3s"), "K3");
    }

    #[test]
    fn sidebar_navigation() {
        let (pods, deps, nodes) = (kind_page("", "Pod"), kind_page("apps", "Deployment"), kind_page("", "Node"));
        assert_eq!(nav_target(&[(&pods, false)], &pods, false), NavTo::Focus(0)); // already open
        assert_eq!(nav_target(&[(&pods, false), (&deps, true)], &nodes, false), NavTo::Replace(1)); // reuse the preview
        assert_eq!(nav_target(&[(&pods, false), (&deps, true)], &nodes, true), NavTo::Push); // double-click: own tab
        assert_eq!(nav_target(&[(&pods, false)], &nodes, false), NavTo::Push); // no preview yet
        assert_eq!(nav_target(&[(&pods, false), (&deps, true)], &deps, true), NavTo::Focus(1)); // pinning the preview
    }

    #[test]
    fn saved_tabs_round_trip() {
        let s = SavedTabs {
            pages: vec![SavedPage {
                page: kind_page("", "Pod"),
                preview: false,
                name: Some("web pods".into()),
                objs: vec![("".into(), "Pod".into(), "default".into(), "web-1".into())],
                active: 1,
                sections: vec![details::Section::Yaml],
                view: SavedView { search: "web".into(), re: true, sort: Some((crate::list::DCol::Cell(2), false)), status: Some("Running".into()), panel: Some(("default".into(), "web-2".into())) },
            }],
            cur: 0,
            ns: vec!["default".into()],
            wide: true,
        };
        assert_eq!(serde_json::from_str::<SavedTabs>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
        // saves from before these fields still load
        let old = r#"{"pages":[{"page":"Overview","preview":true,"objs":[],"active":0}],"cur":0,"ns":[]}"#;
        assert_eq!(serde_json::from_str::<SavedTabs>(old).unwrap().pages[0].view, SavedView::default());
    }

    #[test]
    fn workload_breakdown() {
        let mut d = ListData::default();
        d.cols = ["Name", "Ready"].map(|n| watch::Col { name: n.into(), ..Default::default() }).to_vec();
        for (i, ready) in ["1/1", "0/1", "2/2"].iter().enumerate() {
            let r = Row { name: format!("d{i}"), namespace: "x".into(), uid: i.to_string(), rv: "1".into(), created: None, deleting: false, cells: vec![format!("d{i}"), ready.to_string()], images: vec![] };
            d.rows.insert(r.uid.clone(), Arc::new(r));
        }
        assert_eq!(breakdown(&d), vec![("Ready".to_string(), 2), ("Not ready".to_string(), 1)]);
    }
}
