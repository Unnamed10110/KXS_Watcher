//! One cluster tab: connect + discovery, sidebar navigation, resource lists with object sub-tabs,
//! overview pages, Helm releases and port forwards.
use std::collections::{BTreeMap, BTreeSet, HashSet};
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
use crate::tabs::{self, LogTab, YamlTab};
use crate::watch::{self, cell_color, contains_ci, take, Bg, ListData, Pending, Row, Shared, GREEN, RED};
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
}

fn kind_page(g: &str, k: &str) -> Page {
    Page::Kind(g.into(), k.into())
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
    conn: Option<Pending<Res<Conn>>>,
    error: Option<String>,
    ready: Option<Box<Ready>>,
    /// Saved tabs to reopen once connected.
    restore: Option<SavedTabs>,
}

impl ClusterTab {
    pub fn new(ctx: &egui::Context, kctx: Ctx, restore: Option<SavedTabs>) -> Self {
        let conn = Some(Pending::spawn(ctx, connect(kctx.clone())));
        ClusterTab { kctx, conn, error: None, ready: None, restore }
    }

    /// Open tabs to save; the pending restore while still connecting.
    pub fn saved(&self) -> Option<SavedTabs> {
        match &self.ready {
            Some(r) => Some(r.saved()),
            None => self.restore.clone(),
        }
    }

    /// Ctrl+K: find in the view. Ctrl+F (`filter`): focus the list's filter box (find elsewhere).
    pub fn open_find(&mut self, filter: bool) {
        if let Some(r) = &mut self.ready {
            let page = r.pages.get_mut(r.cur).filter(|p| p.active == 0);
            let on_list = page.as_ref().is_some_and(|p| matches!(p.page, Page::Kind(..)));
            if let Some(cs) = page.and_then(|p| p.content.as_mut()) {
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SavedPage {
    page: Page,
    preview: bool,
    /// Object tabs: (group, kind, namespace, name).
    objs: Vec<(String, String, String, String)>,
    active: usize,
}

/// A page tab (Pods, Deployments, Overview…) with its own list and the object tabs opened from it.
struct PageTab {
    id: u64,
    page: Page,
    /// Replaced by the next single click in the sidebar (shown in italics).
    preview: bool,
    list: Option<List>,
    events: Option<List>,
    workloads: Vec<List>,
    /// Object tabs; `active` 0 is the page view, i > 0 is `subs[i - 1]`.
    subs: Vec<ObjTab>,
    active: usize,
    /// Details of the clicked row, right of the list.
    panel: Option<ObjTab>,
    content: Option<ContentSearch>,
}

impl PageTab {
    fn new(page: Page, preview: bool) -> Self {
        PageTab { id: next_id(), page, preview, list: None, events: None, workloads: vec![], subs: vec![], active: 0, panel: None, content: None }
    }

    /// (Re)start the watches this page needs; also after a namespace change.
    fn load(&mut self, ctx: &egui::Context, client: &Client, kinds: &[Kind], ns_sel: &BTreeSet<String>) {
        let kind = |g: &str, k: &str| kinds.iter().find(|x| x.is(g, k)).cloned();
        let keep = self.list.take().map(|l| (l.kind.ar.clone(), l.search, l.filter_re, l.sort));
        self.panel = None;
        self.events = None;
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
            Page::Overview => self.events = kind("", "Event").map(|k| List::new(ctx, client, k, ns_sel, Some("type=Warning"))),
            Page::Workloads => self.workloads = WORKLOADS.iter().filter_map(|(g, k)| kind(g, k)).map(|k| List::new(ctx, client, k, ns_sel, None)).collect(),
            Page::Search => {
                let ns = ns_sel.iter().cloned().collect();
                match &mut self.content {
                    Some(c) => c.reload(ctx, Some(ns)), // keeps the query
                    None => self.content = Some(ContentSearch::new(ctx, client.clone(), ns)),
                }
            }
            Page::Helm | Page::Forwards => {}
        }
    }

    fn title(&self) -> String {
        match &self.page {
            Page::Overview => "📊 Overview".into(),
            Page::Workloads => "Workloads".into(),
            Page::Forwards => "Port Forwarding".into(),
            Page::Helm => "⎈ Helm Releases".into(),
            Page::Search => "🔍 Search".into(),
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
    panel_find: Find,
    find: Find,
    /// Ctrl+F: focus the filter box next frame.
    focus_filter: bool,
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
            panel_find: Find::default(),
            find: Find::default(),
            focus_filter: false,
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
                for (g, k, ns, name) in sp.objs {
                    if let Some(kind) = r.kind(&g, &k) {
                        t.open_obj(ctx, &r.client, kind, ns, name, false);
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
        r.update_metrics(ctx);
        r
    }

    fn saved(&self) -> SavedTabs {
        let pages = self
            .pages
            .iter()
            .map(|t| SavedPage {
                page: t.page.clone(),
                preview: t.preview,
                active: t.active,
                objs: t.subs.iter().map(|o| (o.details.t.kind.ar.group.clone(), o.details.t.kind.ar.kind.clone(), o.details.t.ns.clone(), o.details.t.name.clone())).collect(),
            })
            .collect();
        SavedTabs { pages, cur: self.cur, ns: self.ns_sel.iter().cloned().collect() }
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

    /// Sidebar click: focus an open page, else preview it (single) or open its own tab (double).
    fn show_page(&mut self, ctx: &egui::Context, page: Page, pin: bool) {
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

        let mut nav = None;
        egui::Panel::left(egui::Id::new(("nav", self.kctx.id()))).resizable(true).default_size(210.0).size_range(150.0..=400.0).show(ui, |ui| nav = self.nav(ui, out));
        if let Some((p, pin)) = nav {
            self.show_page(&ctx, p, pin);
        }

        // Page views need `&mut self` too: draw them with the page tabs taken out, put them back after.
        let (mut acts, mut ev) = (Vec::new(), PageEv::default());
        let mut pages = std::mem::take(&mut self.pages);
        egui::Panel::top(egui::Id::new(("tabs", self.kctx.id()))).show(ui, |ui| {
            self.page_tabs_ui(ui, &mut pages);
            if let Some(pt) = pages.get_mut(self.cur) {
                self.subtabs_ui(ui, pt);
            }
            self.find.bar(ui, |_| false);
        });
        match pages.get_mut(self.cur) {
            Some(pt) => {
                self.panel_ui(ui, pt, &mut acts);
                egui::CentralPanel::default().show(ui, |ui| self.page_ui(ui, pt, out, &mut acts, &mut ev));
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

        if ev.ns_changed {
            for pt in &mut self.pages {
                pt.load(&ctx, &self.client, &self.kinds, &self.ns_sel);
            }
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
                Page::Overview => self.overview(ui, pt, acts),
                Page::Workloads => {
                    let (goto, changed) = self.workloads_ui(ui, pt);
                    ev.goto = goto;
                    ev.ns_changed |= changed;
                }
                Page::Forwards => self.forwards_ui(ui),
                Page::Helm => ev.ns_changed |= self.helm_ui(ui),
                Page::Search => ev.ns_changed |= self.search_page(ui, pt),
            }
        } else if let Some(t) = pt.subs.get_mut(pt.active - 1) {
            let mut dacts = vec![];
            // Object tabs share widget ids (grids, collapsing headers): keep their state apart.
            ui.push_id(t.id, |ui| t.details.ui(ui, &self.metrics.lock().unwrap(), &mut dacts, &mut self.find));
            acts.extend(dacts.into_iter().map(|a| (vec![t.details.t.clone()], a)));
        }
    }

    /// Details of the clicked row, right of the list (only on the page view).
    fn panel_ui(&mut self, ui: &mut Ui, pt: &mut PageTab, acts: &mut Vec<(Vec<Target>, Act)>) {
        if pt.active != 0 {
            return;
        }
        let Some(p) = &mut pt.panel else { return };
        let (mut close, mut to_tab, mut dacts) = (false, false, vec![]);
        let max = (ui.available_width() - 360.0).max(320.0);
        egui::Panel::right(egui::Id::new(("details", self.kctx.id(), pt.id))).resizable(true).default_size(480.0).size_range(320.0..=max).show(ui, |ui| {
            ui.horizontal(|ui| {
                to_tab = ui.button("↗ Open in tab").on_hover_text("Open in its own tab (double-click a row does the same)").clicked();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| close = ui.button("×").on_hover_text("Close (Esc)").clicked());
            });
            ui.push_id(p.id, |ui| p.details.ui(ui, &self.metrics.lock().unwrap(), &mut dacts, &mut self.panel_find));
        });
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
        ui.scope(|ui| {
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal().id_salt(("pagetabs", self.kctx.id())).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, t) in pages.iter_mut().enumerate() {
                        if i > 0 {
                            ui.separator();
                        }
                        let mut text = RichText::new(t.title());
                        if t.preview {
                            text = text.italics();
                        }
                        if i == self.cur {
                            text = text.strong();
                        }
                        let tip = if t.preview { "Preview: the next single click in the sidebar replaces it. Double-click to keep it." } else { "Middle-click to close" };
                        let r = ui.selectable_label(i == self.cur, text).on_hover_text(tip);
                        if r.double_clicked() {
                            t.preview = false;
                        }
                        if r.clicked() {
                            self.cur = i;
                        }
                        if r.middle_clicked() || ui.small_button("×").clicked() {
                            close = Some(i);
                        }
                        r.context_menu(|ui| {
                            if t.preview && ui.button("Keep open").clicked() {
                                t.preview = false;
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
                });
            });
        });
        if let Some(i) = close {
            pages.remove(i);
            if self.cur > i || self.cur >= pages.len() {
                self.cur = self.cur.saturating_sub(1); // closing the current tab shows its left neighbour
            }
        }
        if let Some(i) = keep_only {
            let keep = (i < pages.len()).then(|| pages.swap_remove(i));
            pages.clear();
            pages.extend(keep);
            self.cur = 0;
        }
    }

    /// Second tab row: the page's list plus the objects opened from it.
    fn subtabs_ui(&mut self, ui: &mut Ui, pt: &mut PageTab) {
        if pt.subs.is_empty() {
            return;
        }
        ui.separator();
        let title = pt.title();
        let (mut close, mut keep_only) = (None, None);
        ui.scope(|ui| {
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal().id_salt(("subtabs", self.kctx.id(), pt.id)).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(pt.active == 0, RichText::new(format!("☰ {title}")).strong()).clicked() {
                        pt.active = 0;
                    }
                    for (i, t) in pt.subs.iter().enumerate() {
                        ui.separator();
                        let r = ui.selectable_label(pt.active == i + 1, &t.title).on_hover_text("Middle-click to close");
                        if r.clicked() {
                            pt.active = i + 1;
                        }
                        if r.middle_clicked() || ui.small_button("×").clicked() {
                            close = Some(i);
                        }
                        r.context_menu(|ui| {
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
                });
            });
        });
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

    /// Sidebar; returns (page, pin): a single click previews, a double-click keeps a tab.
    fn nav(&mut self, ui: &mut Ui, out: &mut Out) -> Option<(Page, bool)> {
        let mut goto = None;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.kctx.name).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⌨").on_hover_text("Open a terminal for this context").clicked() {
                    out.term(ui.ctx(), format!("Terminal {}", self.kctx.name), tabs::local_shell(&self.kctx), None);
                }
            });
        });
        ui.label(RichText::new(&self.version).weak().small());
        ui.separator();
        let cur = self.pages.get(self.cur).map(|t| t.page.clone());
        let has = |g: &str, k: &str| self.kinds.iter().any(|x| x.is(g, k));
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut item = |ui: &mut Ui, text: &str, p: Page| {
                let r = ui.selectable_label(cur.as_ref() == Some(&p), text).on_hover_text("Click: preview · double-click: open as a tab");
                if r.double_clicked() {
                    goto = Some((p, true));
                } else if r.clicked() {
                    goto = Some((p, false));
                }
            };
            item(ui, "📊 Overview", Page::Overview);
            if has("", "Node") {
                item(ui, "🖥 Nodes", kind_page("", "Node"));
            }
            if has("", "ConfigMap") || has("", "Secret") {
                item(ui, "🔍 Search configs & secrets", Page::Search);
            }
            for (section, entries) in NAV {
                egui::CollapsingHeader::new(*section).default_open(*section == "Workloads").show(ui, |ui| {
                    if *section == "Workloads" {
                        item(ui, "Overview", Page::Workloads);
                    }
                    for (g, k, text) in entries.iter().filter(|(g, k, _)| has(g, k)) {
                        item(ui, text, kind_page(g, k));
                    }
                    if *section == "Network" {
                        item(ui, &format!("Port Forwarding ({})", self.forwards.len()), Page::Forwards);
                    }
                });
            }
            item(ui, "📂 Namespaces", kind_page("", "Namespace"));
            item(ui, "🔔 Events", kind_page("", "Event"));
            if *HELM {
                item(ui, "⎈ Helm Releases", Page::Helm);
            }
            egui::CollapsingHeader::new("Custom Resources").show(ui, |ui| {
                item(ui, "Definitions", kind_page("apiextensions.k8s.io", "CustomResourceDefinition"));
                for (group, kinds) in &self.custom {
                    egui::CollapsingHeader::new(group).id_salt(("crd-group", group)).show(ui, |ui| {
                        for k in kinds {
                            item(ui, &k.ar.kind, kind_page(group, &k.ar.kind));
                        }
                    });
                }
            });
        });
        goto
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
        let mut ns_changed = false;
        // Wraps instead of overflowing on narrow windows.
        ui.horizontal_wrapped(|ui| {
            ui.heading(label(&kind));
            let mut syncing = false;
            if let Some(l) = &pt.list {
                let d = l.data.lock().unwrap();
                syncing = !d.synced && d.error.is_none();
                // Fixed width: the namespace menu must not move while it is open.
                let count = RichText::new(format!("{} / {}", l.view.len(), d.rows.len())).weak();
                ui.add_sized([96.0, ui.spacing().interact_size.y], egui::Label::new(count));
            }
            if kind.namespaced {
                ns_changed = self.ns_picker(ui);
            }
            if let Some(l) = &mut pt.list {
                let r = ui.add(egui::TextEdit::singleline(&mut l.search).hint_text("🔍 Filter… (Ctrl+F)").desired_width(200.0));
                if std::mem::take(&mut self.focus_filter) {
                    r.request_focus();
                }
                ui.toggle_value(&mut l.filter_re, ".*").on_hover_text("Filter with a regular expression");
                if let Some(e) = &l.filter_error {
                    ui.colored_label(RED, "invalid regex").on_hover_text(e);
                }
            }
            ui.toggle_value(&mut self.wide, "Wide").on_hover_text("Show extra columns (kubectl -o wide)");
            if kind.can("create") && ui.button("➕ Create").on_hover_text("Create resource from YAML").clicked() {
                out.tab(Body::Yaml(Box::new(YamlTab::create(self.client.clone(), self.kinds.clone(), self.default_ns()))));
            }
            if syncing {
                ui.spinner();
            }
        });
        self.bulk_bar(ui, pt, acts);
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

    /// Actions on the selected rows.
    fn bulk_bar(&mut self, ui: &mut Ui, pt: &mut PageTab, acts: &mut Vec<(Vec<Target>, Act)>) {
        let Some(l) = &mut pt.list else { return };
        let sel = l.selected();
        // Always takes the same space: rows must not jump under the pointer when a selection starts.
        if sel.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_height(ui.spacing().interact_size.y);
                ui.label(RichText::new("Click shows details · double-click opens a tab · checkboxes, Ctrl/Shift+click or Ctrl+A select for bulk actions").weak());
            });
            return;
        }
        let kind = l.kind.clone();
        let targets: Vec<Target> = sel.iter().map(|r| Target { kind: kind.clone(), ns: r.namespace.clone(), name: r.name.clone() }).collect();
        let mut clear = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{} selected", sel.len())).strong());
                let mut act = |a: Act| acts.push((targets.clone(), a));
                if details::has_logs(&kind) && ui.button("📜 Logs").on_hover_text("One tab, lines interleaved by time").clicked() {
                    act(Act::Logs(None));
                }
                if details::can_restart(&kind) && ui.button("⟳ Restart").clicked() {
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
                if ui.button("✏ Edit YAML").clicked() {
                    act(Act::Edit);
                }
                if ui.button("Copy names").clicked() {
                    ui.ctx().copy_text(sel.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join("\n"));
                }
                if kind.can("delete") && ui.button(RichText::new("🗑 Delete").color(RED)).clicked() {
                    act(Act::Delete);
                }
                clear = ui.button("× Clear").on_hover_text("Esc").clicked();
            });
        });
        if clear {
            l.sel.clear();
        }
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

    fn overview(&mut self, ui: &mut Ui, pt: &mut PageTab, acts: &mut Vec<(Vec<Target>, Act)>) {
        ui.horizontal(|ui| {
            ui.heading(&self.kctx.name);
            ui.label(RichText::new(format!("{} · {}", self.kctx.server, self.version)).weak());
        });
        ui.add_space(6.0);
        {
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
            ui.columns(3, |cols| {
                let bar = |ui: &mut Ui, title: &str, used: f64, total: f64, text: String| {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(RichText::new(title).strong());
                        let frac = if total > 0.0 { (used / total) as f32 } else { 0.0 };
                        ui.add(egui::ProgressBar::new(frac).text(text).fill(if frac > 0.85 { RED } else { GREEN }.gamma_multiply(0.8)));
                    });
                };
                if self.has_metrics {
                    bar(&mut cols[0], "CPU", u.0, a.0, format!("{} / {} cores ({:.0}%)", fmt_cpu(u.0), fmt_cpu(a.0), 100.0 * u.0 / a.0.max(1e-9)));
                    bar(&mut cols[1], "Memory", u.1, a.1, format!("{} / {} ({:.0}%)", fmt_bytes(u.1), fmt_bytes(a.1), 100.0 * u.1 / a.1.max(1.0)));
                    bar(&mut cols[2], "Pods", m.pods.len() as f64, a.2, format!("{} / {}", m.pods.len(), a.2));
                } else {
                    cols[0].label(format!("{} nodes · {} cores · {} memory allocatable", m.alloc.len(), fmt_cpu(a.0), fmt_bytes(a.1)));
                    cols[1].label(RichText::new("metrics-server not available: usage is hidden").weak());
                }
            });
            ui.add_space(6.0);
            egui::CollapsingHeader::new(format!("Nodes ({})", m.alloc.len())).default_open(true).show(ui, |ui| {
                egui::Grid::new("ov-nodes").num_columns(4).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                    for h in ["Node", "CPU", "Memory", "Pods (allocatable)"] {
                        ui.label(RichText::new(h).weak());
                    }
                    ui.end_row();
                    let mut names: Vec<&String> = m.alloc.keys().collect();
                    names.sort();
                    for n in names {
                        let (a, u) = (m.alloc[n], m.nodes.get(n).copied());
                        if ui.link(n).on_hover_text("Open node").clicked() {
                            acts.push((vec![], Act::Open { group: String::new(), kind: "Node".into(), ns: String::new(), name: n.clone() }));
                        }
                        for (used, total, text) in [(u.map(|u| u.0), a.0, fmt_cpu(a.0)), (u.map(|u| u.1), a.1, fmt_bytes(a.1))] {
                            match used {
                                Some(v) => ui.add(egui::ProgressBar::new((v / total.max(1e-9)) as f32).desired_width(180.0).text(format!("{:.0}% of {text}", 100.0 * v / total.max(1e-9)))),
                                None => ui.label(text),
                            };
                        }
                        ui.label(format!("{}", a.2));
                        ui.end_row();
                    }
                });
            });
        }
        ui.add_space(6.0);
        ui.label(RichText::new("⚠ Warning events (identical ones grouped)").strong());
        let m = self.metrics.lock().unwrap();
        let picked = pt.events.as_mut().map(|l| (l.kind.clone(), l.data.clone(), l.table(ui, &m, false, &mut self.find)));
        drop(m);
        if let Some((kind, data, pick)) = picked {
            self.pick(ui.ctx(), pt, kind, data, pick, acts);
        }
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
                                    if ui.link(RichText::new(format!("{} ({})", label(&l.kind), d.rows.len())).strong().size(16.0)).clicked() {
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
            pages: vec![SavedPage { page: kind_page("", "Pod"), preview: false, objs: vec![("".into(), "Pod".into(), "default".into(), "web-1".into())], active: 1 }],
            cur: 0,
            ns: vec!["default".into()],
        };
        assert_eq!(serde_json::from_str::<SavedTabs>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
    }

    #[test]
    fn workload_breakdown() {
        let mut d = ListData::default();
        d.cols = ["Name", "Ready"].map(|n| watch::Col { name: n.into(), ..Default::default() }).to_vec();
        for (i, ready) in ["1/1", "0/1", "2/2"].iter().enumerate() {
            let r = Row { name: format!("d{i}"), namespace: "x".into(), uid: i.to_string(), rv: "1".into(), created: None, deleting: false, cells: vec![format!("d{i}"), ready.to_string()] };
            d.rows.insert(r.uid.clone(), Arc::new(r));
        }
        assert_eq!(breakdown(&d), vec![("Ready".to_string(), 2), ("Not ready".to_string(), 1)]);
    }
}
