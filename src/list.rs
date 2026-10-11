//! A live, filterable, sortable, multi-select table of one resource kind.
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use kube::Client;

use crate::details::{row_actions, Act};
use crate::find::{self, Find, Matcher};
use crate::ops::{fmt_bytes, fmt_cpu, Kind, Metrics};
use crate::ui_kit;
use crate::watch::{self, cell_color, cmp_cells, Bg, ListData, Lock, Row, Shared, GREEN, ORANGE, RED};

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

/// A file format the rows of a list can be exported in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Export {
    Csv,
    Json,
}

impl Export {
    pub fn extension(self) -> &'static str {
        match self {
            Export::Csv => "csv",
            Export::Json => "json",
        }
    }
}

/// A CSV field (RFC 4180): quoted when it holds a comma, a quote or a line break.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// A row as a JSON object whose keys keep the order of the columns.
struct JsonRow<'a>(&'a [String], &'a [String]);

impl serde::Serialize for JsonRow<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.0.iter().zip(self.1))
    }
}

/// How an arrow key moves the cursor of a list.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Rows(isize),
    Home,
    End,
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

/// The rows change at most this often on screen while the data keeps changing: rebuilding the view of
/// 50,000 rows (filter, sort) takes tens of milliseconds, and a watch event arrives every few.
const REFRESH_EVERY: Duration = Duration::from_millis(250);

/// Events with their repeats grouped, newest first, and the columns they have.
pub type Grouped = (Vec<(Arc<Row>, usize)>, Vec<watch::Col>);

/// What is derived from the rows, kept until they (`rev`) or the selection change.
#[derive(Default)]
struct Derived {
    /// Times `refresh` rebuilt the view; the view is the same while it is the same.
    refreshes: u64,
    refreshed: Option<Instant>,
    counts: Option<(u64, Vec<(&'static str, usize)>)>,
    grouped: Option<(u64, Arc<Grouped>)>,
    breakdown: Option<(u64, Vec<(String, usize)>)>,
    selected: Option<(u64, HashSet<String>, Vec<Arc<Row>>)>,
}

type ViewKey = (u64, String, bool, Option<(DCol, bool)>, Option<Instant>, Option<&'static str>);

/// What `refresh` needs of the data, taken under its lock so that filtering and sorting happen outside it.
struct Snapshot {
    rev: u64,
    cols: Vec<watch::Col>,
    rows: Vec<Arc<Row>>,
    /// Selected rows that are not in the data any more.
    gone: Vec<String>,
}

pub struct List {
    pub kind: Kind,
    pub data: Shared,
    _watches: Vec<Bg>,
    pub view: Vec<Arc<Row>>,
    key: Option<ViewKey>,
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
    /// The row the arrow keys move from (the moving end of a Shift selection), by uid.
    cursor: Option<String>,
    /// Events: how many identical events each shown row stands for.
    groups: HashMap<String, usize>,
    find_rows: Vec<usize>,
    find_key: Option<(u64, String, bool, bool, usize)>,
    derived: RefCell<Derived>,
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
        List { kind, data, _watches: watches, view: vec![], key: None, search: String::new(), filter_re: false, filter_error: None, status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, cursor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, derived: Default::default() }
    }

    fn is_pods(&self) -> bool {
        self.kind.is("", "Pod")
    }

    /// Pods: how many rows fall in each status bucket.
    pub fn status_counts(&self) -> Option<Vec<(&'static str, usize)>> {
        if !self.is_pods() {
            return None;
        }
        let d = self.data.locked();
        d.shown.mark();
        if let Some((rev, counts)) = &self.derived.borrow().counts
            && *rev == d.rev
        {
            return Some(counts.clone());
        }
        let i = d.cols.iter().position(|c| c.name == "Status")?;
        let counts: Vec<_> = POD_BUCKETS.iter().map(|b| (*b, d.rows.values().filter(|r| pod_bucket(r.cells.get(i).map_or("", |s| s)) == *b).count())).collect();
        self.derived.borrow_mut().counts = Some((d.rev, counts.clone()));
        Some(counts)
    }

    /// Distinct events (repeats grouped).
    pub fn grouped_len(&self) -> usize {
        self.grouped_rows().0.len()
    }

    /// Events grouped, newest first, with how many identical events each stands for; built again only
    /// when the data changed.
    pub fn grouped_rows(&self) -> Arc<Grouped> {
        let d = self.data.locked();
        d.shown.mark();
        if let Some((rev, g)) = &self.derived.borrow().grouped
            && *rev == d.rev
            && d.rev != 0
        {
            return g.clone();
        }
        let (mut rows, groups) = group_events(d.rows.values().cloned().collect(), &d.cols);
        rows.sort_by_cached_key(|r| std::cmp::Reverse(r.rv.parse::<u64>().unwrap_or(0)));
        let g: Arc<Grouped> = Arc::new((rows.into_iter().map(|r| { let n = groups.get(&r.uid).copied().unwrap_or(1); (r, n) }).collect(), d.cols.clone()));
        self.derived.borrow_mut().grouped = Some((d.rev, g.clone()));
        g
    }

    /// `count(d)` (a status breakdown of the rows), built again only when the data changed.
    pub fn breakdown(&self, d: &ListData, count: impl FnOnce(&ListData) -> Vec<(String, usize)>) -> Vec<(String, usize)> {
        d.shown.mark();
        if let Some((rev, v)) = &self.derived.borrow().breakdown
            && *rev == d.rev
        {
            return v.clone();
        }
        let v = count(d);
        self.derived.borrow_mut().breakdown = Some((d.rev, v.clone()));
        v
    }

    /// Selected rows in view order; the same while the view and the selection are.
    pub fn selected(&self) -> Vec<Arc<Row>> {
        if self.sel.is_empty() {
            return vec![];
        }
        let mut dv = self.derived.borrow_mut();
        let n = dv.refreshes;
        if let Some((at, sel, rows)) = &dv.selected
            && *at == n
            && *sel == self.sel
        {
            return rows.clone();
        }
        let rows: Vec<Arc<Row>> = self.view.iter().filter(|r| self.sel.contains(&r.uid)).cloned().collect();
        dv.selected = Some((n, self.sel.clone(), rows.clone()));
        rows
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
                if name == "Message"
                    && let Some(n) = self.groups.get(&r.uid) {
                        t = format!("×{n}  {t}");
                    }
                t
            }
            DCol::Ns => r.namespace.clone(),
            DCol::Images => self.images_text(r),
            DCol::Age => watch::age(r.created),
            DCol::Cpu | DCol::Mem => self.usage_text(m, r, c == DCol::Cpu),
        }
    }

    /// Whether `mt` matches a cell; a plain cell is looked at where it is, its text is not built.
    fn cell_hit(&self, mt: &Matcher, r: &Row, c: DCol, name: &str, types: &[String], m: &Metrics) -> bool {
        match c {
            DCol::Cell(i) if types.get(i).is_none_or(|t| t != "date") && !(name == "Message" && self.groups.contains_key(&r.uid)) => mt.hit(r.cells.get(i).map_or("", |s| s)),
            DCol::Ns => mt.hit(&r.namespace),
            _ => mt.hit(&self.cell_text(r, c, name, types, m)),
        }
    }

    /// Images of a row, comma-separated: the pod's own, or the workload's "Images" cell.
    fn images_text(&self, r: &Row) -> String {
        if !r.images.is_empty() {
            return r.images.join(",");
        }
        let d = self.data.locked();
        d.cols.iter().position(|c| c.name == "Images").and_then(|i| r.cells.get(i)).cloned().unwrap_or_default()
    }

    /// The data as `refresh` needs it.
    fn snapshot(&self, d: &ListData) -> Snapshot {
        Snapshot { rev: d.rev, cols: d.cols.clone(), rows: d.rows.values().cloned().collect(), gone: self.sel.iter().filter(|u| !d.rows.contains_key(*u)).cloned().collect() }
    }

    /// What the view was built for: the data, the filter box, the sort and the status filter (and
    /// when the metrics were read, if that is what the rows are sorted by).
    fn view_key(&self, rev: u64, m: &Metrics) -> ViewKey {
        let by_usage = matches!(self.sort, Some((DCol::Cpu | DCol::Mem, _)));
        (rev, self.search.clone(), self.filter_re, self.sort, m.at.filter(|_| by_usage), self.status)
    }

    /// Whether the view is to be built now. A change the user made (filter, sort) is shown at once; a
    /// change of the data waits until the last build is `REFRESH_EVERY` old, and a repaint is asked
    /// for then.
    fn refresh_due(&self, ctx: &egui::Context, rev: u64, m: &Metrics) -> bool {
        let key = self.view_key(rev, m);
        let Some(old) = &self.key else { return true };
        if *old == key {
            return false;
        }
        let only_data = (&old.1, old.2, old.3, old.5) == (&key.1, key.2, key.3, key.5);
        match self.derived.borrow().refreshed.map(|t| t.elapsed()) {
            Some(age) if only_data && age < REFRESH_EVERY => {
                ctx.request_repaint_after(REFRESH_EVERY - age);
                false
            }
            _ => true,
        }
    }

    #[cfg(test)]
    fn refresh_now(&mut self, d: &ListData, m: &Metrics) {
        let s = self.snapshot(d);
        self.refresh(&s, m);
    }

    fn refresh(&mut self, d: &Snapshot, m: &Metrics) {
        crate::prof!("List::refresh");
        let key = Some(self.view_key(d.rev, m));
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
        let status_col = d.cols.iter().position(|c| c.name == "Status").zip(self.status.filter(|_| self.is_pods()));
        let rows: Vec<Arc<Row>> = d
            .rows
            .iter()
            .filter(|r| status_col.is_none_or(|(i, b)| pod_bucket(r.cells.get(i).map_or("", |s| s)) == b))
            // The filter box (Ctrl+F) looks at names only; Ctrl+K finds text in every column.
            .filter(|r| filt.as_ref().is_none_or(|f| f.hit(&r.name)))
            .cloned()
            .collect();
        (self.view, self.groups) = if is_event(kind) { group_events(rows, &d.cols) } else { (rows, HashMap::new()) };
        match self.sort {
            // Events: newest activity first (resourceVersion grows with every update).
            None if is_event(kind) => self.view.sort_by_cached_key(|r| std::cmp::Reverse(r.rv.parse::<u64>().unwrap_or(0))),
            // Namespace and name are unique, so equal rows (whose order an unstable sort leaves open) don't exist.
            None => self.view.sort_unstable_by(|a, b| a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name))),
            Some((DCol::Images, asc)) => {
                let i = d.cols.iter().position(|c| c.name == "Images");
                sort_keyed(&mut self.view, asc, |r| (r.images.join(","), i.and_then(|i| r.cells.get(i)).cloned().unwrap_or_default()));
            }
            Some((col @ (DCol::Cpu | DCol::Mem), asc)) => sort_keyed(&mut self.view, asc, |r| f64_key(usage(kind, m, r).map(|u| if col == DCol::Cpu { u.0 } else { u.1 }).unwrap_or(-1.0))),
            Some((col, asc)) => self.view.sort_by(|a, b| {
                let o = match col {
                    DCol::Cell(i) => cmp_cells(a.cells.get(i).map_or("", |s| s), b.cells.get(i).map_or("", |s| s)),
                    DCol::Ns => a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name)),
                    _ => b.created.cmp(&a.created),
                };
                if asc { o } else { o.reverse() }
            }),
        }
        self.filter_hits = filt.as_ref().map_or(0, |f| self.view.iter().map(|r| f.ranges(&r.name).len()).sum());
        self.filter_m = filt;
        // Drop selections of rows that are gone.
        for u in &d.gone {
            self.sel.remove(u);
        }
        self.key = key;
        let dv = self.derived.get_mut();
        (dv.refreshes, dv.refreshed) = (dv.refreshes + 1, Some(Instant::now()));
    }

    fn click(&mut self, i: usize, m: egui::Modifiers) {
        let uid = self.view[i].uid.clone();
        self.cursor = Some(uid.clone());
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

    /// The rows shown (filtered and sorted as they are) with the columns of the table, as text in `format`.
    pub fn export(&self, wide: bool, m: &Metrics, format: Export) -> String {
        let (heads, rows) = {
            let d = self.data.locked();
            let cols = self.columns(&d, wide, self.metrics_kind() && m.at.is_some() && !m.nodes.is_empty());
            (cols, d.cols.iter().map(|c| c.ty.clone()).collect::<Vec<_>>())
        };
        let (cols, types) = (heads, rows);
        let names: Vec<String> = cols.iter().map(|(_, n)| n.clone()).collect();
        let rows: Vec<Vec<String>> = self.view.iter().map(|r| cols.iter().map(|(c, n)| self.cell_text(r, *c, n, &types, m)).collect()).collect();
        match format {
            Export::Csv => {
                let line = |cells: &[String]| cells.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(",");
                std::iter::once(line(&names)).chain(rows.iter().map(|r| line(r))).collect::<Vec<_>>().join("\r\n") + "\r\n"
            }
            Export::Json => serde_json::to_string_pretty(&rows.iter().map(|r| JsonRow(&names, r)).collect::<Vec<_>>()).unwrap_or_default(),
        }
    }

    /// A key moved the cursor: the row it is on now, with that row selected alone, or the rows from the
    /// anchor to it when `extend` (Shift). With no cursor yet a move down starts at the first row, one up
    /// at the last.
    fn step(&mut self, to: Step, extend: bool) -> Option<usize> {
        let n = self.view.len().checked_sub(1)?;
        let from = self.cursor.as_ref().or(self.anchor.as_ref()).and_then(|u| self.view.iter().position(|r| &r.uid == u));
        let i = match (to, from) {
            (Step::Home, _) => 0,
            (Step::End, _) => n,
            (Step::Rows(d), Some(f)) => f.saturating_add_signed(d).min(n),
            (Step::Rows(d), None) if d >= 0 => 0,
            (Step::Rows(_), None) => n,
        };
        self.click(i, if extend { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE });
        Some(i)
    }

    /// Draws the table. `find` highlights matching cells and jumps between matching rows.
    pub fn table(&mut self, ui: &mut Ui, m: &Metrics, wide: bool, find: &mut Find) -> Option<Pick> {
        let data = self.data.clone();
        let d = data.locked();
        d.shown.mark();
        let with_metrics = self.metrics_kind() && m.at.is_some() && !m.nodes.is_empty();
        let cols = self.columns(&d, wide, with_metrics);
        let types: Vec<String> = d.cols.iter().map(|c| c.ty.clone()).collect();
        let snapshot = self.refresh_due(ui.ctx(), d.rev, m).then(|| self.snapshot(&d));
        drop(d);
        if let Some(s) = snapshot {
            self.refresh(&s, m);
        }

        let matcher = find.matcher();
        let fkey = matcher.as_ref().map(|_| (self.key.as_ref().map_or(0, |k| k.0), find.query.clone(), find.regex, find.case, cols.len()));
        if fkey != self.find_key {
            self.find_rows = match &matcher {
                Some(mt) => (0..self.view.len()).filter(|&i| cols.iter().any(|(c, name)| self.cell_hit(mt, &self.view[i], *c, name, &types, m))).collect(),
                None => vec![],
            };
            find.set_total(self.find_rows.len());
            self.find_key = fkey;
        }
        let cur_row = matcher.as_ref().and_then(|_| self.find_rows.get(find.current).copied());
        let mut scroll_to = cur_row.filter(|_| std::mem::take(&mut find.scroll)).map(|i| (i, Some(egui::Align::Center)));

        let mut picked = None;
        let row_h = ui.spacing().interact_size.y + 6.0; // 30 px comfortable, 26 compact
        // Keyboard (with the pointer over the list, not while typing somewhere): Ctrl+A selects everything
        // shown, Esc clears the selection (and hides the details), the arrows, Page Up/Down and Home/End
        // move the selection (Shift extends it), Enter opens the row in a tab, Delete deletes the selection
        // (after the usual confirmation).
        let mut esc = false;
        if ui.rect_contains_pointer(ui.max_rect()) && !ui.ctx().egui_wants_keyboard_input() {
            use egui::{Key, Modifiers as M};
            if ui.input_mut(|i| i.consume_key(M::COMMAND, Key::A)) {
                self.sel = self.view.iter().map(|r| r.uid.clone()).collect();
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                self.sel.clear();
                esc = true;
            }
            let page = ((ui.available_height() / row_h) as isize - 2).max(1);
            let moves = [(Key::ArrowDown, Step::Rows(1)), (Key::ArrowUp, Step::Rows(-1)), (Key::PageDown, Step::Rows(page)), (Key::PageUp, Step::Rows(-page)), (Key::Home, Step::Home), (Key::End, Step::End)];
            // Shift first: a plain pattern matches Shift presses too.
            let presses: Vec<(Step, bool)> = ui.input_mut(|i| moves.iter().flat_map(|&(k, s)| [(s, true, i.count_and_consume_key(M::SHIFT, k)), (s, false, i.count_and_consume_key(M::NONE, k))]).flat_map(|(s, shift, n)| std::iter::repeat_n((s, shift), n)).collect());
            if let Some(i) = presses.into_iter().filter_map(|(s, shift)| self.step(s, shift)).last() {
                scroll_to = Some((i, None));
                picked = Some(Pick::Show(self.view[i].clone()));
            }
            if ui.input_mut(|i| i.consume_key(M::NONE, Key::Enter))
                && let Some(r) = self.cursor.as_ref().and_then(|u| self.view.iter().find(|r| &r.uid == u))
            {
                picked = Some(Pick::Open(r.clone(), true));
            }
            if self.kind.can("delete") && ui.input_mut(|i| i.consume_key(M::NONE, Key::Delete)) {
                let rows = self.selected();
                if !rows.is_empty() {
                    picked = Some(Pick::Menu(rows, Act::Delete));
                }
            }
        }

        let (mut clicked, mut toggled, mut all) = (None, None, None);
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
            if let Some((i, align)) = scroll_to {
                tb = tb.scroll_to_row(i, align);
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
                        if let Some(k) = crate::details::kubectl_menu(ui, &self.kind) {
                            picked = Some(Pick::Menu(targets.clone(), Act::Kubectl(k)));
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

/// Sorts by a key built once per row (`sort_by` would build it for every comparison).
fn sort_keyed<K: Ord>(rows: &mut [Arc<Row>], asc: bool, key: impl Fn(&Row) -> K) {
    if asc {
        rows.sort_by_cached_key(|r| key(r));
    } else {
        rows.sort_by_cached_key(|r| std::cmp::Reverse(key(r)));
    }
}

/// A number as an integer that sorts the way `f64::total_cmp` does.
fn f64_key(x: f64) -> u64 {
    let b = x.to_bits();
    if b >> 63 == 1 { !b } else { b | 1 << 63 }
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

    /// 50k pods like the Table API gives them (7 columns, images, 40 namespaces): the cost of a
    /// `refresh` for each sort and filter, the thing a watch event triggers on the next frame.
    #[test]
    #[ignore]
    fn bench_list_refresh_50k() {
        let kind = Kind { ar: kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), namespaced: true, verbs: vec![] };
        let mut l = List {
            kind, data: Default::default(), _watches: vec![], view: vec![], key: None, search: String::new(), filter_re: false, filter_error: None,
            status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, cursor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, derived: Default::default(),
        };
        let mut d = ListData { cols: ["Name", "Ready", "Status", "Restarts", "Age", "IP", "Node"].map(|n| crate::watch::Col { name: n.into(), ..Default::default() }).to_vec(), ..Default::default() };
        for i in 0..50_000u64 {
            let name = format!("service-{:05}-{:x}-abcde", i % 2500, i * 2654435761 % 0xfffff);
            let cells = [name.clone(), "1/1".into(), if i % 40 == 0 { "CrashLoopBackOff".into() } else { "Running".into() }, (i % 7).to_string(), format!("{}d", i % 300), format!("10.42.{}.{}", i % 250, i % 200), format!("node-{}", i % 12)];
            let r = Row { name, namespace: format!("ns{}", i % 40), uid: format!("{i:08x}-0000-4000-8000-000000000000"), rv: i.to_string(), created: None, deleting: false, cells: cells.to_vec(), images: vec![format!("registry.example.com/team/service-{}:{}.{}.{}", i % 2500, i % 3, i % 10, i % 20)] };
            d.rows.insert(r.uid.clone(), Arc::new(r));
        }
        let m = Metrics::default();
        let mut run = |label: &str, f: &mut dyn FnMut(&mut List)| {
            f(&mut l);
            crate::prof::bench(label, 5, || {
                d.rev += 1;
                l.refresh_now(&d, &m);
            });
        };
        run("list refresh 50k, default sort", &mut |_| {});
        run("list refresh 50k, wildcard filter", &mut |l| l.search = "service-0*|node-3".into());
        run("list refresh 50k, regex filter", &mut |l| (l.filter_re, l.search) = (true, "^service-0[0-4].*-abcde$".into()));
        run("list refresh 50k, sort by Restarts", &mut |l| (l.filter_re, l.search, l.sort) = (false, String::new(), Some((DCol::Cell(3), true))));
        run("list refresh 50k, sort by Age", &mut |l| l.sort = Some((DCol::Age, true)));
        run("list refresh 50k, sort by Images", &mut |l| l.sort = Some((DCol::Images, true)));
        run("list refresh 50k, sort by Namespace", &mut |l| l.sort = Some((DCol::Ns, false)));
    }

    fn list_of(names: &[&str], cells: impl Fn(usize) -> Vec<String>) -> (List, ListData) {
        let kind = Kind { ar: kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), namespaced: true, verbs: vec![] };
        let l = List {
            kind, data: Default::default(), _watches: vec![], view: vec![], key: None, search: String::new(), filter_re: false, filter_error: None,
            status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, cursor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, derived: Default::default(),
        };
        let mut d = ListData { cols: ["Name", "Restarts"].map(|n| crate::watch::Col { name: n.into(), ..Default::default() }).to_vec(), ..Default::default() };
        for (i, n) in names.iter().enumerate() {
            let r = Arc::new(Row { name: n.to_string(), namespace: "default".into(), uid: n.to_string(), rv: "1".into(), created: None, deleting: false, cells: cells(i), images: vec![] });
            d.rows.insert(r.uid.clone(), r);
        }
        (l, d)
    }

    #[test]
    fn clicks_select_like_a_file_manager() {
        use egui::Modifiers as M;
        let (mut l, d) = list_of(&["a", "b", "c", "d", "e"], |_| vec![]);
        l.refresh_now(&d, &Metrics::default());
        let sel = |l: &List| {
            let mut v: Vec<_> = l.sel.iter().cloned().collect();
            v.sort();
            v.join("")
        };
        l.click(1, M::NONE);
        assert_eq!(sel(&l), "b");
        l.click(3, M::SHIFT); // range from the anchor
        assert_eq!(sel(&l), "bcd");
        l.click(0, M::COMMAND); // adds one
        assert_eq!(sel(&l), "abcd");
        l.click(2, M::COMMAND); // takes one out
        assert_eq!(sel(&l), "abd");
        l.click(4, M::NONE); // a plain click starts over
        assert_eq!(sel(&l), "e");
        l.click(2, M::SHIFT); // the range is from the last click
        assert_eq!(sel(&l), "cde");
        l.click(0, M::SHIFT | M::COMMAND); // Ctrl+Shift extends what is selected
        assert_eq!(sel(&l), "abcde");
    }

    /// The arrows move the selection like in a file manager, Shift extends it, Enter opens the row,
    /// Delete asks to delete the selection; nothing happens while the pointer is elsewhere.
    #[test]
    fn the_keyboard_moves_through_a_list() {
        use egui::{Key, Modifiers as M};
        let (mut l, d) = list_of(&["a", "b", "c", "d", "e"], |_| vec![]);
        l.kind.verbs = vec!["delete".into()];
        *l.data.locked() = ListData { rev: 1, ..d };
        let mut h = crate::testkit::Harness::themed(egui::vec2(800.0, 500.0));
        let (m, mut find) = (Metrics::default(), Find::default());
        let mut press = |l: &mut List, keys: Vec<egui::Event>, over: bool| {
            let mut picked = None;
            let mut events = if over { vec![crate::testkit::moved(egui::pos2(300.0, 250.0))] } else { vec![crate::testkit::moved(egui::pos2(5000.0, 5000.0))] };
            events.extend(keys);
            h.frame(events, |ui| picked = l.table(ui, &m, false, &mut find));
            picked
        };
        let sel = |l: &List| {
            let mut v: Vec<_> = l.sel.iter().cloned().collect();
            v.sort();
            v.join("")
        };
        let k = crate::testkit::key;
        press(&mut l, vec![], true);
        assert!(press(&mut l, vec![k(Key::ArrowDown, M::NONE)], false).is_none() && sel(&l).is_empty(), "the pointer is elsewhere");
        let p = press(&mut l, vec![k(Key::ArrowDown, M::NONE)], true);
        assert_eq!(sel(&l), "a", "down from nothing: the first row");
        assert!(matches!(p, Some(Pick::Show(r)) if r.name == "a"), "the details follow");
        press(&mut l, vec![k(Key::ArrowDown, M::NONE)], true);
        assert_eq!(sel(&l), "b");
        press(&mut l, vec![k(Key::ArrowDown, M::SHIFT), k(Key::ArrowDown, M::SHIFT)], true);
        assert_eq!(sel(&l), "bcd", "Shift extends from where the move started");
        press(&mut l, vec![k(Key::ArrowUp, M::SHIFT)], true);
        assert_eq!(sel(&l), "bc", "and shrinks it again");
        press(&mut l, vec![k(Key::ArrowUp, M::NONE)], true);
        assert_eq!(sel(&l), "b", "a plain move selects one row, from the moving end");
        press(&mut l, vec![k(Key::End, M::NONE)], true);
        assert_eq!(sel(&l), "e");
        press(&mut l, vec![k(Key::ArrowDown, M::NONE)], true);
        assert_eq!(sel(&l), "e", "the last row stays");
        press(&mut l, vec![k(Key::Home, M::NONE)], true);
        assert_eq!(sel(&l), "a");
        let p = press(&mut l, vec![k(Key::Enter, M::NONE)], true);
        assert!(matches!(p, Some(Pick::Open(r, true)) if r.name == "a"));
        press(&mut l, vec![k(Key::ArrowDown, M::SHIFT)], true);
        let p = press(&mut l, vec![k(Key::Delete, M::NONE)], true);
        assert!(matches!(p, Some(Pick::Menu(rows, Act::Delete)) if rows.len() == 2));
        l.kind.verbs.clear();
        assert!(press(&mut l, vec![k(Key::Delete, M::NONE)], true).is_none(), "a kind that can't be deleted");
    }

    /// The export is the table as it is shown: the filtered and sorted rows, the columns in order.
    #[test]
    fn a_list_exports_as_csv_and_json() {
        let names = ["pod, one", "say \"hi\"", "plain"];
        let (mut l, mut d) = list_of(&names, |i| vec![names[i].to_string(), ["3", "0", "12"][i].into()]);
        d.cols[0].name = "Name".into();
        d.rev = 1;
        *l.data.locked() = d;
        let m = Metrics::default();
        let s = { let d = l.data.locked(); l.snapshot(&d) };
        l.sort = Some((DCol::Cell(1), true)); // by Restarts
        l.refresh(&s, &m);
        let csv = l.export(false, &m, Export::Csv);
        assert_eq!(csv, "Name,Namespace,Restarts,Image\r\n\"say \"\"hi\"\"\",default,0,\r\n\"pod, one\",default,3,\r\nplain,default,12,\r\n");
        l.search = "plain|one".into();
        l.refresh(&{ let d = l.data.locked(); l.snapshot(&d) }, &m);
        let json: serde_json::Value = serde_json::from_str(&l.export(false, &m, Export::Json)).unwrap();
        assert_eq!(json.as_array().map(Vec::len), Some(2), "only the rows shown");
        assert_eq!(json[0]["Name"], "pod, one");
        assert_eq!(json[1]["Restarts"], "12");
        // The keys come in the order of the columns (a JSON map would sort them): Name, Namespace, Restarts, Image.
        let text = l.export(false, &m, Export::Json);
        let at = |k: &str| text.find(k).unwrap();
        assert!(at("Namespace") < at("Restarts") && at("Restarts") < at("Image"), "{text}");
        assert_eq!((Export::Csv.extension(), Export::Json.extension()), ("csv", "json"));
    }

    #[test]
    fn the_header_sorts_by_number_age_and_name() {
        let (mut l, d) = list_of(&["x-10", "x-9", "x-100"], |i| vec![String::new(), ["7", "12", "3 (5m ago)"][i].into()]);
        let m = Metrics::default();
        let order = |l: &List| l.view.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(" ");
        l.refresh_now(&d, &m);
        assert_eq!(order(&l), "x-10 x-100 x-9", "by namespace then name: text order");
        l.sort = Some((DCol::Cell(1), true));
        l.refresh_now(&d, &m);
        assert_eq!(order(&l), "x-100 x-10 x-9", "restarts 3, 7, 12: numbers, not text");
        l.sort = Some((DCol::Cell(1), false));
        l.refresh_now(&d, &m);
        assert_eq!(order(&l), "x-9 x-10 x-100", "descending");
    }

    #[test]
    fn filter_plain_and_regex() {
        let kind = Kind { ar: kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), namespaced: true, verbs: vec![] };
        let mut l = List {
            kind, data: Default::default(), _watches: vec![], view: vec![], key: None, search: String::new(), filter_re: true, filter_error: None,
            status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, cursor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, derived: Default::default(),
        };
        let mut d = ListData::default();
        for n in ["argocd-redis-ha-server-0", "argocd-redis-ha-haproxy-x", "argocd-server-1"] {
            let r = row(n, "1", &[n]);
            d.rows.insert(r.uid.clone(), r);
        }
        let m = Metrics::default();
        l.search = "^argocd-redis-ha-(server|haproxy)".into();
        l.refresh_now(&d, &m);
        assert_eq!(l.view.len(), 2);
        l.search = "(".into();
        l.refresh_now(&d, &m);
        assert!(l.filter_error.is_some());
        assert_eq!(l.view.len(), 3); // invalid regex filters nothing
        (l.filter_re, l.search) = (false, "SERVER".into());
        l.refresh_now(&d, &m);
        assert_eq!(l.view.len(), 2);
        assert!(l.filter_error.is_none());
        (l.filter_re, l.search) = (false, "argocd*ha*0".into()); // `*` in the middle
        l.refresh_now(&d, &m);
        assert_eq!(l.view.len(), 1);
    }

    #[test]
    fn filter_like_lens() {
        let kind = Kind { ar: kube::api::ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&()), namespaced: true, verbs: vec![] };
        let mut l = List {
            kind, data: Default::default(), _watches: vec![], view: vec![], key: None, search: String::new(), filter_re: false, filter_error: None,
            status: None, filter_m: None, filter_hits: 0, sort: None, sel: HashSet::new(), anchor: None, cursor: None, groups: HashMap::new(), find_rows: vec![], find_key: None, derived: Default::default(),
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
            l.refresh_now(&d, &m);
            let mut names: Vec<&str> = l.view.iter().map(|r| r.name.as_str()).collect();
            names.sort();
            assert_eq!(names, ["AS400-proxy", "api-ingenico-gw", "jpts-pos-1-55d6"], "regex {re}");
            assert_eq!(l.filter_hits, 3, "one match per name, regex {re}");
        }
        (l.filter_re, l.search) = (false, " | ".into()); // nothing but separators: everything
        l.refresh_now(&d, &m);
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

    fn names(rows: &[Arc<Row>]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    /// Watch events change the data a few times a second; the table is rebuilt (filtered, sorted) at
    /// most four times a second for them, at once for what the user does.
    #[test]
    fn a_changing_list_is_rebuilt_a_few_times_a_second() {
        let (mut l, d) = list_of(&["a", "b"], |_| vec![]);
        *l.data.locked() = ListData { rev: 1, ..d };
        let mut h = crate::testkit::Harness::themed(egui::vec2(800.0, 400.0));
        let (m, mut find) = (Metrics::default(), Find::default());
        let mut frame = |l: &mut List| h.frame_delay(vec![], |ui| drop(l.table(ui, &m, false, &mut find)));
        let add = |l: &List, name: &str| {
            let mut d = l.data.locked();
            let r = row(name, "1", &[name]);
            d.rows.insert(r.uid.clone(), r);
            d.rev += 1;
        };
        frame(&mut l);
        assert_eq!(names(&l.view), ["a", "b"]);
        add(&l, "c");
        let wait = frame(&mut l);
        assert_eq!(names(&l.view), ["a", "b"], "a change of the data waits for the next rebuild");
        assert!(wait <= REFRESH_EVERY, "and a frame is asked for by then: {wait:?}");
        std::thread::sleep(REFRESH_EVERY + Duration::from_millis(20));
        frame(&mut l);
        assert_eq!(names(&l.view), ["a", "b", "c"]);
        add(&l, "d");
        l.search = "d".into(); // the filter was typed: shown at once, with the newest data
        frame(&mut l);
        assert_eq!(names(&l.view), ["d"]);
    }

    #[test]
    fn what_is_derived_from_the_rows_is_kept_until_they_change() {
        let (mut l, mut d) = list_of(&["a", "b", "c"], |i| vec![String::new(), ["Running", "Running", "Error"][i].into()]);
        (d.cols[1].name, d.rev) = ("Status".into(), 1);
        *l.data.locked() = d;
        let counts = |l: &List| l.status_counts().unwrap().iter().map(|c| c.1).collect::<Vec<_>>();
        assert_eq!(counts(&l), [2, 0, 1, 0], "running, pending, failing, completed");
        {
            let mut d = l.data.locked();
            d.rows.insert("c".into(), row("c", "2", &["c", "Running"]));
            d.rev += 1;
        }
        assert_eq!(counts(&l), [3, 0, 0, 0], "a new revision: counted again");

        let m = Metrics::default();
        let build = |l: &mut List| {
            let s = {
                let d = l.data.locked();
                l.snapshot(&d)
            };
            l.refresh(&s, &m);
        };
        build(&mut l);
        l.sel = HashSet::from(["a".to_string()]);
        assert_eq!(names(&l.selected()), ["a"]);
        l.sel.insert("c".into());
        assert_eq!(names(&l.selected()), ["a", "c"], "the selection changed");
        l.sort = Some((DCol::Cell(0), false));
        build(&mut l);
        assert_eq!(names(&l.selected()), ["c", "a"], "the view changed");
        assert_eq!(l.derived.borrow().refreshes, 2);
        // The metrics being read again is no reason to rebuild unless the rows are sorted by them.
        build(&mut l);
        let m2 = Metrics { at: Some(Instant::now()), ..Default::default() };
        assert_eq!(Some(l.view_key(2, &m2)), l.key, "sorted by name: same view");
        l.sort = Some((DCol::Cpu, true));
        assert_ne!(l.view_key(2, &m2), l.view_key(2, &m), "sorted by CPU: the metrics are part of it");
    }

    #[test]
    fn sorts_by_images_and_by_usage() {
        let (mut l, mut d) = list_of(&[], |_| vec![]);
        d.cols.push(watch::Col { name: "Images".into(), ..Default::default() });
        for (n, images) in [("a", vec!["reg/x:2"]), ("b", vec!["reg/x:10"]), ("c", vec![])] {
            let r = Row { name: n.into(), namespace: "default".into(), uid: n.into(), rv: "1".into(), created: None, deleting: false, cells: vec![], images: images.into_iter().map(String::from).collect() };
            d.rows.insert(n.into(), Arc::new(r));
        }
        let mut m = Metrics::default();
        m.pods.insert(("default".into(), "a".into()), (0.5, 100.0));
        m.pods.insert(("default".into(), "b".into()), (2.0, 50.0));
        let order = |l: &mut List, d: &ListData, m: &Metrics, sort| {
            l.sort = Some(sort);
            l.refresh_now(d, m);
            names(&l.view).join("")
        };
        assert_eq!(order(&mut l, &d, &m, (DCol::Images, true)), "cba", "no image, then reg/x:10, then reg/x:2: as text");
        assert_eq!(order(&mut l, &d, &m, (DCol::Images, false)), "abc");
        assert_eq!(order(&mut l, &d, &m, (DCol::Cpu, true)), "cab", "no usage first");
        assert_eq!(order(&mut l, &d, &m, (DCol::Cpu, false)), "bac");
        assert_eq!(order(&mut l, &d, &m, (DCol::Mem, true)), "cba");
    }

    #[test]
    fn f64_keys_sort_like_total_cmp() {
        let v = [f64::NEG_INFINITY, -2.5, -1.0, -0.0, 0.0, 1e-9, 1.0, 7.5, f64::INFINITY];
        for (i, a) in v.iter().enumerate() {
            for b in &v[i..] {
                assert_eq!(f64_key(*a).cmp(&f64_key(*b)), a.total_cmp(b), "{a} {b}");
            }
        }
    }
}
