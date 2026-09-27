//! Live resource lists built on the API server's Table output (the columns `kubectl get` prints),
//! plus the two background-task helpers every view uses.
use std::cmp::Ordering;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::Color32;
use futures::{AsyncBufReadExt, StreamExt};
use kube::api::{ApiResource, DynamicObject, ListParams, WatchParams};
use kube::core::Request;
use kube::{Client, Resource};
use serde::Deserialize;
use serde_json::Value;

/// Task handle that aborts on drop: whoever shows the data owns the task.
pub struct Bg(tokio::task::JoinHandle<()>);

impl Bg {
    pub fn spawn(f: impl Future<Output = ()> + Send + 'static) -> Self {
        Bg(tokio::spawn(f))
    }
}

impl Drop for Bg {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One-shot async result polled by the UI each frame.
pub struct Pending<T>(tokio::sync::oneshot::Receiver<T>);

impl<T: Send + 'static> Pending<T> {
    pub fn spawn(ctx: &egui::Context, f: impl Future<Output = T> + Send + 'static) -> Self {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = tx.send(f.await);
            ctx.request_repaint();
        });
        Pending(rx)
    }

    /// `Some` exactly once, when the task finished.
    pub fn poll(&mut self) -> Option<T> {
        self.0.try_recv().ok()
    }
}

/// Poll an optional pending slot, clearing it once done.
pub fn take<T: Send + 'static>(slot: &mut Option<Pending<T>>) -> Option<T> {
    let r = slot.as_mut()?.poll();
    if r.is_some() {
        *slot = None;
    }
    r
}

pub const ACCEPT_TABLE: &str = "application/json;as=Table;v=v1;g=meta.k8s.io,application/json";

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Col {
    pub name: String,
    #[serde(rename = "type", default)]
    pub ty: String,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub rv: String,
    pub created: Option<jiff::Timestamp>,
    pub deleting: bool,
    pub cells: Vec<String>,
    /// Pods: the container images (`spec.containers[].image`), from the full object.
    pub images: Vec<String>,
}

#[derive(Default)]
pub struct ListData {
    pub cols: Vec<Col>,
    pub rows: HashMap<String, Arc<Row>>,
    pub rev: u64,
    pub error: Option<String>,
    /// When the current run of failures started (cleared by a successful sync).
    pub error_since: Option<std::time::Instant>,
    pub synced: bool,
}

impl ListData {
    /// The error, only once it affects what is shown: nothing loaded yet, or retries failing for 10s+.
    /// Brief reconnects while data is already on screen stay silent.
    pub fn visible_error(&self) -> Option<&str> {
        let persistent = self.error_since.is_some_and(|t| t.elapsed() > Duration::from_secs(10));
        self.error.as_deref().filter(|_| !self.synced || persistent)
    }
}

pub type Shared = Arc<Mutex<ListData>>;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Table {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    metadata: ListMeta,
    #[serde(default)]
    column_definitions: Vec<Col>,
    #[serde(default)]
    rows: Vec<RawRow>,
    /// Plain list, when a server ignores `as=Table`.
    #[serde(default)]
    items: Vec<Obj>,
}

#[derive(Deserialize, Default)]
struct ListMeta {
    #[serde(default, rename = "resourceVersion")]
    rv: String,
    #[serde(default, rename = "continue")]
    cont: String,
}

#[derive(Deserialize)]
struct RawRow {
    #[serde(default)]
    cells: Vec<Value>,
    #[serde(default)]
    object: Obj,
}

#[derive(Deserialize, Default)]
struct Obj {
    #[serde(default)]
    metadata: Meta,
    /// Only pods are listed with their full object; only their images are kept.
    #[serde(default)]
    spec: ObjSpec,
}

#[derive(Deserialize, Default)]
struct ObjSpec {
    #[serde(default)]
    containers: Vec<ObjContainer>,
}

#[derive(Deserialize, Default)]
struct ObjContainer {
    #[serde(default)]
    image: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Meta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    uid: String,
    #[serde(default)]
    resource_version: String,
    creation_timestamp: Option<jiff::Timestamp>,
    deletion_timestamp: Option<jiff::Timestamp>,
}

impl Row {
    fn new(o: Obj, cells: Vec<String>) -> Self {
        let m = o.metadata;
        Row {
            images: o.spec.containers.into_iter().map(|c| c.image).filter(|i| !i.is_empty()).collect(),
            uid: if m.uid.is_empty() { format!("{}/{}", m.namespace, m.name) } else { m.uid },
            name: m.name,
            namespace: m.namespace,
            rv: m.resource_version,
            created: m.creation_timestamp,
            deleting: m.deletion_timestamp.is_some(),
            cells,
        }
    }
}

fn fallback_cols() -> Vec<Col> {
    ["Name", "Age"].map(|n| Col { name: n.into(), ..Default::default() }).to_vec()
}

fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Columns + rows of a list response (Table, or plain list fallback).
fn parse_list(t: Table) -> (Vec<Col>, Vec<Row>, ListMeta) {
    if t.kind == "Table" {
        let rows = t.rows.into_iter().map(|r| Row::new(r.object, r.cells.iter().map(cell_text).collect())).collect();
        (t.column_definitions, rows, t.metadata)
    } else {
        let rows = t.items.into_iter().map(|o| { let n = o.metadata.name.clone(); Row::new(o, vec![n, String::new()]) }).collect();
        (fallback_cols(), rows, t.metadata)
    }
}

/// Rows carried by one watch event object.
fn event_rows(obj: Value) -> anyhow::Result<Vec<Row>> {
    if obj.get("kind").and_then(Value::as_str) == Some("Table") {
        Ok(parse_list(serde_json::from_value(obj)?).1)
    } else {
        let o: Obj = serde_json::from_value(obj)?;
        let n = o.metadata.name.clone();
        Ok(vec![Row::new(o, vec![n, String::new()])])
    }
}

/// Apply one watch event; returns the last resourceVersion seen.
fn apply(d: &mut ListData, kind: &str, rows: Vec<Row>) -> Option<String> {
    let mut rv = None;
    for r in rows {
        rv = Some(r.rv.clone());
        if kind == "DELETED" {
            d.rows.remove(&r.uid);
        } else {
            d.rows.insert(r.uid.clone(), Arc::new(r));
        }
    }
    d.rev += 1;
    rv
}

fn accept_table(mut req: http::Request<Vec<u8>>) -> http::Request<Vec<u8>> {
    req.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static(ACCEPT_TABLE));
    req
}

/// Table rows carrying the whole object, not just its metadata (pods: for their images).
fn include_object(mut req: http::Request<Vec<u8>>) -> http::Request<Vec<u8>> {
    let uri = req.uri().to_string();
    let sep = if uri.contains('?') { '&' } else { '?' };
    if let Ok(u) = format!("{uri}{sep}includeObject=Object").parse() {
        *req.uri_mut() = u;
    }
    req
}

/// Keep `data` in sync with a list+watch of `ar` (optionally one namespace / field selector). Runs forever.
pub async fn run(client: Client, ar: ApiResource, ns: Option<String>, fields: Option<String>, data: Shared, ctx: egui::Context) {
    let req = Request::new(DynamicObject::url_path(&ar, ns.as_deref()));
    // Pods have no image column in their table: list them with the object to read it.
    let full = ar.group.is_empty() && ar.kind == "Pod";
    let mut backoff = 1;
    loop {
        match sync(&client, &req, ns.as_deref(), fields.as_deref(), full, &data, &ctx).await {
            Ok(()) => backoff = 1, // 410 Gone: relist right away
            Err(e) => {
                {
                    let mut d = data.lock().unwrap();
                    d.error = Some(crate::ops::err_text(&e));
                    d.error_since.get_or_insert_with(std::time::Instant::now);
                    d.rev += 1;
                }
                ctx.request_repaint();
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

async fn sync(client: &Client, req: &Request, ns: Option<&str>, fields: Option<&str>, full: bool, data: &Shared, ctx: &egui::Context) -> anyhow::Result<()> {
    let shape = |r: http::Request<Vec<u8>>| if full { include_object(accept_table(r)) } else { accept_table(r) };
    let (mut cols, mut rows, mut cont) = (Vec::new(), Vec::new(), None);
    let mut rv = loop {
        let lp = ListParams { limit: Some(500), continue_token: cont.take(), field_selector: fields.map(String::from), ..Default::default() };
        let (c, r, meta) = parse_list(client.request::<Table>(shape(req.list(&lp)?)).await?);
        if cols.is_empty() {
            cols = c;
        }
        rows.extend(r);
        if meta.cont.is_empty() {
            break meta.rv;
        }
        cont = Some(meta.cont);
    };
    {
        let mut d = data.lock().unwrap();
        d.cols = cols;
        match ns {
            Some(ns) => d.rows.retain(|_, r| r.namespace != ns),
            None => d.rows.clear(),
        }
        d.rows.extend(rows.into_iter().map(|r| (r.uid.clone(), Arc::new(r))));
        d.synced = true;
        d.error = None;
        d.error_since = None;
        d.rev += 1;
    }
    ctx.request_repaint();

    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: String,
        object: Value,
    }
    loop {
        let wp = WatchParams { timeout: Some(290), bookmarks: false, field_selector: fields.map(String::from), ..Default::default() };
        let mut lines = client.request_stream(shape(req.watch(&wp, &rv)?)).await?.lines();
        loop {
            // Server closes the watch at 290s; silence beyond that means a dead connection.
            let line = match tokio::time::timeout(Duration::from_secs(330), lines.next()).await {
                Err(_) => anyhow::bail!("watch stalled"),
                Ok(None) => break,
                Ok(Some(l)) => l?,
            };
            let ev: Event = serde_json::from_str(&line)?;
            match ev.kind.as_str() {
                "ERROR" => {
                    if ev.object.get("code").and_then(Value::as_u64) == Some(410) {
                        return Ok(());
                    }
                    anyhow::bail!("{}", ev.object.get("message").and_then(Value::as_str).unwrap_or("watch error"));
                }
                "ADDED" | "MODIFIED" | "DELETED" => {
                    let new = event_rows(ev.object)?;
                    if let Some(v) = apply(&mut data.lock().unwrap(), &ev.kind, new) {
                        rv = v;
                    }
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
                _ => {}
            }
        }
    }
}

/// kubectl's HumanDuration.
pub fn human(secs: i64) -> String {
    let s = secs.max(0);
    let (m, h) = (s / 60, s / 3600);
    let d = h / 24;
    match () {
        _ if s < 120 => format!("{s}s"),
        _ if m < 10 && s % 60 == 0 => format!("{m}m"),
        _ if m < 10 => format!("{m}m{}s", s % 60),
        _ if m < 180 => format!("{m}m"),
        _ if h < 8 && m % 60 == 0 => format!("{h}h"),
        _ if h < 8 => format!("{h}h{}m", m % 60),
        _ if h < 48 => format!("{h}h"),
        _ if h < 192 && h % 24 == 0 => format!("{d}d"),
        _ if h < 192 => format!("{d}d{}h", h % 24),
        _ if d < 730 => format!("{d}d"),
        _ if d < 2920 && d % 365 == 0 => format!("{}y", d / 365),
        _ if d < 2920 => format!("{}y{}d", d / 365, d % 365),
        _ => format!("{}y", d / 365),
    }
}

pub fn age(ts: Option<jiff::Timestamp>) -> String {
    ts.map(|t| human(jiff::Timestamp::now().as_second() - t.as_second())).unwrap_or_default()
}

/// Age for a `date` typed cell (RFC3339 string); passes other text through.
pub fn date_cell(text: &str) -> String {
    text.parse::<jiff::Timestamp>().map(|t| age(Some(t))).unwrap_or_else(|_| text.to_string())
}

/// ASCII case-insensitive substring test without allocating.
pub fn contains_ci(hay: &str, needle: &str) -> bool {
    needle.is_empty() || hay.as_bytes().windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn lead_num(s: &str) -> Option<f64> {
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// Numeric when both cells start with a number (`3`, `1/2`, `5 (3m ago)`), else case-insensitive text.
// ponytail: ignores units (128Mi vs 1Gi); parse quantities if someone sorts by memory columns.
pub fn cmp_cells(a: &str, b: &str) -> Ordering {
    match (lead_num(a), lead_num(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal).then_with(|| a.cmp(b)),
        _ => a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase())),
    }
}

pub const GREEN: Color32 = Color32::from_rgb(46, 160, 67);
pub const ORANGE: Color32 = Color32::from_rgb(210, 153, 34);
pub const RED: Color32 = Color32::from_rgb(218, 54, 51);

/// Painted status dot (the default fonts have no reliable bullet glyph).
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// Status coloring for status-like columns.
pub fn cell_color(col: &str, text: &str) -> Option<Color32> {
    if !matches!(col, "Status" | "Phase" | "Type" | "Ready" | "Reason" | "Condition") {
        return None;
    }
    if let Some((a, b)) = text.split_once('/') {
        return match (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
            (Ok(a), Ok(b)) if a < b => Some(ORANGE),
            _ => None,
        };
    }
    const RED_WORDS: &[&str] = &["NotReady", "Failed", "Error", "BackOff", "ErrImage", "OOMKilled", "Evicted", "Lost", "Invalid", "Unknown", "False", "Unhealthy"];
    const ORANGE_WORDS: &[&str] = &["Not ready", "SchedulingDisabled", "Pending", "Creating", "Initializing", "Init:", "Terminating", "Released", "Warning", "Progressing", "Suspended"];
    const GREEN_WORDS: &[&str] = &["Running", "Active", "Bound", "Available", "Succeeded", "Completed", "Complete", "Ready", "True", "deployed", "Healthy"];
    let has = |ws: &[&str]| ws.iter().any(|w| text.contains(w));
    if has(RED_WORDS) {
        Some(RED)
    } else if has(ORANGE_WORDS) {
        Some(ORANGE)
    } else if has(GREEN_WORDS) {
        Some(GREEN)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"42"},
      "columnDefinitions":[{"name":"Name","type":"string","format":"name","priority":0},
                           {"name":"Ready","type":"string","priority":0},
                           {"name":"Restarts","type":"string","priority":0},
                           {"name":"IP","type":"string","priority":1}],
      "rows":[{"cells":["web-1","1/1",0,"10.0.0.5"],"object":{"kind":"PartialObjectMetadata","metadata":
                {"name":"web-1","namespace":"default","uid":"u1","resourceVersion":"40","creationTimestamp":"2026-01-01T00:00:00Z"}}},
              {"cells":["web-2","0/1","3 (5m ago)",null],"object":{"metadata":
                {"name":"web-2","namespace":"default","uid":"u2","resourceVersion":"41","deletionTimestamp":"2026-01-02T00:00:00Z"}}}]}"#;

    #[test]
    fn parses_table_and_applies_events() {
        let (cols, rows, meta) = parse_list(serde_json::from_str(LIST).unwrap());
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[3].priority, 1);
        assert_eq!(meta.rv, "42");
        assert_eq!(rows[0].cells, vec!["web-1", "1/1", "0", "10.0.0.5"]);
        assert_eq!(rows[1].cells[3], "");
        assert!(rows[1].deleting && !rows[0].deleting);
        assert!(rows[0].created.is_some());

        let mut d = ListData::default();
        apply(&mut d, "ADDED", rows);
        assert_eq!(d.rows.len(), 2);
        let ev = serde_json::json!({"kind":"Table","rows":[{"cells":["web-1"],"object":{"metadata":{"name":"web-1","uid":"u1","resourceVersion":"43"}}}]});
        let rv = apply(&mut d, "DELETED", event_rows(ev).unwrap());
        assert_eq!(rv.as_deref(), Some("43"));
        assert_eq!(d.rows.len(), 1);
        assert!(d.rows.contains_key("u2"));
    }

    #[test]
    fn plain_list_fallback() {
        let (cols, rows, _) = parse_list(serde_json::from_str(r#"{"kind":"FooList","items":[{"metadata":{"name":"a","uid":"x"}}]}"#).unwrap());
        assert_eq!(cols[0].name, "Name");
        assert_eq!(rows[0].cells[0], "a");
    }

    #[test]
    fn human_durations_match_kubectl() {
        assert_eq!(human(5), "5s");
        assert_eq!(human(119), "119s");
        assert_eq!(human(150), "2m30s");
        assert_eq!(human(600), "10m");
        assert_eq!(human(3 * 3600 + 5 * 60), "3h5m");
        assert_eq!(human(30 * 3600), "30h");
        assert_eq!(human(3 * 86400 + 4 * 3600), "3d4h");
        assert_eq!(human(40 * 86400), "40d");
        assert_eq!(human(800 * 86400), "2y70d");
        assert_eq!(human(-4), "0s");
    }

    #[test]
    fn sorting_and_colors() {
        assert_eq!(cmp_cells("10", "9"), Ordering::Greater);
        assert_eq!(cmp_cells("3 (5m ago)", "12"), Ordering::Less);
        assert_eq!(cmp_cells("abc", "ABD"), Ordering::Less);
        assert_eq!(cell_color("Status", "CrashLoopBackOff"), Some(RED));
        assert_eq!(cell_color("Status", "Ready,SchedulingDisabled"), Some(ORANGE));
        assert_eq!(cell_color("Status", "NotReady"), Some(RED));
        assert_eq!(cell_color("Status", "Running"), Some(GREEN));
        assert_eq!(cell_color("Ready", "0/1"), Some(ORANGE));
        assert_eq!(cell_color("Ready", "1/1"), None);
        assert_eq!(cell_color("Name", "Failed"), None);
    }

    #[test]
    fn pod_rows_keep_their_images() {
        let t = serde_json::json!({"kind":"Table","metadata":{"resourceVersion":"7"},"columnDefinitions":[{"name":"Name","type":"string"}],
            "rows":[{"cells":["web-1"],"object":{"kind":"Pod","metadata":{"name":"web-1","uid":"u1"},
                "spec":{"containers":[{"name":"app","image":"ghcr.io/x/web:1.2.3"},{"name":"proxy","image":"envoy:v1.30"}]}}}]});
        let (_, rows, _) = parse_list(serde_json::from_value(t).unwrap());
        assert_eq!(rows[0].images, vec!["ghcr.io/x/web:1.2.3", "envoy:v1.30"]);
    }
}
