//! A live, filterable, sortable, multi-select table of one resource kind.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use kube::Client;

use crate::details::{row_actions, Act};
use crate::find::{self, Find, Matcher};
use crate::ops::{fmt_bytes, fmt_cpu, Kind, Metrics};
use crate::watch::{self, cell_color, cmp_cells, Bg, ListData, Row, Shared};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DCol {
    Cell(usize),
    Ns,
    Age,
    Cpu,
    Mem,
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
    key: Option<(u64, String, bool, Option<(DCol, bool)>, Option<std::time::Instant>)>,
    pub search: String,
    /// The filter box is a regular expression.
    pub filter_re: bool,
    pub filter_error: Option<String>,
    pub sort: Option<(DCol, bool)>,
    /// Selected rows by uid (survives re-sorts and watch updates).
    pub sel: HashSet<String>,
    anchor: Option<String>,
    /// Events: how many identical events each shown row stands for.
    groups: HashMap<String, usize>,
    find_rows: Vec<usize>,
    find_key: Option<(u64, String, bool, bool, usize)>,
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
        List { kind, data, _watches: watches, view: vec![], key: None, search: String::new(), filter_re: false, filter_error: None, sort: None, sel: HashSet::new(), anchor: None, groups: HashMap::new(), find_rows: vec![], find_key: None }
    }

    /// Selected rows in view order.
    pub fn selected(&self) -> Vec<Arc<Row>> {
        self.view.iter().filter(|r| self.sel.contains(&r.uid)).cloned().collect()
    }

    fn metrics_kind(&self) -> bool {
        self.kind.is("", "Pod") || self.kind.is("", "Node")
    }

    fn columns(&self, d: &ListData, wide: bool, metrics: bool) -> Vec<(DCol, String)> {
        let mut out = vec![];
        for (i, c) in d.cols.iter().enumerate() {
            if c.priority > 0 && !wide {
                continue;
            }
            if c.name == "Age" {
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
            DCol::Age => watch::age(r.created),
            DCol::Cpu | DCol::Mem => self.usage_text(m, r, c == DCol::Cpu),
        }
    }

    fn refresh(&mut self, d: &ListData, m: &Metrics) {
        let key = Some((d.rev, self.search.clone(), self.filter_re, self.sort, m.at));
        if key == self.key {
            return;
        }
        let (q, kind) = (self.search.trim(), &self.kind);
        // Plain: ASCII case-insensitive substring. Regex: case-insensitive; an invalid one filters nothing.
        let filt = match (q.is_empty(), self.filter_re) {
            (true, _) => Ok(None),
            (false, true) => Matcher::regex(q, false).map(Some),
            (false, false) => Ok(Some(Matcher::plain(q, false))),
        };
        self.filter_error = filt.as_ref().err().cloned();
        let filt = filt.unwrap_or(None);
        let rows: Vec<Arc<Row>> = d.rows.values().filter(|r| filt.as_ref().is_none_or(|f| f.hit(&r.name) || f.hit(&r.namespace) || r.cells.iter().any(|c| f.hit(c)))).cloned().collect();
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
                    DCol::Cpu | DCol::Mem => {
                        let f = |r: &Row| usage(kind, m, r).map(|u| if col == DCol::Cpu { u.0 } else { u.1 }).unwrap_or(-1.0);
                        f(a).total_cmp(&f(b))
                    }
                };
                if asc { o } else { o.reverse() }
            }),
        }
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
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
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
                        if ui.add(egui::Button::new(RichText::new(format!("{name}{arrow}")).strong()).frame(false)).clicked() {
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
                            let mut rt = RichText::new(&text);
                            if let Some(col) = matches!(c, DCol::Cell(_)).then(|| cell_color(name, &text)).flatten() {
                                rt = rt.color(col);
                            }
                            if r.deleting {
                                rt = rt.italics().weak();
                            }
                            let hits = matcher.as_ref().map(|mt| mt.ranges(&text)).unwrap_or_default();
                            if hits.is_empty() {
                                ui.add(egui::Label::new(rt).truncate().selectable(false));
                            } else {
                                let mut job = (*egui::WidgetText::from(rt).into_layout_job(ui.style(), egui::FontSelection::Default, egui::Align::Center)).clone();
                                let cur = (cur_row == Some(i)).then_some(0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(uid: &str, rv: &str, cells: &[&str]) -> Arc<Row> {
        Arc::new(Row { name: uid.into(), namespace: "default".into(), uid: uid.into(), rv: rv.into(), created: None, deleting: false, cells: cells.iter().map(|s| s.to_string()).collect() })
    }

    #[test]
    fn filter_plain_and_regex() {
        let kind = Kind { ar: kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), namespaced: true, verbs: vec![] };
        let mut l = List {
            kind, data: Default::default(), _watches: vec![], view: vec![], key: None, search: String::new(), filter_re: true, filter_error: None,
            sort: None, sel: HashSet::new(), anchor: None, groups: HashMap::new(), find_rows: vec![], find_key: None,
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
