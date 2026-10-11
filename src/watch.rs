//! Live resource lists built on the API server's Table output (the columns `kubectl get` prints),
//! The background-task helpers (`tasks`), the value formatting (`units`) and the status colors (`theme`)
//! are re-exported here, where the views have always found them.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{AsyncBufReadExt, StreamExt};
use kube::api::{ApiResource, DynamicObject, ListParams, WatchParams};
use kube::core::Request;
use kube::{Client, Resource};
use serde::Deserialize;
use serde_json::Value;

pub use crate::tasks::{take, Bg, Lock, Pending};
pub use crate::theme::{cell_color, GREEN, ORANGE, RED};
pub use crate::units::{age, cmp_cells, date_cell};

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
    /// Marked by whatever draws this data; the watch wakes the window at full pace only while it is.
    pub shown: crate::repaint::Shown,
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

/// The names of everything at a collection path, asked for as a Table without its objects: on a
/// cluster with many CRDs their metadata alone (annotations) is megabytes, and only names are needed.
pub async fn names(client: &Client, path: &str) -> anyhow::Result<Vec<String>> {
    #[derive(Deserialize, Default)]
    struct Names {
        #[serde(default)]
        rows: Vec<NameRow>,
        /// A plain list, when a server ignores `as=Table`.
        #[serde(default)]
        items: Vec<Obj>,
    }
    #[derive(Deserialize)]
    struct NameRow {
        #[serde(default)]
        cells: Vec<Value>,
    }
    let req = accept_table(http::Request::get(format!("{path}?includeObject=None")).body(vec![])?);
    let t: Names = client.request(req).await?;
    let mut names: Vec<String> = t.rows.into_iter().filter_map(|r| r.cells.first().and_then(Value::as_str).map(String::from)).collect();
    names.extend(t.items.into_iter().map(|o| o.metadata.name));
    Ok(names)
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
    let shown = data.locked().shown.clone();
    loop {
        match sync(&client, &req, ns.as_deref(), fields.as_deref(), full, &data, &ctx).await {
            Ok(()) => backoff = 1, // 410 Gone: relist right away
            Err(e) => {
                {
                    let mut d = data.locked();
                    d.error = Some(crate::ops::err_text(&e));
                    d.error_since.get_or_insert_with(std::time::Instant::now);
                    d.rev += 1;
                }
                shown.wake(&ctx, Duration::ZERO);
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

async fn sync(client: &Client, req: &Request, ns: Option<&str>, fields: Option<&str>, full: bool, data: &Shared, ctx: &egui::Context) -> anyhow::Result<()> {
    let shape = |r: http::Request<Vec<u8>>| if full { include_object(accept_table(r)) } else { accept_table(r) };
    let shown = data.locked().shown.clone();
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
        let mut d = data.locked();
        d.cols = cols;
        match ns {
            Some(ns) => d.rows.retain(|_, r| r.namespace != ns),
            None => d.rows.clear(),
        }
        d.rows.extend(rows.into_iter().map(|r| (r.uid.clone(), Arc::new(r))));
        d.synced = true;
        static FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        crate::prof::trace_once(&FIRST, "first list synced");
        d.error = None;
        d.error_since = None;
        d.rev += 1;
    }
    shown.wake(ctx, Duration::ZERO);

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
                    if let Some(v) = apply(&mut data.locked(), &ev.kind, new) {
                        rv = v;
                    }
                    shown.wake(ctx, Duration::from_millis(250));
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::MockApi;
    use crate::units::human;
    use std::cmp::Ordering;

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

    /// `sync` against the fake server: (result, what the list holds, the requests made).
    async fn sync_against(watches: &[&str]) -> (anyhow::Result<()>, ListData, Vec<String>) {
        let api = MockApi::default().route("watch=true", watches).always("/pods", LIST);
        let client = api.client();
        let pod = ApiResource::erase::<k8s_openapi::api::core::v1::Pod>(&());
        let req = Request::new(DynamicObject::url_path(&pod, None));
        let data: Shared = Default::default();
        let r = sync(&client, &req, None, None, false, &data, &egui::Context::default()).await;
        let d = std::mem::take(&mut *data.locked());
        (r, d, api.requests())
    }

    fn table_event(kind: &str, name: &str, uid: &str, rv: &str) -> String {
        format!(r#"{{"type":"{kind}","object":{{"kind":"Table","rows":[{{"cells":["{name}"],"object":{{"metadata":{{"name":"{name}","namespace":"default","uid":"{uid}","resourceVersion":"{rv}"}}}}}}]}}}}"#)
    }

    #[tokio::test]
    async fn the_list_follows_watch_events_and_a_410_relists() {
        let gone = r#"{"type":"ERROR","object":{"code":410,"message":"too old resource version"}}"#;
        let events = [table_event("ADDED", "web-3", "u3", "44"), table_event("MODIFIED", "web-1", "u1", "45"), table_event("DELETED", "web-2", "u2", "46")].join("\n");
        // The first watch ends after its events (a closed connection): the next one resumes from the
        // last resourceVersion seen, and is told it is too old: `sync` returns so `run` relists.
        let (r, d, seen) = sync_against(&[&events, gone]).await;
        assert!(r.is_ok(), "410 is not an error: {r:?}");
        assert!(d.synced && d.error.is_none());
        let mut uids: Vec<_> = d.rows.keys().cloned().collect();
        uids.sort();
        assert_eq!(uids, ["u1", "u3"], "web-3 added, web-2 deleted");
        assert_eq!(d.rows["u1"].rv, "45", "web-1 modified");
        assert_eq!(d.cols.len(), 4, "columns come from the list");
        let watches: Vec<_> = seen.iter().filter(|u| u.contains("watch=true")).collect();
        assert_eq!(watches.len(), 2);
        assert!(watches[0].contains("resourceVersion=42"), "the first watch starts at the list's version: {}", watches[0]);
        assert!(watches[1].contains("resourceVersion=46"), "the second resumes after the last event: {}", watches[1]);
        assert!(seen[0].contains("limit=500"), "the list is paged: {}", seen[0]);
    }

    #[tokio::test]
    async fn a_watch_error_is_reported_and_the_list_stays() {
        let (r, d, _) = sync_against(&[r#"{"type":"ERROR","object":{"code":500,"message":"etcd is not available"}}"#]).await;
        assert!(r.is_err_and(|e| e.to_string().contains("etcd is not available")));
        assert_eq!(d.rows.len(), 2, "what was listed stays on screen");
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

    /// The names come from a Table asked for without its objects, or from a plain list.
    #[tokio::test]
    async fn names_of_a_collection() {
        let table = r#"{"kind":"Table","columnDefinitions":[{"name":"Name"},{"name":"Created At"}],"rows":[{"cells":["a.example.com","2026-01-01T00:00:00Z"]},{"cells":["b.example.com","2026-01-02T00:00:00Z"]}]}"#;
        let api = MockApi::default().always("/customresourcedefinitions", table);
        assert_eq!(names(&api.client(), "/apis/apiextensions.k8s.io/v1/customresourcedefinitions").await.unwrap(), ["a.example.com", "b.example.com"]);
        let uri = &api.requests()[0];
        assert!(uri.contains("includeObject=None"), "{uri}");
        let plain = r#"{"kind":"CustomResourceDefinitionList","items":[{"metadata":{"name":"c.example.com"}}]}"#;
        let api = MockApi::default().always("/customresourcedefinitions", plain);
        assert_eq!(names(&api.client(), "/apis/apiextensions.k8s.io/v1/customresourcedefinitions").await.unwrap(), ["c.example.com"]);
    }
}
