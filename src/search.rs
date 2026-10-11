//! Content search across Config Maps and Secrets (all namespaces or the selected ones).
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::{ApiResource, ListParams};
use kube::Client;

use crate::details::decode_value;
use crate::find::{self, Find, Matcher};
use crate::ops;
use crate::watch::{take, Pending, RED};

/// One key of a Config Map or Secret, decoded to text.
pub struct Entry {
    secret: bool,
    ns: String,
    name: String,
    key: String,
    value: String,
}

/// Helm keeps releases in secrets (gzipped, base64 inside): noise for a text search; the Helm page has them.
const SKIP_TYPES: &[&str] = &["helm.sh/release.v1"];

// Every text value is held in memory and scanned for each query, on a thread (see `Scan`).
async fn entries(client: &Client, secret: bool, ns: &[String]) -> Result<Vec<Entry>, String> {
    let ar = if secret { ApiResource::erase::<Secret>(&()) } else { ApiResource::erase::<ConfigMap>(&()) };
    let scopes = if ns.is_empty() { vec![String::new()] } else { ns.to_vec() };
    let mut out = vec![];
    for scope in scopes {
        let api = ops::api(client, &ar, &scope);
        let mut lp = ListParams::default().limit(250);
        loop {
            let page = api.list(&lp).await.map_err(|e| ops::err_text(&e.into()))?;
            for o in page.items {
                if secret && SKIP_TYPES.contains(&o.data["type"].as_str().unwrap_or_default()) {
                    continue;
                }
                let (ns, name) = (o.metadata.namespace.unwrap_or_default(), o.metadata.name.unwrap_or_default());
                for (key, v) in o.data["data"].as_object().into_iter().flatten() {
                    // Binary values (not UTF-8) are skipped.
                    if let Some(value) = v.as_str().and_then(|v| decode_value(v, secret)) {
                        out.push(Entry { secret, ns: ns.clone(), name: name.clone(), key: key.clone(), value });
                    }
                }
            }
            match page.metadata.continue_ {
                Some(c) if !c.is_empty() => lp = lp.continue_token(&c),
                _ => break,
            }
        }
    }
    Ok(out)
}

/// Both kinds; a kind that can't be listed (RBAC) is reported and skipped.
async fn fetch(client: Client, ns: Vec<String>) -> (Vec<Entry>, Vec<String>) {
    let (mut all, mut errors) = (vec![], vec![]);
    for secret in [false, true] {
        match entries(&client, secret, &ns).await {
            Ok(e) => all.extend(e),
            Err(e) => errors.push(format!("{}: {e}", if secret { "Secrets" } else { "Config Maps" })),
        }
    }
    (all, errors)
}

/// A matching key: entry index, matches in the value, and the line that shows the first one.
struct Hit {
    i: usize,
    n: usize,
    snippet: String,
}

/// The entries `m` matches; cut short (with what was found so far) once `stop` is set.
fn search(entries: &[Entry], m: &Matcher, stop: &AtomicBool) -> Vec<Hit> {
    let hit = |(i, e): (usize, &Entry)| {
        let r = m.ranges(&e.value);
        (!r.is_empty() || m.hit(&e.key) || m.hit(&e.name)).then(|| Hit { i, n: r.len(), snippet: snippet(&e.value, r.first()) })
    };
    entries.iter().enumerate().take_while(|_| !stop.load(Ordering::Relaxed)).filter_map(hit).collect()
}

/// What `hits` was computed for: (query, regex, case, load generation).
type Key = (String, bool, bool, usize);

/// A search of the entries for one query, running on a thread so typing never waits for it. Dropped
/// (and so stopped) when the query changes.
struct Scan {
    key: Key,
    stop: Arc<AtomicBool>,
    hits: Pending<Vec<Hit>>,
}

impl Scan {
    fn start(ctx: &egui::Context, key: Key, entries: Arc<Vec<Entry>>, m: Matcher) -> Scan {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let hits = Pending::spawn(ctx, async move { tokio::task::spawn_blocking(move || search(&entries, &m, &flag)).await.unwrap_or_default() });
        Scan { key, stop, hits }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The line holding `at` (or the first line), cut to ~200 chars around it.
fn snippet(v: &str, at: Option<&Range<usize>>) -> String {
    let at = at.map_or(0, |r| r.start);
    let (ls, le) = (v[..at].rfind('\n').map_or(0, |i| i + 1), v[at..].find('\n').map_or(v.len(), |i| at + i));
    let mut a = at.saturating_sub(60).max(ls);
    while !v.is_char_boundary(a) {
        a += 1;
    }
    let mut b = (a + 200).min(le);
    while !v.is_char_boundary(b) {
        b -= 1;
    }
    format!("{}{}{}", if a > ls { "…" } else { "" }, v[a..b].trim_end(), if b < le { "…" } else { "" })
}

/// A clicked result: `tab` for a double-click (object tab), else the details panel.
pub struct Picked {
    pub tab: bool,
    pub secret: bool,
    pub ns: String,
    pub name: String,
}

pub struct ContentSearch {
    client: Client,
    ns: Vec<String>,
    /// Query state (always open); handed to the details so the same text is highlighted there.
    pub find: Find,
    show_secrets: bool,
    /// Ctrl+F / Ctrl+K: focus the query box next frame.
    pub focus: bool,
    entries: Arc<Vec<Entry>>,
    /// Config Maps and Secrets the entries come from.
    objects: usize,
    errors: Vec<String>,
    load: Option<Pending<(Vec<Entry>, Vec<String>)>>,
    hits: Vec<Hit>,
    key: Option<Key>,
    scan: Option<Scan>,
    rev: usize,
    sel: Option<usize>,
}

impl ContentSearch {
    pub fn new(ctx: &egui::Context, client: Client, ns: Vec<String>) -> Self {
        let mut find = Find::default();
        find.open = true;
        let mut s = ContentSearch { client, ns, find, show_secrets: false, focus: true, entries: Arc::default(), objects: 0, errors: vec![], load: None, hits: vec![], key: None, scan: None, rev: 0, sel: None };
        s.reload(ctx, None);
        s
    }

    /// Fetch again; `ns` is a new namespace selection.
    pub fn reload(&mut self, ctx: &egui::Context, ns: Option<Vec<String>>) {
        if let Some(ns) = ns {
            self.ns = ns;
        }
        self.load = Some(Pending::spawn(ctx, fetch(self.client.clone(), self.ns.clone())));
    }

    pub fn ui(&mut self, ui: &mut Ui) -> Option<Picked> {
        if let Some((e, err)) = take(&mut self.load) {
            self.objects = e.iter().map(|e| (&e.ns, &e.name, e.secret)).collect::<std::collections::HashSet<_>>().len();
            (self.entries, self.errors, self.sel) = (Arc::new(e), err, None);
            self.rev += 1;
        }
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.find.query).hint_text("🔍 Text in values, keys or names… (Ctrl+F)").desired_width(320.0));
            if std::mem::take(&mut self.focus) {
                r.request_focus();
            }
            ui.toggle_value(&mut self.find.regex, ".*").on_hover_text("Regular expression");
            ui.toggle_value(&mut self.find.case, "Aa").on_hover_text("Match case");
            ui.toggle_value(&mut self.show_secrets, "👁 Secret values").on_hover_text("Show matching secret values in the results (hidden by default)");
            if ui.add_enabled(self.load.is_none(), egui::Button::new("⟳ Refresh")).on_hover_text("Fetch Config Maps and Secrets again").clicked() {
                self.reload(ui.ctx(), None);
            }
            if self.load.is_some() {
                crate::ui_kit::spinner(ui);
            }
        });

        let matcher = self.find.matcher();
        let key = (self.find.query.clone(), self.find.regex, self.find.case, self.rev);
        if self.key.as_ref() != Some(&key) && self.scan.as_ref().is_none_or(|s| s.key != key) {
            match &matcher {
                Some(m) => self.scan = Some(Scan::start(ui.ctx(), key, self.entries.clone(), m.clone())),
                None => (self.scan, self.hits, self.key, self.sel) = (None, vec![], Some(key), None), // no query, or an invalid regex
            }
        }
        if let Some((hits, key)) = self.scan.as_mut().and_then(|s| s.hits.poll().map(|h| (h, s.key.clone()))) {
            (self.scan, self.hits, self.key, self.sel) = (None, hits, Some(key), None);
        }
        let status = match (&matcher, self.find.query.is_empty()) {
            (_, true) => format!("{} keys in {} Config Maps and Secrets", self.entries.len(), self.objects),
            (None, false) => String::new(), // invalid regex, shown below
            (Some(_), false) if self.scan.is_some() => format!("searching {} keys…", self.entries.len()),
            (Some(_), false) => format!("{} matching keys · {} keys searched", self.hits.len(), self.entries.len()),
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(status).weak());
            if matcher.is_none() && !self.find.query.is_empty() {
                ui.colored_label(RED, "invalid regex");
            }
            for e in &self.errors {
                ui.colored_label(RED, e);
            }
        });
        ui.separator();

        let mut picked = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let cell = |ui: &mut Ui, text: &str, rt: RichText| {
            let hits = matcher.as_ref().map(|m| m.ranges(text)).unwrap_or_default();
            let mut job = (*egui::WidgetText::from(rt).into_layout_job(ui.style(), egui::FontSelection::Default, egui::Align::Center)).clone();
            find::overlay(&mut job, &hits, None);
            ui.add(egui::Label::new(job).truncate().selectable(false));
        };
        TableBuilder::new(ui)
            .id_salt("content-search")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .auto_shrink(false)
            .column(Column::initial(80.0).at_least(40.0).clip(true))
            .column(Column::initial(130.0).at_least(40.0).clip(true))
            .column(Column::initial(240.0).at_least(40.0).clip(true))
            .column(Column::initial(180.0).at_least(40.0).clip(true))
            .column(Column::remainder().at_least(120.0).clip(true))
            .header(row_h, |mut h| {
                for t in ["Kind", "Namespace", "Name", "Key", "Match"] {
                    h.col(|ui| {
                        ui.label(RichText::new(t).strong());
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, self.hits.len(), |mut row| {
                    let i = row.index();
                    let (h, e) = (&self.hits[i], &self.entries[self.hits[i].i]);
                    row.set_selected(self.sel == Some(i));
                    row.col(|ui| {
                        ui.label(if e.secret { "Secret" } else { "ConfigMap" });
                    });
                    row.col(|ui| {
                        ui.label(&e.ns);
                    });
                    row.col(|ui| cell(ui, &e.name, RichText::new(&e.name)));
                    row.col(|ui| cell(ui, &e.key, RichText::new(&e.key).strong()));
                    row.col(|ui| match (e.secret && !self.show_secrets, h.n) {
                        (_, 0) => {
                            ui.label(RichText::new("match in key or name").weak());
                        }
                        (true, n) => {
                            ui.label(RichText::new(format!("•••••••• {n} hidden match{}", if n == 1 { "" } else { "es" })).weak());
                        }
                        (false, n) => {
                            cell(ui, &h.snippet, RichText::new(&h.snippet).monospace());
                            if n > 1 {
                                ui.label(RichText::new(format!("+{}", n - 1)).weak());
                            }
                        }
                    });
                    let r = row.response();
                    if r.double_clicked() || r.clicked() {
                        self.sel = Some(i);
                        picked = Some(Picked { tab: r.double_clicked(), secret: e.secret, ns: e.ns.clone(), name: e.name.clone() });
                    }
                });
            });
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(secret: bool, name: &str, key: &str, value: &str) -> Entry {
        Entry { secret, ns: "default".into(), name: name.into(), key: key.into(), value: value.into() }
    }

    #[test]
    fn finds_values_keys_and_names() {
        let es = [e(false, "app", "config.yaml", "db:\n  host: pg.local\n  port: 5432"), e(true, "db-creds", "password", "hunter2"), e(false, "other", "x", "nothing")];
        let go = AtomicBool::new(false);
        let search = |es: &[Entry], m: &Matcher| search(es, m, &go);
        let hits = search(&es, &Matcher::plain("PG.LOCAL", false));
        assert_eq!((hits.len(), hits[0].i, hits[0].n, hits[0].snippet.as_str()), (1, 0, 1, "  host: pg.local"));
        let by_key = search(&es, &Matcher::plain("password", false));
        assert_eq!((by_key.len(), by_key[0].i, by_key[0].n), (1, 1, 0)); // key only: no value hits
        assert_eq!(search(&es, &Matcher::regex(r"^\d{4}$|hunter\d", false).unwrap()).len(), 1);
    }

    #[test]
    fn a_stopped_search_leaves_the_rest_unscanned() {
        let es: Vec<Entry> = (0..100).map(|i| e(false, "cm", "k", &format!("value {i}"))).collect();
        let m = Matcher::plain("value", false);
        assert_eq!(search(&es, &m, &AtomicBool::new(false)).len(), 100);
        assert!(search(&es, &m, &AtomicBool::new(true)).is_empty());
    }

    /// The scan runs on a thread: the results arrive a frame or more later, for the latest query only.
    #[tokio::test]
    async fn the_page_shows_hits_when_the_scan_is_done() {
        let mut h = crate::testkit::Harness::new(egui::vec2(900.0, 400.0));
        let mut s = ContentSearch::new(&h.ctx, crate::testkit::dummy_client(), vec![]);
        s.load = None;
        s.entries = Arc::new(vec![e(false, "app", "a.yaml", "host: pg.local"), e(true, "db", "password", "hunter2"), e(false, "other", "x", "nothing")]);
        s.find.query = "hunter".into();
        let mut frame = |s: &mut ContentSearch| h.frame(vec![], |ui| drop(s.ui(ui)));
        frame(&mut s);
        assert!(s.scan.is_some(), "the scan was started, not run in the frame");
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            frame(&mut s);
            if s.scan.is_none() {
                break;
            }
        }
        assert_eq!(s.hits.iter().map(|h| h.i).collect::<Vec<_>>(), [1]);
        // A new query replaces the results; an invalid regex shows none.
        (s.find.query, s.find.regex) = ("(".into(), true);
        frame(&mut s);
        assert!(s.scan.is_none() && s.hits.is_empty());
    }

    #[test]
    fn snippet_cuts_long_lines_on_char_boundaries() {
        let v = format!("first\n{}needle{}\nlast", "é".repeat(100), "x".repeat(300));
        let at = v.find("needle").unwrap();
        let s = snippet(&v, Some(&(at..at + 6)));
        assert!(s.starts_with('…') && s.ends_with('…') && s.contains("needle") && !s.contains('\n'));
        assert_eq!(snippet("a\nb", None), "a");
    }
}
