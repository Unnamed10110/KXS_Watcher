//! A live, filterable, sortable, multi-select table of one resource kind.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use kube::Client;

use crate::details::{row_actions, Act};
use crate::find::{self, Find, Matcher};
use crate::ops::{fmt_bytes, fmt_cpu, Kind, Metrics};
use crate::ui_kit;
use crate::watch::{self, cell_color, cmp_cells, Bg, ListData, Row, Shared, GREEN, ORANGE, RED};

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum DCol {
    Cell(usize),
    Ns,
    Age,
    Cpu,
    Mem,
    /// Container images (registry/repo:tag): pods from their object, workloads from their "Images" column.
    Images,
}

/// What the user did with rows this frame.
pub enum Pick {
    /// Double-click (focus the new tab) or middle-click (open in the background).
    Open(Arc<Row>, bool),
    /// Context-menu action on the clicked row, or on the whole selection when the row is part of it.
    Menu(Vec<Arc<Row>>, Act),
    /// Plain / Ctrl / Shift click: show this row's details.
    Show(Arc<Row>),
    /// Esc: selection cleared, hide the details.
    Hide,
}

pub struct List {
    pub kind: Kind,
    pub data: Shared,
    _watches: Vec<Bg>,
    pub view: Vec<Arc<Row>>,
    key: Option<(u64, String, bool, Option<(DCol, bool)>, Option<std::time::Instant>, Option<&'static str>)>,
    pub search: String,
    /// The filter box is a regular expression.
    pub filter_re: bool,
    pub filter_error: Option<String>,
    /// Pods: show only this status bucket (`pod_bucket`).
    pub status: Option<&'static str>,
    /// The filter as a matcher (to highlight it in the cells) and its matches in the rows shown.
    filter_m: Option<Matcher>,
    pub filter_hits: usize,
    pub sort: Option<(DCol, bool)>,
    /// Selected rows by uid (survives re-sorts and watch updates).
    pub sel: HashSet<String>,
    anchor: Option<String>,
    /// Events: how many identical events each shown row stands for.
    groups: HashMap<String, usize>,
    find_rows: Vec<usize>,
    find_key: Option<(u64, String, bool, bool, usize)>,
    /// (data rev, distinct events) for `grouped_len`.
    grouped: std::cell::Cell<(u64, usize)>,
}

/// Status filter buckets for pods, in display order.
pub const POD_BUCKETS: [&str; 4] = ["Running", "Pending", "Failing", "Completed"];

/// Which bucket a pod's Status column falls in.
pub fn pod_bucket(status: &str) -> &'static str {
    match status {
        "Running" => "Running",
        "Completed" | "Succeeded" => "Completed",
        s if s == "Pending" || s == "ContainerCreating" || s == "PodInitializing" || s == "Terminating" || s.starts_with("Init:") => "Pending",
        _ => "Failing",
    }
}

pub fn bucket_color(b: &str) -> egui::Color32 {
    match b {
        "Running" => GREEN,
        "Pending" => ORANGE,
        "Failing" => RED,
        _ => egui::Color32::from_rgb(128, 136, 150),
    }
}

fn usage(kind: &Kind, m: &Metrics, r: &Row) -> Option<(f64, f64)> {
    if kind.is("", "Node") { m.nodes.get(&r.name).copied() } else { m.pods.get(&(r.namespace.clone(), r.name.clone())).copied() }
}

fn is_event(kind: &Kind) -> bool {
    kind.ar.kind == "Event"
}

/// Collapse identical events (same namespace, type, reason, object and message) into their newest row.
fn group_events(rows: Vec<Arc<Row>>, cols: &[watch::Col]) -> (Vec<Arc<Row>>, HashMap<String, usize>) {
    let idx: Vec<usize> = ["Type", "Reason", "Object", "Message"].iter().filter_map(|n| cols.iter().position(|c| c.name == *n)).collect();
    let mut best: HashMap<Vec<&str>, (Arc<Row>, usize)> = HashMap::new();
    for r in &rows {
        let mut k: Vec<&str> = vec![&r.namespace];
        k.extend(idx.iter().map(|&i| r.cells.get(i).map_or("", |s| s.as_str())));
        let rv = |r: &Row| r.rv.parse::<u64>().unwrap_or(0);
        best.entry(k).and_modify(|(b, n)| {
            *n += 1;
            if rv(r) > rv(b) {
                *b = r.clone();
            }
        }).or_insert((r.clone(), 1));
    }
    let groups = best.values().filter(|(_, n)| *n > 1).map(|(r, n)| (r.uid.clone(), *n)).collect();
    (best.into_values().map(|(r, _)| r).collect(), groups)
}

impl List {
    /// One cluster-wide watch, or one per selected namespace (also the fallback for namespace-scoped RBAC).
    pub fn new(ctx: &egui::Context, client: &Client, kind: Kind, ns_sel: &BTreeSet<String>, fields: Option<&str>) -> Self {
        let data: Shared = Default::default();
        let scopes: Vec<Option<String>> = if kind.namespaced && !ns_sel.is_empty() { ns_sel.iter().cloned().map(Some).collect() } else { vec![None] };
        let watches = scopes
            .into_iter()
            .map(|ns| Bg::spawn(watch::run(client.clone(), kind.ar.clone(), ns, fields.map(String::from), data.clone(), ctx.clone())))
            .collect();
        List { kind, data, _watches: watches, view: vec![], key: None, search: String::new(), filter_re: false, filter_error: None, status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, grouped: Default::default() }
    }

    fn is_pods(&self) -> bool {
        self.kind.is("", "Pod")
    }

    /// Pods: how many rows fall in each status bucket.
    pub fn status_counts(&self) -> Option<Vec<(&'static str, usize)>> {
        if !self.is_pods() {
            return None;
        }
        let d = self.data.lock().unwrap();
        let i = d.cols.iter().position(|c| c.name == "Status")?;
        Some(POD_BUCKETS.iter().map(|b| (*b, d.rows.values().filter(|r| pod_bucket(r.cells.get(i).map_or("", |s| s)) == *b).count())).collect())
    }

    /// Distinct events (repeats grouped), cached per data revision.
    pub fn grouped_len(&self) -> usize {
        let d = self.data.lock().unwrap();
        let (rev, n) = self.grouped.get();
        if rev == d.rev && rev != 0 {
            return n;
        }
        let n = group_events(d.rows.values().cloned().collect(), &d.cols).0.len();
        self.grouped.set((d.rev, n));
        n
    }

    /// Events grouped, newest first, with how many identical events each stands for.
    pub fn grouped_rows(&self) -> (Vec<(Arc<Row>, usize)>, Vec<watch::Col>) {
        let d = self.data.lock().unwrap();
        let (mut rows, groups) = group_events(d.rows.values().cloned().collect(), &d.cols);
        rows.sort_by_key(|r| std::cmp::Reverse(r.rv.parse::<u64>().unwrap_or(0)));
        (rows.into_iter().map(|r| { let n = groups.get(&r.uid).copied().unwrap_or(1); (r, n) }).collect(), d.cols.clone())
    }

    /// Selected rows in view order.
    pub fn selected(&self) -> Vec<Arc<Row>> {
        self.view.iter().filter(|r| self.sel.contains(&r.uid)).cloned().collect()
    }

    fn metrics_kind(&self) -> bool {
        self.kind.is("", "Pod") || self.kind.is("", "Node")
    }

    /// Kinds that show their image versions (always, not only in Wide).
    fn has_images(&self, d: &ListData) -> bool {
        self.is_pods() || d.cols.iter().any(|c| c.name == "Images")
    }

    fn columns(&self, d: &ListData, wide: bool, metrics: bool) -> Vec<(DCol, String)> {
        let mut out = vec![];
        let images = self.has_images(d);
        for (i, c) in d.cols.iter().enumerate() {
            if c.name == "Images" {
                continue; // shown as DCol::Images, before Age
            }
            if c.priority > 0 && !wide {
                continue;
            }
            if c.name == "Age" {
                if images {
                    out.push((DCol::Images, "Image".into()));
                }
                out.push((DCol::Age, "Age".into()));
                continue;
            }
            out.push((DCol::Cell(i), c.name.clone()));
            if c.name == "Name" {
                if self.kind.namespaced {
                    out.push((DCol::Ns, "Namespace".into()));
                }
                if metrics {
                    out.extend([(DCol::Cpu, "CPU".into()), (DCol::Mem, "Memory".into())]);
                }
            }
        }
        if images && !out.iter().any(|c| c.0 == DCol::Images) {
            out.push((DCol::Images, "Image".into()));
        }
        if self.kind.namespaced && !out.iter().any(|c| c.0 == DCol::Ns) {
            out.insert(0, (DCol::Ns, "Namespace".into()));
        }
        out
    }

    fn usage_text(&self, m: &Metrics, r: &Row, cpu: bool) -> String {
        let Some(u) = usage(&self.kind, m, r) else { return String::new() };
        let text = if cpu { fmt_cpu(u.0) } else { fmt_bytes(u.1) };
        match (self.kind.is("", "Node"), m.alloc.get(&r.name)) {
            (true, Some(a)) => format!("{text} ({:.0}%)", 100.0 * if cpu { u.0 / a.0.max(1e-9) } else { u.1 / a.1.max(1.0) }),
            _ => text,
        }
    }

    fn cell_text(&self, r: &Row, c: DCol, name: &str, types: &[String], m: &Metrics) -> String {
        match c {
            DCol::Cell(i) => {
                let raw = r.cells.get(i).map_or("", |s| s);
                let mut t = if types.get(i).is_some_and(|t| t == "date") { watch::date_cell(raw) } else { raw.to_string() };
                if name == "Message" {
                    if let Some(n) = self.groups.get(&r.uid) {
                        t = format!("×{n}  {t}");
                    }
                }
                t
            }
            DCol::Ns => r.namespace.clone(),
            DCol::Images => self.images_text(r),
            DCol::Age => watch::age(r.created),
            DCol::Cpu | DCol::Mem => self.usage_text(m, r, c == DCol::Cpu),
        }
    }

    /// Images of a row, comma-separated: the pod's own, or the workload's "Images" cell.
    fn images_text(&self, r: &Row) -> String {
        if !r.images.is_empty() {
            return r.images.join(",");
        }
        let d = self.data.lock().unwrap();
        d.cols.iter().position(|c| c.name == "Images").and_then(|i| r.cells.get(i)).cloned().unwrap_or_default()
    }

    fn refresh(&mut self, d: &ListData, m: &Metrics) {
        let key = Some((d.rev, self.search.clone(), self.filter_re, self.sort, m.at, self.status));
        if key == self.key {
            return;
        }
        let (q, kind) = (self.search.trim(), &self.kind);
        // Plain: case-insensitive text, `|` and `*` like Lens (`Matcher::wildcard`). Regex:
        // case-insensitive; an invalid one filters nothing.
        let filt = match (q.is_empty(), self.filter_re) {
            (true, _) => Ok(None),
            (false, true) => Matcher::regex(q, false).map(Some),
            (false, false) => Ok(Some(Matcher::wildcard(q))),
        };
        self.filter_error = filt.as_ref().err().cloned();
        let filt = filt.unwrap_or(None);
        let status_col = self.status.filter(|_| self.is_pods()).and_then(|b| d.cols.iter().position(|c| c.name == "Status").map(|i| (i, b)));
        let rows: Vec<Arc<Row>> = d
            .rows
            .values()
            .filter(|r| status_col.is_none_or(|(i, b)| pod_bucket(r.cells.get(i).map_or("", |s| s)) == b))
            // The filter box (Ctrl+F) looks at names only; Ctrl+K finds text in every column.
            .filter(|r| filt.as_ref().is_none_or(|f| f.hit(&r.name)))
            .cloned()
            .collect();
        (self.view, self.groups) = if is_event(kind) { group_events(rows, &d.cols) } else { (rows, HashMap::new()) };
        match self.sort {
            // Events: newest activity first (resourceVersion grows with every update).
            None if is_event(kind) => self.view.sort_by_key(|r| std::cmp::Reverse(r.rv.parse::<u64>().unwrap_or(0))),
            None => self.view.sort_by(|a, b| a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name))),
            Some((col, asc)) => self.view.sort_by(|a, b| {
                let o = match col {
                    DCol::Cell(i) => cmp_cells(a.cells.get(i).map_or("", |s| s), b.cells.get(i).map_or("", |s| s)),
                    DCol::Ns => a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name)),
                    DCol::Age => b.created.cmp(&a.created),
                    DCol::Images => a.images.join(",").cmp(&b.images.join(",")).then_with(|| {
                        let i = d.cols.iter().position(|c| c.name == "Images");
                        let cell = |r: &Row| i.and_then(|i| r.cells.get(i)).map_or("", |s| s.as_str()).to_string();
                        cell(a).cmp(&cell(b))
                    }),
                    DCol::Cpu | DCol::Mem => {
                        let f = |r: &Row| usage(kind, m, r).map(|u| if col == DCol::Cpu { u.0 } else { u.1 }).unwrap_or(-1.0);
                        f(a).total_cmp(&f(b))
                    }
                };
                if asc { o } else { o.reverse() }
            }),
        }
        self.filter_hits = filt.as_ref().map_or(0, |f| self.view.iter().map(|r| f.ranges(&r.name).len()).sum());
        self.filter_m = filt;
        // Drop selections of rows that are gone.
        let live: HashSet<&String> = d.rows.keys().collect();
        self.sel.retain(|u| live.contains(u));
        self.key = key;
    }

    fn click(&mut self, i: usize, m: egui::Modifiers) {
        let uid = self.view[i].uid.clone();
        let anchor = self.anchor.as_ref().and_then(|a| self.view.iter().position(|r| &r.uid == a));
        match anchor {
            Some(a) if m.shift => {
                if !m.command {
                    self.sel.clear();
                }
                self.sel.extend(self.view[a.min(i)..=a.max(i)].iter().map(|r| r.uid.clone()));
            }
            _ if m.command => {
                if !self.sel.remove(&uid) {
                    self.sel.insert(uid.clone());
                }
                self.anchor = Some(uid);
            }
            _ => {
                self.sel = HashSet::from([uid.clone()]);
                self.anchor = Some(uid);
            }
        }
    }

    /// Draws the table. `find` highlights matching cells and jumps between matching rows.
    pub fn table(&mut self, ui: &mut Ui, m: &Metrics, wide: bool, find: &mut Find) -> Option<Pick> {
        let data = self.data.clone();
        let d = data.lock().unwrap();
        let with_metrics = self.metrics_kind() && m.at.is_some() && !m.nodes.is_empty();
        let cols = self.columns(&d, wide, with_metrics);
        let types: Vec<String> = d.cols.iter().map(|c| c.ty.clone()).collect();
        self.refresh(&d, m);
        drop(d);

        let matcher = find.matcher();
        let fkey = matcher.as_ref().map(|_| (self.key.as_ref().map_or(0, |k| k.0), find.query.clone(), find.regex, find.case, cols.len()));
        if fkey != self.find_key {
            self.find_rows = match &matcher {
                Some(mt) => (0..self.view.len()).filter(|&i| cols.iter().any(|(c, name)| mt.hit(&self.cell_text(&self.view[i], *c, name, &types, m)))).collect(),
                None => vec![],
            };
            find.set_total(self.find_rows.len());
            self.find_key = fkey;
        }
        let cur_row = matcher.as_ref().and_then(|_| self.find_rows.get(find.current).copied());
        let scroll_to = cur_row.filter(|_| std::mem::take(&mut find.scroll));

        // Keyboard: Ctrl+A selects everything shown, Esc clears the selection (and hides the details).
        let mut esc = false;
        if ui.rect_contains_pointer(ui.max_rect()) && !ui.ctx().egui_wants_keyboard_input() {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
                self.sel = self.view.iter().map(|r| r.uid.clone()).collect();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.sel.clear();
                esc = true;
            }
        }

        let mut picked = None;
        let (mut clicked, mut toggled, mut all) = (None, None, None);
        let row_h = ui.spacing().interact_size.y + 6.0; // 30 px comfortable, 26 compact
        let tk = ui_kit::tokens(ui);
        let pods = self.is_pods();
        let status_i = cols.iter().find(|(_, n)| n == "Status").and_then(|(c, _)| if let DCol::Cell(i) = c { Some(*i) } else { None });
        let id = ("table", self.kind.ar.group.clone(), self.kind.ar.kind.clone(), wide, with_metrics);
        // Too many columns for the width (wide mode, narrow window) scroll sideways.
        egui::ScrollArea::horizontal().id_salt(&id).auto_shrink(false).show(ui, |ui| {
            let mut tb = TableBuilder::new(ui)
                .id_salt(&id)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .auto_shrink(false)
                .column(Column::exact(22.0).resizable(false));
            if let Some(i) = scroll_to {
                tb = tb.scroll_to_row(i, Some(egui::Align::Center));
            }
            for (i, (c, name)) in cols.iter().enumerate() {
                let w: f32 = match c {
                    DCol::Cell(_) if name == "Name" => 260.0,
                    DCol::Cell(_) if name == "Message" => 420.0,
                    DCol::Ns => 130.0,
                    DCol::Cpu | DCol::Mem | DCol::Age => 70.0,
                    DCol::Images => 230.0,
                    DCol::Cell(_) => 95.0,
                };
                tb = tb.column(if i + 1 == cols.len() { Column::remainder().at_least(w.min(120.0)) } else { Column::initial(w).at_least(40.0) }.clip(true));
            }
            let (sort, n_sel, n_view) = (&mut self.sort, self.sel.len(), self.view.len());
            let table = tb.header(row_h, |mut h| {
                h.col(|ui| {
                    let mut every = n_view > 0 && n_sel == n_view;
                    if ui.checkbox(&mut every, "").on_hover_text("Select all shown (Ctrl+A)").changed() {
                        all = Some(every);
                    }
                });
                for (c, name) in &cols {
                    h.col(|ui| {
                        let arrow = match *sort {
                            Some((s, true)) if s == *c => " ⏶",
                            Some((s, false)) if s == *c => " ⏷",
                            _ => "",
                        };
                        let head = RichText::new(format!("{}{arrow}", name.to_uppercase())).font(ui_kit::semibold(11.0)).color(tk.dim).extra_letter_spacing(0.6);
                        if ui.add(egui::Button::new(head).frame(false)).clicked() {
                            *sort = match *sort {
                                Some((s, true)) if s == *c => Some((*c, false)),
                                Some((s, false)) if s == *c => None,
                                _ => Some((*c, true)),
                            };
                        }
                    });
                }
            });
            table.body(|body| {
                body.rows(row_h, self.view.len(), |mut row| {
                    let i = row.index();
                    let r = self.view[i].clone();
                    let mut on = self.sel.contains(&r.uid);
                    row.set_selected(on);
                    row.col(|ui| {
                        if ui.checkbox(&mut on, "").changed() {
                            toggled = Some((r.uid.clone(), on));
                        }
                    });
                    for (c, name) in &cols {
                        row.col(|ui| {
                            let text = self.cell_text(&r, *c, name, &types, m);
                            let is_name = matches!(c, DCol::Cell(_)) && name == "Name";
                            let status = matches!(c, DCol::Cell(_)).then(|| cell_color(name, &text)).flatten();
                            let mut rt = RichText::new(&text);
                            if let Some(col) = status {
                                rt = rt.color(ui_kit::status_fg(ui, col));
                            }
                            if is_name {
                                rt = rt.font(ui_kit::mono(12.5)).color(tk.text);
                            }
                            match c {
                                DCol::Age | DCol::Cpu | DCol::Mem => rt = rt.font(ui_kit::mono(12.0)).color(tk.muted),
                                DCol::Cell(_) if name == "Restarts" => {
                                    let n: u32 = text.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
                                    rt = rt.font(ui_kit::mono(12.0)).color(if n > 5 { ui_kit::status_fg(ui, RED) } else if n > 0 { ui_kit::status_fg(ui, ORANGE) } else { tk.muted });
                                }
                                _ => {}
                            }
                            if r.deleting {
                                rt = rt.italics().weak();
                            }
                            if is_name && pods {
                                let b = status_i.and_then(|i| r.cells.get(i)).map_or("Running", |s| pod_bucket(s));
                                ui_kit::dot(ui, bucket_color(b), 3.5);
                            }
                            // Ctrl+K matches, else the filter's in the name (never current: not stepped through).
                            let mut hits = matcher.as_ref().map(|mt| mt.ranges(&text)).unwrap_or_default();
                            let find_hits = !hits.is_empty();
                            if !find_hits && is_name {
                                hits = self.filter_m.as_ref().map(|f| f.ranges(&text)).unwrap_or_default();
                            }
                            if hits.is_empty() && *c == DCol::Images && !text.is_empty() {
                                image_cell(ui, &text, tk.muted, tk.text);
                            } else if hits.is_empty() && !r.deleting && matches!(name.as_str(), "Status" | "Phase") && status.is_some() && !text.is_empty() {
                                ui_kit::pill(ui, &text, status.unwrap_or(GREEN));
                            } else if hits.is_empty() && *c == DCol::Ns && !text.is_empty() {
                                ui_kit::chip(ui, &text, ui_kit::prop(12.0));
                            } else if hits.is_empty() {
                                ui.add(egui::Label::new(rt).truncate().selectable(false));
                            } else {
                                let mut job = (*egui::WidgetText::from(rt).into_layout_job(ui.style(), egui::FontSelection::Default, egui::Align::Center)).clone();
                                let cur = (find_hits && cur_row == Some(i)).then_some(0);
                                find::overlay(&mut job, &hits, cur);
                                ui.add(egui::Label::new(job).truncate().selectable(false));
                            }
                        });
                    }
                    let resp = row.response();
                    if resp.double_clicked() {
                        picked = Some(Pick::Open(r.clone(), true));
                    } else if resp.middle_clicked() {
                        picked = Some(Pick::Open(r.clone(), false));
                    } else if resp.clicked() {
                        clicked = Some((i, resp.ctx.input(|x| x.modifiers)));
                        picked = Some(Pick::Show(r.clone()));
                    }
                    resp.context_menu(|ui| {
                        let targets = if self.sel.contains(&r.uid) { self.view.iter().filter(|x| self.sel.contains(&x.uid)).cloned().collect() } else { vec![r.clone()] };
                        if targets.len() > 1 {
                            ui.label(RichText::new(format!("{} selected", targets.len())).weak());
                        }
                        for a in row_actions(&self.kind) {
                            if targets.len() > 1 && matches!(a, Act::Shell(_) | Act::NodeShell | Act::Drain) {
                                continue;
                            }
                            if ui.button(a.label()).clicked() {
                                picked = Some(Pick::Menu(targets.clone(), a));
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Copy name").clicked() {
                            ui.ctx().copy_text(targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join("\n"));
                            ui.close();
                        }
                    });
                });
            });
        });
        if let Some((i, mods)) = clicked {
            self.click(i, mods);
        }
        if let Some((uid, on)) = toggled {
            if on { self.sel.insert(uid.clone()) } else { self.sel.remove(&uid) };
            self.anchor = Some(uid);
        }
        match all {
            Some(true) => self.sel = self.view.iter().map(|r| r.uid.clone()).collect(),
            Some(false) => self.sel.clear(),
            None => {}
        }
        picked.or(esc.then_some(Pick::Hide))
    }
}

/// An image reference as (registry and path, name, :tag or @digest).
fn split_image(image: &str) -> (&str, &str, &str) {
    let slash = image.rfind('/').map_or(0, |i| i + 1);
    let (path, rest) = image.split_at(slash);
    let cut = rest.find('@').or_else(|| rest.rfind(':')).unwrap_or(rest.len());
    let (name, tag) = rest.split_at(cut);
    (path, name, tag)
}

/// Image cell: `name:tag` of the first image (the tag stands out), `+N` more; the full references
/// with their registries on hover.
fn image_cell(ui: &mut Ui, text: &str, muted: egui::Color32, strong: egui::Color32) {
    let images: Vec<&str> = text.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let Some(first) = images.first() else { return };
    let (_, name, tag) = split_image(first);
    let mut job = egui::text::LayoutJob::default();
    let font = ui_kit::mono(12.0);
    job.append(name, 0.0, egui::TextFormat::simple(font.clone(), muted));
    job.append(tag, 0.0, egui::TextFormat::simple(font.clone(), strong));
    if images.len() > 1 {
        job.append(&format!("  +{}", images.len() - 1), 0.0, egui::TextFormat::simple(font, muted));
    }
    ui.add(egui::Label::new(job).truncate().selectable(false)).on_hover_text(images.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(uid: &str, rv: &str, cells: &[&str]) -> Arc<Row> {
        Arc::new(Row { name: uid.into(), namespace: "default".into(), uid: uid.into(), rv: rv.into(), created: None, deleting: false, cells: cells.iter().map(|s| s.to_string()).collect(), images: vec![] })
    }

    #[test]
    fn filter_plain_and_regex() {
        let kind = Kind { ar: kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), namespaced: true, verbs: vec![] };
        let mut l = List {
            kind, data: Default::default(), _watches: vec![], view: vec![], key: None, search: String::new(), filter_re: true, filter_error: None,
            status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, grouped: Default::default(),
        };
        let mut d = ListData::default();
        for n in ["argocd-redis-ha-server-0", "argocd-redis-ha-haproxy-x", "argocd-server-1"] {
            let r = row(n, "1", &[n]);
            d.rows.insert(r.uid.clone(), r);
        }
        let m = Metrics::default();
        l.search = "^argocd-redis-ha-(server|haproxy)".into();
        l.refresh(&d, &m);
        assert_eq!(l.view.len(), 2);
        l.search = "(".into();
        l.refresh(&d, &m);
        assert!(l.filter_error.is_some());
        assert_eq!(l.view.len(), 3); // invalid regex filters nothing
        (l.filter_re, l.search) = (false, "SERVER".into());
        l.refresh(&d, &m);
        assert_eq!(l.view.len(), 2);
        assert!(l.filter_error.is_none());
        (l.filter_re, l.search) = (false, "argocd*ha*0".into()); // `*` in the middle
        l.refresh(&d, &m);
        assert_eq!(l.view.len(), 1);
    }

    #[test]
    fn filter_like_lens() {
        let kind = Kind { ar: kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), namespaced: true, verbs: vec![] };
        let mut l = List {
            kind, data: Default::default(), _watches: vec![], view: vec![], key: None, search: String::new(), filter_re: false, filter_error: None,
            status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, grouped: Default::default(),
        };
        let mut d = ListData::default();
        for n in ["jpts-pos-1-55d6", "api-ingenico-gw", "AS400-proxy", "feitian-x", "api-cnp"] {
            let r = row(n, "1", &[n]);
            d.rows.insert(r.uid.clone(), r);
        }
        // Only names count: a match in another column (an image, a node) doesn't keep the row.
        let other = row("api-tms", "1", &["api-tms", "registry/jpts-base:1.0"]);
        d.rows.insert(other.uid.clone(), other);
        let m = Metrics::default();
        for re in [false, true] {
            (l.filter_re, l.search) = (re, "jpts*|ingenico*|as400*".into());
            l.refresh(&d, &m);
            let mut names: Vec<&str> = l.view.iter().map(|r| r.name.as_str()).collect();
            names.sort();
            assert_eq!(names, ["AS400-proxy", "api-ingenico-gw", "jpts-pos-1-55d6"], "regex {re}");
            assert_eq!(l.filter_hits, 3, "one match per name, regex {re}");
        }
        (l.filter_re, l.search) = (false, " | ".into()); // nothing but separators: everything
        l.refresh(&d, &m);
        assert_eq!(l.view.len(), 6);
    }

    #[test]
    fn image_names_and_tags() {
        assert_eq!(split_image("registry.example.com:5000/team/api-gateway:2.14.1"), ("registry.example.com:5000/team/", "api-gateway", ":2.14.1"));
        assert_eq!(split_image("nginx"), ("", "nginx", ""));
        assert_eq!(split_image("ghcr.io/x/app@sha256:abcd"), ("ghcr.io/x/", "app", "@sha256:abcd"));
    }

    #[test]
    fn pod_status_buckets() {
        for (s, b) in [("Running", "Running"), ("Completed", "Completed"), ("Init:0/1", "Pending"), ("ContainerCreating", "Pending"), ("CrashLoopBackOff", "Failing"), ("ImagePullBackOff", "Failing"), ("Error", "Failing")] {
            assert_eq!(pod_bucket(s), b, "{s}");
        }
    }

    #[test]
    fn groups_identical_events() {
        let cols: Vec<watch::Col> = ["Last Seen", "Type", "Reason", "Object", "Message"].map(|n| watch::Col { name: n.into(), ..Default::default() }).to_vec();
        let rows = vec![
            row("a", "10", &["1m", "Warning", "PortOutOfRange", "service/x", "Port 45108 is not within the port range"]),
            row("b", "30", &["5s", "Warning", "PortOutOfRange", "service/x", "Port 45108 is not within the port range"]),
            row("c", "20", &["2m", "Warning", "BackOff", "pod/y", "Back-off restarting"]),
        ];
        let (view, groups) = group_events(rows, &cols);
        assert_eq!(view.len(), 2);
        assert_eq!(groups.get("b"), Some(&2)); // newest row represents the group
        assert!(!groups.contains_key("c"));
    }
}
