//! Cluster I/O besides live lists: discovery, get/edit/apply/delete, actions, metrics,
//! and the external commands (kubectl, helm) we deliberately shell out to.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{Api, ApiResource, DeleteParams, DynamicObject, ListParams, Patch, PatchParams, PostParams};
use kube::discovery::{Discovery, Scope};
use kube::Client;
use serde_json::{json, Value};

use crate::kubeconfig::Ctx;
use crate::watch::Bg;

pub type Res<T = String> = Result<T, String>;

pub fn err_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<kube::Error>() {
        Some(kube::Error::Api(s)) if !s.message.is_empty() => s.message.clone(),
        _ => format!("{e:#}"),
    }
}

fn res<T>(r: anyhow::Result<T>) -> Res<T> {
    r.map_err(|e| err_text(&e))
}

#[derive(Clone, Debug)]
pub struct Kind {
    pub ar: ApiResource,
    pub namespaced: bool,
    pub verbs: Vec<String>,
}

impl Kind {
    pub fn can(&self, verb: &str) -> bool {
        self.verbs.iter().any(|v| v == verb)
    }
    pub fn is(&self, group: &str, kind: &str) -> bool {
        self.ar.group == group && self.ar.kind == kind
    }
}

/// Every listable resource, preferred versions only. One broken aggregated API must not break the cluster.
pub async fn discover(client: &Client) -> anyhow::Result<Vec<Kind>> {
    let aggregated: Vec<_> = match Discovery::new(client.clone()).run_aggregated().await {
        Ok(d) => d.groups().flat_map(|g| g.recommended_resources()).collect(),
        Err(_) => vec![],
    };
    // Aggregated discovery exists since Kubernetes 1.26; older servers (k3s v1.23…) answer with the
    // plain lists, which parse as *no* resources. Without the core kinds, ask group by group.
    let resources = if aggregated.iter().any(|(ar, _)| ar.group.is_empty() && ar.kind == "Pod") {
        aggregated
    } else {
        let mut names = vec![String::new()];
        names.extend(client.list_api_groups().await?.groups.into_iter().map(|g| g.name));
        // All groups at once: one at a time takes seconds on a remote cluster.
        let mut set = tokio::task::JoinSet::new();
        for n in names {
            let client = client.clone();
            set.spawn(async move { kube::discovery::group(&client, &n).await.ok() });
        }
        let mut out = vec![];
        while let Some(done) = set.join_next().await {
            if let Ok(Some(g)) = done {
                out.extend(g.recommended_resources());
            }
        }
        out
    };
    Ok(resources
        .into_iter()
        .filter(|(_, caps)| caps.supports_operation("list"))
        .map(|(ar, caps)| Kind { namespaced: caps.scope == Scope::Namespaced, verbs: caps.operations.clone(), ar })
        .collect())
}

pub fn api(client: &Client, ar: &ApiResource, ns: &str) -> Api<DynamicObject> {
    if ns.is_empty() { Api::all_with(client.clone(), ar) } else { Api::namespaced_with(client.clone(), ns, ar) }
}

pub async fn get(client: Client, ar: ApiResource, ns: String, name: String) -> Res<Value> {
    res(async { Ok(serde_json::to_value(api(&client, &ar, &ns).get(&name).await?)?) }.await)
}

/// YAML for viewing/editing: managedFields are noise.
pub fn to_yaml(obj: &Value) -> String {
    let mut o = obj.clone();
    if let Some(m) = o.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("managedFields");
    }
    serde_saphyr::to_string(&o).unwrap_or_else(|e| format!("# cannot render YAML: {e}"))
}

pub async fn replace_yaml(client: Client, ar: ApiResource, ns: String, name: String, yaml: String) -> Res {
    match serde_saphyr::from_str::<Value>(&yaml) {
        Ok(obj) => replace(client, ar, ns, name, obj).await,
        Err(e) => Err(format!("invalid YAML: {e}")),
    }
}

/// PUT the whole object; its resourceVersion makes concurrent edits fail instead of clobbering.
pub async fn replace(client: Client, ar: ApiResource, ns: String, name: String, obj: Value) -> Res {
    res(async {
        let obj: DynamicObject = serde_json::from_value(obj)?;
        api(&client, &ar, &ns).replace(&name, &PostParams::default(), &obj).await?;
        Ok(format!("Saved {} {name}", ar.kind))
    }
    .await)
}

/// Group of an `apiVersion` ("apps/v1" → "apps", "v1" → "").
pub fn api_group(api_version: &str) -> &str {
    api_version.rsplit_once('/').map_or("", |(g, _)| g)
}

/// "k=v,k2=v2" from a label map; `None` when empty (an empty selector would match everything).
pub fn label_selector(labels: &Value) -> Option<String> {
    let m = labels.as_object().filter(|m| !m.is_empty())?;
    Some(m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(","))
}

/// An object shown (and linked) in another object's details.
#[derive(Clone, Debug)]
pub struct Rel {
    pub group: String,
    pub kind: String,
    pub ns: String,
    pub name: String,
    pub status: String,
}

pub fn pod_status(p: &Pod) -> String {
    if p.metadata.deletion_timestamp.is_some() {
        return "Terminating".into();
    }
    let st = p.status.as_ref();
    let waiting = st.and_then(|s| s.container_statuses.as_ref()).into_iter().flatten().find_map(|c| c.state.as_ref()?.waiting.as_ref()?.reason.clone());
    waiting.or_else(|| st.and_then(|s| s.phase.clone())).unwrap_or_default()
}

/// Objects that belong to `obj`: pods of workloads/services/nodes, ReplicaSets of a Deployment, Jobs of a CronJob.
pub async fn related(client: Client, kind: Kind, obj: Value) -> Res<Vec<Rel>> {
    res(async {
        let (meta, spec) = (&obj["metadata"], &obj["spec"]);
        let ns = meta["namespace"].as_str().unwrap_or_default().to_string();
        let uid = meta["uid"].as_str().unwrap_or_default();
        let owned = |refs: &Option<Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference>>| refs.iter().flatten().any(|o| o.uid == uid);
        let mut out = vec![];
        let pods_lp = match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
            ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") | ("batch", "Job") => label_selector(&spec["selector"]["matchLabels"]).map(|s| ListParams::default().labels(&s)),
            ("", "Service") => label_selector(&spec["selector"]).map(|s| ListParams::default().labels(&s)),
            ("", "Node") => Some(ListParams::default().fields(&format!("spec.nodeName={}", meta["name"].as_str().unwrap_or_default()))),
            _ => None,
        };
        if kind.is("apps", "Deployment") {
            let rs: Api<k8s_openapi::api::apps::v1::ReplicaSet> = Api::namespaced(client.clone(), &ns);
            let lp = label_selector(&spec["selector"]["matchLabels"]).map(|s| ListParams::default().labels(&s)).unwrap_or_default();
            for r in rs.list(&lp).await?.items.into_iter().filter(|r| owned(&r.metadata.owner_references)) {
                let (want, ready) = (r.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0), r.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0));
                out.push(Rel { group: "apps".into(), kind: "ReplicaSet".into(), ns: ns.clone(), name: r.metadata.name.unwrap_or_default(), status: format!("{ready}/{want}") });
            }
        }
        if kind.is("batch", "CronJob") {
            let jobs: Api<k8s_openapi::api::batch::v1::Job> = Api::namespaced(client.clone(), &ns);
            for j in jobs.list(&ListParams::default()).await?.items.into_iter().filter(|j| owned(&j.metadata.owner_references)) {
                let s = j.status.unwrap_or_default();
                let status = if s.succeeded.unwrap_or(0) > 0 { "Complete" } else if s.failed.unwrap_or(0) > 0 { "Failed" } else { "Running" };
                out.push(Rel { group: "batch".into(), kind: "Job".into(), ns: ns.clone(), name: j.metadata.name.unwrap_or_default(), status: status.into() });
            }
        }
        if let Some(lp) = pods_lp {
            let pods: Api<Pod> = if kind.namespaced { Api::namespaced(client.clone(), &ns) } else { Api::all(client.clone()) };
            for p in pods.list(&lp).await?.items {
                out.push(Rel { group: String::new(), kind: "Pod".into(), status: pod_status(&p), ns: p.metadata.namespace.unwrap_or_default(), name: p.metadata.name.unwrap_or_default() });
            }
        }
        Ok(out)
    }
    .await)
}

/// Pods behind the given objects (pods themselves, or the pods of workloads) for merged logs.
pub async fn pods_of(client: Client, targets: Vec<(Kind, String, String)>) -> Res<Vec<(String, String)>> {
    let mut out = vec![];
    for (kind, ns, name) in targets {
        if kind.is("", "Pod") {
            out.push((ns, name));
            continue;
        }
        let obj = get(client.clone(), kind.ar.clone(), ns, name).await?;
        out.extend(related(client.clone(), kind, obj).await?.into_iter().filter(|r| r.kind == "Pod").map(|r| (r.ns, r.name)));
    }
    out.sort();
    out.dedup();
    if out.is_empty() { Err("no pods found".into()) } else { Ok(out) }
}

/// Name for a node-shell pod: DNS-safe and within the 63-char limit.
pub fn node_shell_name(node: &str, n: i64) -> String {
    let base: String = node.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).take(40).collect();
    format!("kxs-shell-{}-{}", base.trim_matches('-'), n.rem_euclid(100_000))
}

/// Privileged pod pinned to `node`, sharing its PID/network/IPC namespaces (entered with nsenter).
pub fn node_shell_pod(node: &str, name: &str, image: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": "kube-system", "labels": {"app.kubernetes.io/managed-by": "kxs-watcher"}},
        "spec": {
            "nodeName": node, "hostPID": true, "hostNetwork": true, "hostIPC": true,
            "restartPolicy": "Never", "terminationGracePeriodSeconds": 0,
            "tolerations": [{"operator": "Exists"}],
            // Safety net: a pod left behind (app killed) stops by itself after 4h.
            "containers": [{"name": "shell", "image": image, "command": ["sleep", "14400"], "securityContext": {"privileged": true}}]
        }
    })
}

/// Start a node-shell pod and wait until it runs; returns (namespace, pod).
pub async fn node_shell(client: Client, node: String, image: String) -> Res<(String, String)> {
    let ns = "kube-system".to_string();
    let name = node_shell_name(&node, jiff::Timestamp::now().as_millisecond());
    let api: Api<Pod> = Api::namespaced(client, &ns);
    let started: anyhow::Result<()> = async {
        let pod: Pod = serde_json::from_value(node_shell_pod(&node, &name, &image))?;
        api.create(&PostParams::default(), &pod).await?;
        for _ in 0..90 {
            let p = api.get(&name).await?;
            let status = pod_status(&p);
            if status == "Running" {
                return Ok(());
            }
            if matches!(status.as_str(), "ErrImagePull" | "ImagePullBackOff" | "InvalidImageName" | "CreateContainerConfigError" | "Failed") {
                anyhow::bail!("node shell pod: {status} (image {image}; set another in Settings)");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        anyhow::bail!("node shell pod did not start within 90s")
    }
    .await;
    match started {
        Ok(()) => Ok((ns, name)),
        Err(e) => {
            let _ = api.delete(&name, &DeleteParams::default()).await;
            Err(err_text(&e))
        }
    }
}

/// Run a batch of actions concurrently and summarize them in one message.
pub async fn all<F: std::future::Future<Output = Res>>(jobs: Vec<F>, done: String) -> Res {
    let n = jobs.len();
    let errs: Vec<String> = futures::future::join_all(jobs).await.into_iter().filter_map(Result::err).collect();
    if errs.is_empty() { Ok(done) } else { Err(format!("{} of {n} failed: {}", errs.len(), errs.join("; "))) }
}

/// Create/update every `---` document (server-side apply; POST when only generateName is set).
pub async fn apply_yaml(client: Client, kinds: Arc<Vec<Kind>>, default_ns: String, yaml: String) -> Res {
    res(async {
        let docs: Vec<Value> = serde_saphyr::from_multiple(&yaml)?;
        let mut done = vec![];
        for doc in docs.into_iter().filter(|d| d.is_object()) {
            let obj: DynamicObject = serde_json::from_value(doc)?;
            let tm = obj.types.clone().ok_or_else(|| anyhow::anyhow!("document without apiVersion/kind"))?;
            let kind = kinds
                .iter()
                .find(|k| k.ar.api_version == tm.api_version && k.ar.kind == tm.kind)
                .ok_or_else(|| anyhow::anyhow!("unknown kind {} {}", tm.api_version, tm.kind))?;
            let ns = if kind.namespaced { obj.metadata.namespace.clone().unwrap_or_else(|| default_ns.clone()) } else { String::new() };
            let a = api(&client, &kind.ar, &ns);
            let created = match obj.metadata.name.clone() {
                Some(name) => a.patch(&name, &PatchParams::apply("kxs-watcher").force(), &Patch::Apply(&obj)).await?,
                None => a.create(&PostParams::default(), &obj).await?,
            };
            done.push(format!("{}/{}", tm.kind, created.metadata.name.unwrap_or_default()));
        }
        anyhow::ensure!(!done.is_empty(), "nothing to apply");
        Ok(format!("Applied {}", done.join(", ")))
    }
    .await)
}

pub async fn delete(client: Client, ar: ApiResource, ns: String, name: String) -> Res {
    res(async {
        api(&client, &ar, &ns).delete(&name, &DeleteParams::default()).await?;
        Ok(format!("Deleted {} {name}", ar.kind))
    }
    .await)
}

pub async fn merge_patch(client: Client, ar: ApiResource, ns: String, name: String, patch: Value, done: String) -> Res {
    res(async {
        api(&client, &ar, &ns).patch(&name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
        Ok(done)
    }
    .await)
}

pub fn scale_patch(replicas: i64) -> Value {
    json!({"spec": {"replicas": replicas}})
}

pub fn restart_patch() -> Value {
    json!({"spec": {"template": {"metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": jiff::Timestamp::now().to_string()}}}}})
}

/// Job name for a manual CronJob run; Job names must stay within 63 chars.
pub fn manual_job_name(cronjob: &str, now_secs: i64) -> String {
    let base: String = cronjob.chars().take(63 - "-manual-99999".len()).collect();
    format!("{}-manual-{}", base.trim_end_matches('-'), now_secs % 100_000)
}

pub async fn trigger_cronjob(client: Client, ns: String, name: String) -> Res {
    res(async {
        let cj_ar = ApiResource::erase::<k8s_openapi::api::batch::v1::CronJob>(&());
        let job_ar = ApiResource::erase::<k8s_openapi::api::batch::v1::Job>(&());
        let cj = serde_json::to_value(api(&client, &cj_ar, &ns).get(&name).await?)?;
        let tpl = &cj["spec"]["jobTemplate"];
        let job_name = manual_job_name(&name, jiff::Timestamp::now().as_second());
        let job: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {
                "name": job_name, "namespace": ns,
                "labels": tpl["metadata"]["labels"],
                "annotations": {"cronjob.kubernetes.io/instantiate": "manual"},
                "ownerReferences": [{"apiVersion": "batch/v1", "kind": "CronJob", "name": name, "uid": cj["metadata"]["uid"], "controller": true}]
            },
            "spec": tpl["spec"]
        }))?;
        api(&client, &job_ar, &ns).create(&PostParams::default(), &job).await?;
        Ok(format!("Created Job {job_name}"))
    }
    .await)
}

#[derive(Clone)]
pub struct Ev {
    pub kind: String,
    pub reason: String,
    pub message: String,
    pub age: String,
    pub count: i32,
}

pub async fn events_for(client: Client, ns: String, uid: String) -> Res<Vec<Ev>> {
    res(async {
        let api: Api<Event> = if ns.is_empty() { Api::all(client) } else { Api::namespaced(client, &ns) };
        let mut list = api.list(&ListParams::default().fields(&format!("involvedObject.uid={uid}"))).await?.items;
        let last = |e: &Event| e.last_timestamp.as_ref().map(|t| t.0).or(e.event_time.as_ref().map(|t| t.0)).or(e.metadata.creation_timestamp.as_ref().map(|t| t.0));
        list.sort_by_key(|e| std::cmp::Reverse(last(e)));
        Ok(list
            .iter()
            .map(|e| Ev {
                kind: e.type_.clone().unwrap_or_default(),
                reason: e.reason.clone().unwrap_or_default(),
                message: e.message.clone().unwrap_or_default(),
                age: crate::watch::age(last(e)),
                count: e.count.unwrap_or(1),
            })
            .collect())
    }
    .await)
}

/// Kubernetes quantity → f64 (cores for CPU, bytes for memory).
pub fn parse_qty(s: &str) -> f64 {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+')).unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let n: f64 = num.parse().unwrap_or(0.0);
    let mult = match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        e if e.starts_with(['e', 'E']) => 10f64.powf(e[1..].parse().unwrap_or(0.0)),
        _ => 1.0,
    };
    n * mult
}

pub fn fmt_cpu(cores: f64) -> String {
    if cores < 1.0 { format!("{:.0}m", cores * 1000.0) } else { format!("{cores:.2}") }
}

pub fn fmt_bytes(b: f64) -> String {
    const U: [&str; 5] = ["B", "Ki", "Mi", "Gi", "Ti"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i >= 3 { format!("{v:.1}{}", U[i]) } else { format!("{v:.0}{}", U[i]) }
}

#[derive(Default)]
pub struct Metrics {
    /// node → (cpu cores, memory bytes) in use
    pub nodes: HashMap<String, (f64, f64)>,
    /// (namespace, pod) → (cpu, memory) in use
    pub pods: HashMap<(String, String), (f64, f64)>,
    /// node → (allocatable cpu, memory, pods)
    pub alloc: HashMap<String, (f64, f64, f64)>,
    pub error: Option<String>,
    pub at: Option<std::time::Instant>,
}

async fn get_json(client: &Client, path: &str) -> anyhow::Result<Value> {
    Ok(client.request::<Value>(http::Request::get(path).body(vec![])?).await?)
}

/// Object counts for the sidebar, refreshed every minute. A `limit=1` list reads
/// `remainingItemCount`, so counting stays cheap on big clusters; kinds that can't be counted
/// that way (or that RBAC forbids) are left out.
pub async fn count_loop(client: Client, kinds: Vec<Kind>, ns: Vec<String>, out: Arc<Mutex<HashMap<(String, String), usize>>>, ctx: egui::Context) {
    loop {
        for k in kinds.iter().filter(|k| k.can("list")) {
            let scopes: Vec<String> = if k.namespaced && !ns.is_empty() { ns.clone() } else { vec![String::new()] };
            let mut total = Some(0usize);
            for scope in &scopes {
                total = match api(&client, &k.ar, scope).list_metadata(&ListParams::default().limit(1)).await {
                    Ok(l) => {
                        let more = l.metadata.continue_.as_deref().is_some_and(|c| !c.is_empty());
                        match l.metadata.remaining_item_count {
                            Some(n) => total.map(|t| t + l.items.len() + n.max(0) as usize),
                            None if !more => total.map(|t| t + l.items.len()),
                            None => None, // the server didn't say how many remain
                        }
                    }
                    Err(_) => None,
                };
                if total.is_none() {
                    break;
                }
            }
            if let Some(n) = total {
                out.lock().unwrap().insert((k.ar.group.clone(), k.ar.kind.clone()), n);
            }
        }
        ctx.request_repaint();
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// Poll metrics-server (if present) and node allocatable every 30s.
pub async fn metrics_loop(client: Client, has_metrics: bool, out: Arc<Mutex<Metrics>>, ctx: egui::Context) {
    loop {
        let r: anyhow::Result<Metrics> = async {
            let mut m = Metrics::default();
            for n in Api::<Node>::all(client.clone()).list(&ListParams::default()).await?.items {
                let a = n.status.and_then(|s| s.allocatable).unwrap_or_default();
                let q = |k: &str| a.get(k).map(|q| parse_qty(&q.0)).unwrap_or(0.0);
                m.alloc.insert(n.metadata.name.unwrap_or_default(), (q("cpu"), q("memory"), q("pods")));
            }
            if has_metrics {
                let usage = |v: &Value| (parse_qty(v["cpu"].as_str().unwrap_or("0")), parse_qty(v["memory"].as_str().unwrap_or("0")));
                for n in get_json(&client, "/apis/metrics.k8s.io/v1beta1/nodes").await?["items"].as_array().into_iter().flatten() {
                    m.nodes.insert(n["metadata"]["name"].as_str().unwrap_or_default().into(), usage(&n["usage"]));
                }
                for p in get_json(&client, "/apis/metrics.k8s.io/v1beta1/pods").await?["items"].as_array().into_iter().flatten() {
                    let sum = p["containers"].as_array().into_iter().flatten().map(|c| usage(&c["usage"])).fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
                    let key = (p["metadata"]["namespace"].as_str().unwrap_or_default().into(), p["metadata"]["name"].as_str().unwrap_or_default().into());
                    m.pods.insert(key, sum);
                }
            }
            Ok(m)
        }
        .await;
        {
            let mut o = out.lock().unwrap();
            match r {
                Ok(m) => *o = Metrics { at: Some(std::time::Instant::now()), ..m },
                Err(e) => o.error = Some(err_text(&e)),
            }
        }
        ctx.request_repaint();
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// Process without a console window flashing up.
pub fn cmd(program: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(program);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

pub fn tool_available(program: &str) -> bool {
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000);
    c.arg("version").output().map(|o| o.status.success()).unwrap_or(false)
}

pub async fn helm(ctx: Ctx, args: Vec<String>) -> Res {
    let out = cmd("helm")
        .arg("--kubeconfig")
        .arg(&ctx.file)
        .arg("--kube-context")
        .arg(&ctx.name)
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("helm: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub struct Forward {
    pub ns: String,
    pub target: String,
    pub remote: u16,
    /// Local port once kubectl reports it; `status` explains otherwise.
    pub local: Arc<Mutex<Option<u16>>>,
    pub status: Arc<Mutex<String>>,
    _bg: Bg,
}

/// `kubectl port-forward` on a random local port; stopped (process killed) when dropped.
pub fn port_forward(kctx: &Ctx, ns: &str, target: &str, remote: u16, ctx: &egui::Context) -> Forward {
    use tokio::io::AsyncBufReadExt;
    let (local, status) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new("starting".to_string())));
    let mut c = cmd("kubectl");
    c.arg("--kubeconfig").arg(&kctx.file).args(["--context", &kctx.name, "-n", ns, "port-forward", target, &format!(":{remote}")]);
    c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let (l, s, ctx) = (local.clone(), status.clone(), ctx.clone());
    let bg = Bg::spawn(async move {
        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => return *s.lock().unwrap() = format!("kubectl: {e}"),
        };
        let mut out = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut err = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
        let mut last_err = String::new();
        loop {
            tokio::select! {
                Ok(Some(line)) = out.next_line() => {
                    // "Forwarding from 127.0.0.1:54321 -> 80"
                    if let Some(p) = line.strip_prefix("Forwarding from 127.0.0.1:").and_then(|r| r.split_whitespace().next()).and_then(|p| p.parse().ok()) {
                        *l.lock().unwrap() = Some(p);
                        *s.lock().unwrap() = "active".into();
                    }
                }
                Ok(Some(line)) = err.next_line() => last_err = line,
                st = child.wait() => {
                    *l.lock().unwrap() = None;
                    *s.lock().unwrap() = format!("exited ({}) {last_err}", st.map(|s| s.to_string()).unwrap_or_default());
                    break;
                }
            }
            ctx.request_repaint();
        }
        ctx.request_repaint();
    });
    Forward { ns: ns.into(), target: target.into(), remote, local, status, _bg: bg }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        assert_eq!(parse_qty("250m"), 0.25);
        assert!((parse_qty("123456789n") - 0.123456789).abs() < 1e-12);
        assert_eq!(parse_qty("1Gi"), 1024f64.powi(3));
        assert_eq!(parse_qty("1e3"), 1000.0);
        assert_eq!(parse_qty("2"), 2.0);
        assert_eq!(parse_qty("128974848"), 128974848.0);
        assert_eq!(fmt_cpu(0.25), "250m");
        assert_eq!(fmt_bytes(1024f64.powi(3) * 1.5), "1.5Gi");
        assert_eq!(fmt_bytes(300.0 * 1024.0 * 1024.0), "300Mi");
    }

    #[test]
    fn groups_and_selectors() {
        assert_eq!(api_group("apps/v1"), "apps");
        assert_eq!(api_group("v1"), "");
        assert_eq!(api_group("gateway.networking.k8s.io/v1beta1"), "gateway.networking.k8s.io");
        assert_eq!(label_selector(&json!({"app": "web", "tier": "fe"})).as_deref(), Some("app=web,tier=fe"));
        assert_eq!(label_selector(&json!({})), None); // never "match everything"
        assert_eq!(label_selector(&Value::Null), None);
    }

    #[test]
    fn node_shell_pod_spec() {
        let name = node_shell_name(&format!("Node.{}", "x".repeat(80)), 1_234_567);
        assert!(name.len() <= 63 && name.starts_with("kxs-shell-node-x"), "{name}");
        assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        let p = node_shell_pod("wrk01", "kxs-shell-wrk01-1", "busybox:1.36");
        assert_eq!(p["spec"]["nodeName"], "wrk01");
        assert_eq!(p["spec"]["tolerations"][0]["operator"], "Exists"); // runs on tainted control-plane nodes too
        assert_eq!(p["spec"]["containers"][0]["securityContext"]["privileged"], true);
        assert!(serde_json::from_value::<Pod>(p).is_ok());
    }

    #[test]
    fn manual_job_names_fit() {
        let n = manual_job_name(&"x".repeat(80), 1_234_567);
        assert!(n.len() <= 63, "{n}");
        assert!(n.ends_with("-manual-34567"));
        assert_eq!(manual_job_name("backup", 5), "backup-manual-5");
    }
}
