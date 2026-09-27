//! Object details (shown in an object sub-tab): per-kind summary of the full object with links
//! to the objects it references, related objects, events, an editor for Secret/ConfigMap data,
//! and the actions that apply to it.
use std::collections::HashSet;

use base64::Engine;
use egui::{Color32, RichText, Ui};
use kube::Client;
use serde_json::{json, Value};

use crate::find::{self, Find};
use crate::ops::{self, fmt_bytes, fmt_cpu, parse_qty, Ev, Kind, Metrics, Rel, Res};
use crate::ui_kit::{self, Btn, Icon};
use crate::watch::{self, cell_color, take, Pending, GREEN, ORANGE, RED};

#[derive(Clone)]
pub struct Target {
    pub kind: Kind,
    pub ns: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// Open the object in its own sub-tab.
    Details,
    /// Open another object by reference (links in details).
    Open { group: String, kind: String, ns: String, name: String },
    Edit,
    Delete,
    Logs(Option<String>),
    Shell(Option<String>),
    /// Shell on the node itself (temporary privileged pod).
    NodeShell,
    Restart,
    Scale(i64),
    Cordon(bool),
    Drain,
    Trigger,
    Suspend(bool),
    /// kubectl port-forward target (`pod/x`, `svc/x`) and remote port
    Forward(String, u16),
}

impl Act {
    pub fn label(&self) -> String {
        match self {
            Act::Details => "↗ Open in tab".into(),
            Act::Open { kind, name, .. } => format!("{kind} {name}"),
            Act::Edit => "✏ Edit YAML".into(),
            Act::Delete => "🗑 Delete".into(),
            Act::Logs(_) => "📜 Logs".into(),
            Act::Shell(_) | Act::NodeShell => "⌨ Terminal".into(),
            Act::Restart => "⟳ Restart".into(),
            Act::Scale(n) => format!("Scale to {n}"),
            Act::Cordon(true) => "⛔ Cordon".into(),
            Act::Cordon(false) => "✅ Uncordon".into(),
            Act::Drain => "⏏ Drain".into(),
            Act::Trigger => "▶ Trigger".into(),
            Act::Suspend(true) => "⏸ Suspend".into(),
            Act::Suspend(false) => "▶ Resume".into(),
            Act::Forward(t, p) => format!("↔ Forward {t}:{p}"),
        }
    }

    pub fn needs_confirm(&self) -> bool {
        matches!(self, Act::Delete | Act::Drain | Act::Restart | Act::NodeShell)
    }
}

fn is(kind: &Kind, group: &str, names: &[&str]) -> bool {
    kind.ar.group == group && names.contains(&kind.ar.kind.as_str())
}

/// Kinds whose logs are their pods' logs.
pub fn has_logs(kind: &Kind) -> bool {
    is(kind, "", &["Pod"]) || is(kind, "apps", &["Deployment", "StatefulSet", "DaemonSet", "ReplicaSet"]) || is(kind, "batch", &["Job"])
}

pub fn can_restart(kind: &Kind) -> bool {
    is(kind, "apps", &["Deployment", "StatefulSet", "DaemonSet"])
}

pub fn can_scale(kind: &Kind) -> bool {
    is(kind, "apps", &["Deployment", "StatefulSet", "ReplicaSet"])
}

/// Actions for a row's context menu (no full object needed).
pub fn row_actions(kind: &Kind) -> Vec<Act> {
    let mut v = vec![Act::Details, Act::Edit];
    if has_logs(kind) {
        v.push(Act::Logs(None));
    }
    if is(kind, "", &["Pod"]) {
        v.push(Act::Shell(None));
    }
    if can_restart(kind) {
        v.push(Act::Restart);
    }
    if is(kind, "batch", &["CronJob"]) {
        v.push(Act::Trigger);
    }
    if is(kind, "", &["Node"]) {
        v.extend([Act::NodeShell, Act::Cordon(true), Act::Cordon(false), Act::Drain]);
    }
    if kind.can("delete") {
        v.push(Act::Delete);
    }
    v
}

/// One key being edited; `orig` is `None` for a new key.
#[derive(Clone, Debug, PartialEq)]
struct KeyEdit {
    orig: Option<String>,
    key: String,
    value: String,
}

#[derive(Debug, PartialEq)]
enum Edit {
    Set(KeyEdit),
    Remove(String),
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s.trim()).ok()
}

/// Text of a data value (Secret values are base64); `None` when it isn't UTF-8 text.
pub fn decode_value(v: &str, secret: bool) -> Option<String> {
    if secret { b64_decode(v).and_then(|b| String::from_utf8(b).ok()) } else { Some(v.to_string()) }
}

/// `data` after one edit; every other key is kept byte for byte.
fn apply_edit(data: &Value, edit: &Edit, secret: bool) -> Result<Value, String> {
    let mut map = data.as_object().cloned().unwrap_or_default();
    match edit {
        Edit::Remove(k) => {
            map.remove(k);
        }
        Edit::Set(e) => {
            let key = e.key.trim();
            if key.is_empty() {
                return Err("empty key".into());
            }
            if e.orig.as_deref() != Some(key) && map.contains_key(key) {
                return Err(format!("key {key} already exists"));
            }
            if let Some(o) = &e.orig {
                map.remove(o);
            }
            let v = if secret { base64::engine::general_purpose::STANDARD.encode(&e.value) } else { e.value.clone() };
            map.insert(key.to_string(), Value::String(v));
        }
    }
    Ok(Value::Object(map))
}

/// Same event repeated (as separate Event objects) → one entry with the summed count.
fn group_events(evs: &[Ev]) -> Vec<Ev> {
    let mut out: Vec<Ev> = vec![];
    for e in evs {
        match out.iter_mut().find(|o| o.kind == e.kind && o.reason == e.reason && o.message == e.message) {
            Some(o) => o.count += e.count,
            None => out.push(e.clone()),
        }
    }
    out
}

/// What an object tab shows (the details panel shows everything at once).
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Section {
    Overview,
    Events,
    Yaml,
}

pub struct Details {
    pub t: Target,
    pub section: Section,
    rv: String,
    client: Client,
    obj: Option<Value>,
    error: Option<String>,
    load: Option<Pending<Res<Value>>>,
    events: Option<Res<Vec<Ev>>>,
    ev_load: Option<Pending<Res<Vec<Ev>>>>,
    related: Option<Res<Vec<Rel>>>,
    rel_load: Option<Pending<Res<Vec<Rel>>>>,
    reveal: HashSet<String>,
    replicas: Option<i64>,
    editing: Option<KeyEdit>,
    confirm_del: Option<String>,
    save: Option<Pending<Res>>,
    save_status: Option<Res>,
    gone: bool,
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn arr(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn kv(ui: &mut Ui, k: &str, v: impl Into<RichText>) {
    find::label(ui, RichText::new(k).color(ui_kit::tokens(ui).dim));
    find::wrapped(ui, v);
    ui.end_row();
}

fn kv_opt(ui: &mut Ui, k: &str, v: &Value) {
    let text = match v {
        Value::Null => return,
        Value::String(t) if t.is_empty() => return,
        Value::String(t) => t.clone(),
        other => other.to_string(),
    };
    kv(ui, k, text);
}

fn open(group: &str, kind: &str, ns: &str, name: &str) -> Act {
    Act::Open { group: group.into(), kind: kind.into(), ns: ns.into(), name: name.into() }
}

/// A clickable object name.
fn obj_link(ui: &mut Ui, acts: &mut Vec<Act>, group: &str, kind: &str, ns: &str, name: &str) {
    if !name.is_empty() && find::link(ui, name).on_hover_text(format!("Open {kind} {name}")).clicked() {
        acts.push(open(group, kind, ns, name));
    }
}

/// Grid row whose value links to another object.
fn kv_link(ui: &mut Ui, acts: &mut Vec<Act>, k: &str, group: &str, kind: &str, ns: &str, name: &str) {
    if name.is_empty() {
        return;
    }
    find::label(ui, RichText::new(k).color(ui_kit::tokens(ui).dim));
    obj_link(ui, acts, group, kind, ns, name);
    ui.end_row();
}

/// A label with a background: unlike a Frame it has a known size, so wrapped rows wrap.
fn chip(ui: &mut Ui, text: &str, color: Option<Color32>) -> egui::Response {
    let t = RichText::new(format!(" {text} ")).small().background_color(ui.visuals().widgets.inactive.weak_bg_fill);
    find::label(ui, match color {
        Some(c) => t.color(c),
        None => t,
    })
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(14.0);
    let dim = ui_kit::tokens(ui).dim;
    find::label(ui, RichText::new(title.to_uppercase()).font(ui_kit::semibold(11.5)).color(dim).extra_letter_spacing(0.8));
    ui.add_space(2.0);
}

fn grid(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([16.0, 7.0]).min_col_width(110.0).show(ui, add);
}

/// Short status for the header.
fn status_of(kind: &Kind, obj: &Value) -> Option<String> {
    let st = &obj["status"];
    if obj["metadata"]["deletionTimestamp"].is_string() {
        return Some("Terminating".into());
    }
    let text = match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("", "Pod") => {
            let cs = || arr(&st["containerStatuses"]);
            let waiting = cs().find_map(|c| c["state"]["waiting"]["reason"].as_str());
            let failed = cs().find_map(|c| c["state"]["terminated"]["reason"].as_str()).filter(|r| *r != "Completed");
            waiting.or(failed).unwrap_or(s(&st["phase"])).to_string()
        }
        ("", "Node") => arr(&st["conditions"]).find(|c| s(&c["type"]) == "Ready").map_or("", |c| if s(&c["status"]) == "True" { "Ready" } else { "NotReady" }).to_string(),
        ("apps", "Deployment" | "StatefulSet" | "ReplicaSet") => {
            let (want, ready) = (obj["spec"]["replicas"].as_i64().unwrap_or(0), st["readyReplicas"].as_i64().unwrap_or(0));
            if ready >= want { "Ready" } else { "Not ready" }.to_string()
        }
        _ => s(&st["phase"]).to_string(),
    };
    (!text.is_empty()).then_some(text)
}

/// An action as a button: icons for the common ones, Delete in red.
fn act_button(ui: &mut Ui, a: &Act) -> egui::Response {
    let label = a.label();
    // Labels start with a glyph ("📜 Logs"); the painted icons replace them.
    let bare = label.split_once(' ').filter(|(g, _)| !g.chars().any(char::is_alphanumeric)).map_or(label.as_str(), |(_, rest)| rest);
    match a {
        Act::Edit => ui_kit::button(ui, Btn::Normal, Some(Icon::Edit), "Edit YAML"),
        Act::Logs(_) => ui_kit::button(ui, Btn::Normal, Some(Icon::Doc), "Logs"),
        Act::Shell(_) | Act::NodeShell => ui_kit::button(ui, Btn::Normal, Some(Icon::Terminal), "Terminal"),
        Act::Delete => ui_kit::button(ui, Btn::Danger, Some(Icon::Trash), "Delete"),
        _ => ui_kit::button(ui, Btn::Normal, None, bare),
    }
}

fn conditions(ui: &mut Ui, v: &Value) {
    if v.as_array().is_none_or(|a| a.is_empty()) {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Conditions").weak());
        for c in arr(v) {
            let ok = s(&c["status"]) == "True";
            // "Pressure"-type node conditions are healthy when False.
            let bad_when_true = s(&c["type"]).ends_with("Pressure") || s(&c["type"]) == "NetworkUnavailable";
            let color = if ok != bad_when_true { GREEN } else if ok { RED } else { ORANGE };
            let tip = format!("{} {}", s(&c["reason"]), s(&c["message"]));
            let r = chip(ui, s(&c["type"]), Some(color));
            if !tip.trim().is_empty() {
                r.on_hover_text(tip);
            }
        }
    });
}

/// Objects a pod spec points at: (group, kind, name), sorted and deduplicated.
fn pod_refs(spec: &Value) -> Vec<(&'static str, &'static str, String)> {
    let mut v = vec![];
    let mut add = |g: &'static str, k: &'static str, n: &Value| {
        if let Some(n) = n.as_str().filter(|n| !n.is_empty()) {
            v.push((g, k, n.to_string()));
        }
    };
    for vol in arr(&spec["volumes"]) {
        add("", "ConfigMap", &vol["configMap"]["name"]);
        add("", "Secret", &vol["secret"]["secretName"]);
        add("", "PersistentVolumeClaim", &vol["persistentVolumeClaim"]["claimName"]);
        for src in arr(&vol["projected"]["sources"]) {
            add("", "ConfigMap", &src["configMap"]["name"]);
            add("", "Secret", &src["secret"]["name"]);
        }
    }
    for c in arr(&spec["containers"]).chain(arr(&spec["initContainers"])) {
        for e in arr(&c["envFrom"]) {
            add("", "ConfigMap", &e["configMapRef"]["name"]);
            add("", "Secret", &e["secretRef"]["name"]);
        }
        for e in arr(&c["env"]) {
            add("", "ConfigMap", &e["valueFrom"]["configMapKeyRef"]["name"]);
            add("", "Secret", &e["valueFrom"]["secretKeyRef"]["name"]);
        }
    }
    for p in arr(&spec["imagePullSecrets"]) {
        add("", "Secret", &p["name"]);
    }
    v.sort();
    v.dedup();
    v
}

fn refs_section(ui: &mut Ui, acts: &mut Vec<Act>, pod_spec: &Value, ns: &str) {
    let refs = pod_refs(pod_spec);
    if refs.is_empty() {
        return;
    }
    section(ui, "References");
    for kind in ["ConfigMap", "Secret", "PersistentVolumeClaim"] {
        let names: Vec<&String> = refs.iter().filter(|r| r.1 == kind).map(|r| &r.2).collect();
        if names.is_empty() {
            continue;
        }
        ui.horizontal_wrapped(|ui| {
            find::label(ui, RichText::new(format!("{kind}s")).weak());
            for n in names {
                obj_link(ui, acts, "", kind, ns, n);
            }
        });
    }
}

impl Details {
    pub fn new(ctx: &egui::Context, client: Client, kind: Kind, ns: String, name: String) -> Self {
        let mut d = Details {
            t: Target { kind, ns, name },
            section: Section::Overview,
            rv: String::new(),
            client,
            obj: None,
            error: None,
            load: None,
            events: None,
            ev_load: None,
            related: None,
            rel_load: None,
            reveal: HashSet::new(),
            replicas: None,
            editing: None,
            confirm_del: None,
            save: None,
            save_status: None,
            gone: false,
        };
        d.reload(ctx);
        d
    }

    fn reload(&mut self, ctx: &egui::Context) {
        let t = &self.t;
        self.load = Some(Pending::spawn(ctx, ops::get(self.client.clone(), t.kind.ar.clone(), t.ns.clone(), t.name.clone())));
    }

    /// Reload when the live object changed; `None` = it is gone.
    pub fn sync(&mut self, ctx: &egui::Context, rv: Option<&str>) {
        match rv {
            None => self.gone = true,
            Some(rv) if rv != self.rv && self.load.is_none() => {
                // The first version seen is the one `new` just fetched; later ones mean a change.
                let seen = !self.rv.is_empty();
                self.rv = rv.to_string();
                self.gone = false;
                if seen {
                    self.reload(ctx);
                }
            }
            _ => {}
        }
    }

    fn bar_actions(&self, obj: &Value) -> Vec<Act> {
        let k = &self.t.kind;
        let mut v = vec![Act::Edit];
        if has_logs(k) {
            v.push(Act::Logs(None));
        }
        if is(k, "", &["Pod"]) {
            v.push(Act::Shell(None));
        }
        if can_restart(k) {
            v.push(Act::Restart);
        }
        if is(k, "batch", &["CronJob"]) {
            v.push(Act::Trigger);
            v.push(Act::Suspend(!obj["spec"]["suspend"].as_bool().unwrap_or(false)));
        }
        if is(k, "", &["Node"]) {
            v.push(Act::NodeShell);
            v.push(Act::Cordon(!obj["spec"]["unschedulable"].as_bool().unwrap_or(false)));
            v.push(Act::Drain);
        }
        if k.can("delete") {
            v.push(Act::Delete);
        }
        v
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(r) = take(&mut self.load) {
            match r {
                Ok(o) => {
                    self.replicas = o["spec"]["replicas"].as_i64();
                    let uid = s(&o["metadata"]["uid"]).to_string();
                    self.ev_load = Some(Pending::spawn(ctx, ops::events_for(self.client.clone(), self.t.ns.clone(), uid)));
                    self.rel_load = Some(Pending::spawn(ctx, ops::related(self.client.clone(), self.t.kind.clone(), o.clone())));
                    self.obj = Some(o);
                    self.error = None;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(r) = take(&mut self.ev_load) {
            self.events = Some(r.map(|e| group_events(&e)));
        }
        if let Some(r) = take(&mut self.rel_load) {
            self.related = Some(r);
        }
        if let Some(r) = take(&mut self.save) {
            if r.is_ok() {
                self.editing = None;
            }
            self.save_status = Some(r);
        }
    }

    /// `full`: an object tab (big header, Overview / Events / YAML); else the compact details panel.
    pub fn ui(&mut self, ui: &mut Ui, metrics: &Metrics, acts: &mut Vec<Act>, find: &mut Find, full: bool) {
        self.poll(ui.ctx());
        let t = ui_kit::tokens(ui);
        let status = self.obj.as_ref().and_then(|o| status_of(&self.t.kind, o));
        let meta: Vec<String> = [
            (!self.t.ns.is_empty()).then(|| self.t.ns.clone()),
            self.obj.as_ref().and_then(|o| o["metadata"]["creationTimestamp"].as_str()).map(|c| format!("created {} ago", watch::date_cell(c))),
            self.obj.as_ref().and_then(|o| o["spec"]["nodeName"].as_str()).map(|n| format!("on {n}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        let actions = self.obj.as_ref().map(|o| self.bar_actions(o)).unwrap_or_default();
        let status_line = |ui: &mut Ui| {
            if let Some(st) = &status {
                ui_kit::pill(ui, st, cell_color("Status", st).unwrap_or(t.muted));
            }
            ui.label(RichText::new(meta.join(" · ")).color(t.muted));
        };
        if full {
            ui.horizontal(|ui| {
                ui.label(RichText::new(crate::cluster::label(&self.t.kind)).color(t.muted));
                if !self.t.ns.is_empty() {
                    ui.label(RichText::new("/").color(t.dim));
                    ui.label(RichText::new(&self.t.ns).color(t.muted));
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.t.name).font(ui_kit::mono(20.0)).color(t.text));
                ui.add_space(4.0);
                status_line(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for a in actions.iter().rev() {
                        if act_button(ui, a).clicked() {
                            acts.push(a.clone());
                        }
                    }
                });
            });
            ui.add_space(8.0);
            let n_events = match &self.events {
                Some(Ok(e)) => format!("Events ({})", e.len()),
                _ => "Events".into(),
            };
            ui.horizontal(|ui| {
                ui.set_height(34.0);
                for (sec, name) in [(Section::Overview, "Overview".to_string()), (Section::Events, n_events), (Section::Yaml, "YAML".into())] {
                    if ui_kit::tab(ui, &name, self.section == sec, false, false).clicked() {
                        self.section = sec;
                    }
                    ui.add_space(22.0);
                }
            });
            ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), egui::Stroke::new(1.0, t.line));
            ui.add_space(4.0);
        } else {
            ui.add(egui::Label::new(RichText::new(&self.t.name).font(ui_kit::mono(15.0)).color(t.text)).truncate());
            ui.horizontal_wrapped(|ui| status_line(ui));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for a in &actions {
                    if act_button(ui, a).clicked() {
                        acts.push(a.clone());
                    }
                }
            });
            ui.add_space(2.0);
        }
        if self.gone {
            ui.colored_label(RED, "This object no longer exists.");
        }

        find::begin(find);
        // Both directions: something wider than the panel scrolls instead of widening it (a
        // side panel wider than the window disappears).
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            match (&self.obj, &self.error) {
                (_, Some(e)) => {
                    ui.colored_label(RED, e);
                }
                (None, None) => {
                    ui.spinner();
                }
                _ => {}
            }
            if let Some(obj) = self.obj.clone() {
                let yaml = |ui: &mut Ui| {
                    let theme = egui_extras::syntax_highlighting::CodeTheme::from_memory(ui.ctx(), ui.style());
                    let mut job = egui_extras::syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, &ops::to_yaml(&obj), "yaml");
                    // Wrapped: certificates and last-applied annotations are single lines thousands of pixels wide.
                    job.wrap = egui::text::TextWrapping { max_width: ui.available_width(), ..Default::default() };
                    find::job(ui, job);
                };
                match (full, self.section) {
                    (true, Section::Events) => self.events_ui(ui),
                    (true, Section::Yaml) => yaml(ui),
                    (true, Section::Overview) => {
                        self.body(ui, &obj, metrics, acts);
                        self.related_ui(ui, acts);
                    }
                    (false, _) => {
                        self.body(ui, &obj, metrics, acts);
                        self.related_ui(ui, acts);
                        self.events_ui(ui);
                        let hit = find::matches(|| ops::to_yaml(&obj));
                        egui::CollapsingHeader::new("YAML").id_salt("yaml").open(hit.then_some(true)).show(ui, yaml);
                    }
                }
            }
        });
        find::end(find);
    }

    fn body(&mut self, ui: &mut Ui, obj: &Value, m: &Metrics, acts: &mut Vec<Act>) {
        let meta = &obj["metadata"];
        let ns = s(&meta["namespace"]).to_string();
        grid(ui, "meta", |ui| {
            let created = meta["creationTimestamp"].as_str().unwrap_or_default();
            kv(ui, "Created", format!("{} ago ({created})", watch::date_cell(created)));
            kv(ui, "Name", s(&meta["name"]));
            if !ns.is_empty() {
                kv_link(ui, acts, "Namespace", "", "Namespace", "", &ns);
            }
            kv(ui, "UID", s(&meta["uid"]));
            for o in arr(&meta["ownerReferences"]) {
                find::label(ui, RichText::new("Controlled by").weak());
                ui.horizontal(|ui| {
                    find::label(ui, s(&o["kind"]));
                    obj_link(ui, acts, ops::api_group(s(&o["apiVersion"])), s(&o["kind"]), &ns, s(&o["name"]));
                });
                ui.end_row();
            }
            if meta["finalizers"].is_array() {
                kv(ui, "Finalizers", arr(&meta["finalizers"]).map(s).collect::<Vec<_>>().join(", "));
            }
        });
        for (title, field) in [("Labels", "labels"), ("Annotations", "annotations")] {
            let Some(map) = meta[field].as_object().filter(|m| !m.is_empty()) else { continue };
            let hit = find::matches(|| map.iter().map(|(k, v)| format!("{k}={}", s(v)).chars().take(90).chain(['\n']).collect::<String>()).collect());
            egui::CollapsingHeader::new(format!("{title} ({})", map.len())).id_salt(field).default_open(field == "labels").open(hit.then_some(true)).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (k, v) in map {
                        let text = format!("{k}={}", s(v));
                        let short: String = text.chars().take(90).collect();
                        let r = chip(ui, &short, None);
                        if short.len() < text.len() {
                            r.on_hover_text(text);
                        }
                    }
                });
            });
        }

        let (spec, status) = (&obj["spec"], &obj["status"]);
        let k = self.t.kind.clone();
        match (k.ar.group.as_str(), k.ar.kind.as_str()) {
            ("", "Pod") => self.pod(ui, obj, m, acts),
            ("apps", "Deployment" | "StatefulSet" | "ReplicaSet") => {
                section(ui, "Replicas");
                grid(ui, "replicas", |ui| {
                    kv(ui, "Desired", spec["replicas"].to_string());
                    for (label, f) in [("Ready", "readyReplicas"), ("Available", "availableReplicas"), ("Updated", "updatedReplicas"), ("Current", "currentReplicas")] {
                        kv_opt(ui, label, &status[f]);
                    }
                    kv_opt(ui, "Strategy", &spec["strategy"]["type"]);
                    kv_opt(ui, "Strategy", &spec["updateStrategy"]["type"]);
                    kv(ui, "Selector", selector(&spec["selector"]));
                    kv(ui, "Images", images(&spec["template"]));
                    kv_link(ui, acts, "Service account", "", "ServiceAccount", &ns, s(&spec["template"]["spec"]["serviceAccountName"]));
                });
                if k.can("patch") {
                    ui.horizontal(|ui| {
                        let n = self.replicas.get_or_insert(0);
                        ui.label("Scale to");
                        ui.add(egui::DragValue::new(n).range(0..=1000));
                        if ui.button("Apply").clicked() {
                            acts.push(Act::Scale(*n));
                        }
                    });
                }
                conditions(ui, &status["conditions"]);
                refs_section(ui, acts, &spec["template"]["spec"], &ns);
            }
            ("apps", "DaemonSet") => {
                section(ui, "Rollout");
                grid(ui, "ds", |ui| {
                    for (label, f) in [("Desired", "desiredNumberScheduled"), ("Current", "currentNumberScheduled"), ("Ready", "numberReady"), ("Up to date", "updatedNumberScheduled"), ("Available", "numberAvailable"), ("Misscheduled", "numberMisscheduled")] {
                        kv_opt(ui, label, &status[f]);
                    }
                    kv(ui, "Selector", selector(&spec["selector"]));
                    kv(ui, "Images", images(&spec["template"]));
                });
                refs_section(ui, acts, &spec["template"]["spec"], &ns);
            }
            ("batch", "Job") => {
                section(ui, "Job");
                grid(ui, "job", |ui| {
                    for (label, v) in [("Completions", &spec["completions"]), ("Parallelism", &spec["parallelism"]), ("Active", &status["active"]), ("Succeeded", &status["succeeded"]), ("Failed", &status["failed"]), ("Started", &status["startTime"]), ("Completed", &status["completionTime"])] {
                        kv_opt(ui, label, v);
                    }
                    kv(ui, "Images", images(&spec["template"]));
                });
                conditions(ui, &status["conditions"]);
            }
            ("batch", "CronJob") => {
                section(ui, "Schedule");
                grid(ui, "cron", |ui| {
                    kv(ui, "Schedule", s(&spec["schedule"]));
                    kv_opt(ui, "Time zone", &spec["timeZone"]);
                    kv(ui, "Suspended", spec["suspend"].as_bool().unwrap_or(false).to_string());
                    kv(ui, "Active jobs", arr(&status["active"]).count().to_string());
                    kv_opt(ui, "Last schedule", &status["lastScheduleTime"]);
                    kv_opt(ui, "Last success", &status["lastSuccessfulTime"]);
                    kv(ui, "Images", images(&spec["jobTemplate"]["spec"]["template"]));
                });
            }
            ("", "Service") => {
                section(ui, "Service");
                grid(ui, "svc", |ui| {
                    kv(ui, "Type", s(&spec["type"]));
                    kv(ui, "Cluster IPs", arr(&spec["clusterIPs"]).map(s).collect::<Vec<_>>().join(", "));
                    kv_opt(ui, "External name", &spec["externalName"]);
                    let lb: Vec<&str> = arr(&status["loadBalancer"]["ingress"]).map(|i| i["ip"].as_str().or(i["hostname"].as_str()).unwrap_or("")).collect();
                    if !lb.is_empty() {
                        kv(ui, "Load balancer", lb.join(", "));
                    }
                    kv_opt(ui, "Session affinity", &spec["sessionAffinity"]);
                    kv(ui, "Selector", selector(&json!({"matchLabels": spec["selector"]})));
                    kv_link(ui, acts, "Endpoints", "", "Endpoints", &ns, s(&meta["name"]));
                });
                section(ui, "Ports");
                for p in arr(&spec["ports"]) {
                    ui.horizontal(|ui| {
                        let port = p["port"].as_u64().unwrap_or(0) as u16;
                        find::label(ui, RichText::new(format!("{} {}/{} → {}{}", s(&p["name"]), port, s(&p["protocol"]), p["targetPort"], p["nodePort"].as_u64().map(|n| format!(" (node {n})")).unwrap_or_default())).monospace());
                        if ui.small_button("↔ Forward").clicked() {
                            acts.push(Act::Forward(format!("svc/{}", s(&meta["name"])), port));
                        }
                    });
                }
            }
            ("networking.k8s.io", "Ingress") => {
                section(ui, "Rules");
                grid(ui, "ing", |ui| {
                    kv_link(ui, acts, "Class", "networking.k8s.io", "IngressClass", "", s(&spec["ingressClassName"]));
                    for t in arr(&spec["tls"]) {
                        kv_link(ui, acts, "TLS secret", "", "Secret", &ns, s(&t["secretName"]));
                    }
                });
                let tls: HashSet<&str> = arr(&spec["tls"]).flat_map(|t| arr(&t["hosts"]).map(s)).collect();
                for r in arr(&spec["rules"]) {
                    let host = r["host"].as_str().unwrap_or("*");
                    for p in arr(&r["http"]["paths"]) {
                        let svc = &p["backend"]["service"];
                        let port = svc["port"]["number"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| s(&svc["port"]["name"]).into());
                        let url = format!("{}://{host}{}", if tls.contains(host) { "https" } else { "http" }, s(&p["path"]));
                        ui.horizontal(|ui| {
                            ui.hyperlink_to(&url, &url);
                            ui.label("→");
                            obj_link(ui, acts, "", "Service", &ns, s(&svc["name"]));
                            find::label(ui, format!(":{port}"));
                        });
                    }
                }
            }
            ("", "Node") => self.node(ui, obj, m),
            ("", "ConfigMap") => self.data_ui(ui, obj, false),
            ("", "Secret") => self.data_ui(ui, obj, true),
            ("", "PersistentVolumeClaim" | "PersistentVolume") => {
                section(ui, "Storage");
                grid(ui, "pv", |ui| {
                    kv_opt(ui, "Status", &status["phase"]);
                    kv_opt(ui, "Capacity", &status["capacity"]["storage"]);
                    kv_opt(ui, "Capacity", &spec["capacity"]["storage"]);
                    kv_opt(ui, "Requested", &spec["resources"]["requests"]["storage"]);
                    kv(ui, "Access modes", arr(&spec["accessModes"]).map(s).collect::<Vec<_>>().join(", "));
                    kv_link(ui, acts, "Storage class", "storage.k8s.io", "StorageClass", "", s(&spec["storageClassName"]));
                    kv_link(ui, acts, "Volume", "", "PersistentVolume", "", s(&spec["volumeName"]));
                    kv_opt(ui, "Reclaim policy", &spec["persistentVolumeReclaimPolicy"]);
                    let claim = &spec["claimRef"];
                    kv_link(ui, acts, "Claim", "", "PersistentVolumeClaim", s(&claim["namespace"]), s(&claim["name"]));
                });
            }
            ("autoscaling", "HorizontalPodAutoscaler") => {
                section(ui, "Autoscaling");
                let t = &spec["scaleTargetRef"];
                grid(ui, "hpa", |ui| {
                    kv_link(ui, acts, &format!("Target {}", s(&t["kind"])), ops::api_group(s(&t["apiVersion"])), s(&t["kind"]), &ns, s(&t["name"]));
                    kv_opt(ui, "Min replicas", &spec["minReplicas"]);
                    kv_opt(ui, "Max replicas", &spec["maxReplicas"]);
                    kv_opt(ui, "Current", &status["currentReplicas"]);
                    kv_opt(ui, "Desired", &status["desiredReplicas"]);
                });
                conditions(ui, &status["conditions"]);
            }
            ("rbac.authorization.k8s.io", "RoleBinding" | "ClusterRoleBinding") => {
                section(ui, "Binding");
                let r = &obj["roleRef"];
                let role_ns = if s(&r["kind"]) == "Role" { ns.as_str() } else { "" };
                grid(ui, "rb", |ui| {
                    kv_link(ui, acts, s(&r["kind"]), "rbac.authorization.k8s.io", s(&r["kind"]), role_ns, s(&r["name"]));
                    for sub in arr(&obj["subjects"]) {
                        if s(&sub["kind"]) == "ServiceAccount" {
                            let sns = sub["namespace"].as_str().unwrap_or(&ns);
                            kv_link(ui, acts, "ServiceAccount", "", "ServiceAccount", sns, s(&sub["name"]));
                        } else {
                            kv(ui, s(&sub["kind"]), s(&sub["name"]));
                        }
                    }
                });
            }
            ("", "Endpoints") => {
                section(ui, "Addresses");
                for sub in arr(&obj["subsets"]) {
                    let ports = arr(&sub["ports"]).map(|p| format!("{}/{}", p["port"], s(&p["protocol"]))).collect::<Vec<_>>().join(", ");
                    for a in arr(&sub["addresses"]).chain(arr(&sub["notReadyAddresses"])) {
                        ui.horizontal(|ui| {
                            find::label(ui, RichText::new(format!("{} {ports}", s(&a["ip"]))).monospace());
                            let tr = &a["targetRef"];
                            obj_link(ui, acts, "", s(&tr["kind"]), s(&tr["namespace"]), s(&tr["name"]));
                        });
                    }
                }
            }
            ("", "Event") | ("events.k8s.io", "Event") => {
                section(ui, "Event");
                let io = if obj["involvedObject"].is_object() { &obj["involvedObject"] } else { &obj["regarding"] };
                grid(ui, "ev", |ui| {
                    kv_link(ui, acts, &format!("Object ({})", s(&io["kind"])), ops::api_group(s(&io["apiVersion"])), s(&io["kind"]), s(&io["namespace"]), s(&io["name"]));
                    kv_opt(ui, "Type", &obj["type"]);
                    kv_opt(ui, "Reason", &obj["reason"]);
                    kv_opt(ui, "Message", if obj["message"].is_null() { &obj["note"] } else { &obj["message"] });
                    kv_opt(ui, "Count", &obj["count"]);
                    kv_opt(ui, "Source", &obj["source"]["component"]);
                    kv_opt(ui, "Last seen", &obj["lastTimestamp"]);
                });
            }
            _ => conditions(ui, &status["conditions"]),
        }
    }

    /// Secret / ConfigMap data: each key can be shown, copied, edited or removed on its own.
    fn data_ui(&mut self, ui: &mut Ui, obj: &Value, secret: bool) {
        let can_edit = self.t.kind.can("update");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            find::label(ui, RichText::new("Data").strong().size(15.0));
            if secret {
                find::label(ui, RichText::new(format!("({})", s(&obj["type"]))).weak());
            }
            if self.save.is_some() {
                ui.spinner();
            }
            match &self.save_status {
                Some(Ok(m)) => ui.colored_label(GREEN, m),
                Some(Err(e)) => ui.colored_label(RED, e),
                None => ui.label(""),
            };
        });
        ui.separator();

        let data = obj["data"].clone();
        let mut edit = None;
        for (key, v) in data.as_object().into_iter().flatten() {
            ui.push_id(key, |ui| {
                if self.editing.as_ref().is_some_and(|e| e.orig.as_deref() == Some(key)) {
                    edit = self.key_editor(ui).or(edit.take());
                    return;
                }
                let text = decode_value(s(v), secret);
                ui.horizontal(|ui| {
                    find::label(ui, RichText::new(key).strong());
                    let shown = !secret || self.reveal.contains(key);
                    if secret && text.is_some() && ui.small_button(if shown { "Hide" } else { "👁 Show" }).clicked() {
                        if shown { self.reveal.remove(key) } else { self.reveal.insert(key.clone()) };
                    }
                    if let Some(t) = &text {
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(t.clone());
                        }
                        if can_edit && self.editing.is_none() && ui.small_button("✏ Edit").on_hover_text("Edit this key").clicked() {
                            self.editing = Some(KeyEdit { orig: Some(key.clone()), key: key.clone(), value: t.clone() });
                            self.save_status = None;
                        }
                    }
                    if can_edit && ui.small_button("🗑").on_hover_text("Remove this key").clicked() {
                        self.confirm_del = Some(key.clone());
                    }
                });
                if self.confirm_del.as_deref() == Some(key) {
                    ui.horizontal(|ui| {
                        ui.colored_label(RED, format!("Remove key {key}?"));
                        if ui.button(RichText::new("Remove").color(RED)).clicked() {
                            edit = Some(Edit::Remove(key.clone()));
                            self.confirm_del = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_del = None;
                        }
                    });
                }
                match (&text, !secret || self.reveal.contains(key)) {
                    (None, _) => ui.label(RichText::new(format!("<binary, {} bytes>", b64_decode(s(v)).map_or(0, |b| b.len()))).weak()),
                    (Some(t), true) => find::wrapped(ui, RichText::new(t).monospace()),
                    (Some(t), false) => find::masked(ui, t),
                };
                ui.add_space(4.0);
            });
        }
        if self.editing.as_ref().is_some_and(|e| e.orig.is_none()) {
            edit = self.key_editor(ui).or(edit);
        } else if can_edit && self.editing.is_none() && ui.button("➕ Add key").clicked() {
            self.editing = Some(KeyEdit { orig: None, key: String::new(), value: String::new() });
            self.save_status = None;
        }
        if let Some(e) = edit {
            match apply_edit(&data, &e, secret) {
                Ok(new_data) => {
                    let mut new = obj.clone();
                    new["data"] = new_data;
                    if let Some(o) = new.as_object_mut() {
                        o.remove("stringData");
                        if let Some(m) = o.get_mut("metadata").and_then(Value::as_object_mut) {
                            m.remove("managedFields");
                        }
                    }
                    let t = &self.t;
                    self.save = Some(Pending::spawn(ui.ctx(), ops::replace(self.client.clone(), t.kind.ar.clone(), t.ns.clone(), t.name.clone(), new)));
                }
                Err(err) => self.save_status = Some(Err(err)),
            }
        }
    }

    /// Key + value editors with Save / Cancel; `Some(edit)` when Save is pressed.
    fn key_editor(&mut self, ui: &mut Ui) -> Option<Edit> {
        let (mut save, mut cancel) = (false, false);
        let busy = self.save.is_some();
        let e = self.editing.as_mut()?;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.add(egui::TextEdit::singleline(&mut e.key).hint_text("key").desired_width(260.0));
            let rows = e.value.lines().count().clamp(3, 16);
            ui.add(egui::TextEdit::multiline(&mut e.value).hint_text("value").code_editor().desired_rows(rows).desired_width(ui.available_width()));
            ui.horizontal(|ui| {
                save = ui.add_enabled(!busy, egui::Button::new(RichText::new("💾 Save").strong())).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if cancel {
            self.editing = None;
        }
        if save { self.editing.clone().map(Edit::Set) } else { None }
    }

    fn pod(&mut self, ui: &mut Ui, obj: &Value, m: &Metrics, acts: &mut Vec<Act>) {
        let (meta, spec, status) = (&obj["metadata"], &obj["spec"], &obj["status"]);
        let ns = s(&meta["namespace"]).to_string();
        section(ui, "Pod");
        grid(ui, "pod", |ui| {
            let phase = format!("{} {}", s(&status["phase"]), s(&status["reason"]));
            find::label(ui, RichText::new("Status").weak());
            find::label(ui, RichText::new(&phase).color(cell_color("Status", &phase).unwrap_or(ui.visuals().text_color())));
            ui.end_row();
            kv_link(ui, acts, "Node", "", "Node", "", s(&spec["nodeName"]));
            kv(ui, "Pod IPs", arr(&status["podIPs"]).map(|i| s(&i["ip"])).collect::<Vec<_>>().join(", "));
            kv_link(ui, acts, "Service account", "", "ServiceAccount", &ns, s(&spec["serviceAccountName"]));
            kv_opt(ui, "QoS class", &status["qosClass"]);
            kv_link(ui, acts, "Priority class", "scheduling.k8s.io", "PriorityClass", "", s(&spec["priorityClassName"]));
            if let Some((cpu, mem)) = m.pods.get(&(ns.clone(), s(&meta["name"]).into())) {
                kv(ui, "Usage", format!("CPU {} · Memory {}", fmt_cpu(*cpu), fmt_bytes(*mem)));
            }
        });
        conditions(ui, &status["conditions"]);
        refs_section(ui, acts, spec, &ns);

        let statuses: Vec<&Value> = arr(&status["containerStatuses"]).chain(arr(&status["initContainerStatuses"])).collect();
        let pod_name = s(&meta["name"]).to_string();
        for (init, c) in arr(&spec["initContainers"]).map(|c| (true, c)).chain(arr(&spec["containers"]).map(|c| (false, c))) {
            let name = s(&c["name"]);
            let st = statuses.iter().find(|x| s(&x["name"]) == name);
            section(ui, &format!("{}{name}", if init { "Init: " } else { "Container: " }));
            grid(ui, &format!("c-{name}"), |ui| {
                if let Some(st) = st {
                    let (state, detail) = match st["state"].as_object().and_then(|o| o.iter().next()) {
                        Some((k, v)) => (k.clone(), format!("{} {}", s(&v["reason"]), v["exitCode"].as_i64().map(|c| format!("exit {c}")).unwrap_or_default())),
                        None => ("unknown".into(), String::new()),
                    };
                    let color = match state.as_str() {
                        "running" => GREEN,
                        "waiting" => ORANGE,
                        _ if detail.contains("Completed") => GREEN,
                        _ => RED,
                    };
                    find::label(ui, RichText::new("State").weak());
                    find::label(ui, RichText::new(format!("{state} {detail}")).color(color));
                    ui.end_row();
                    kv(ui, "Ready", st["ready"].to_string());
                    kv(ui, "Restarts", st["restartCount"].to_string());
                    if let Some((_, last)) = st["lastState"].as_object().and_then(|o| o.iter().next()) {
                        kv(ui, "Last state", format!("{} {} {}", s(&last["reason"]), last["exitCode"], s(&last["finishedAt"])));
                    }
                }
                kv(ui, "Image", s(&c["image"]));
                for f in ["command", "args"] {
                    if c[f].is_array() {
                        kv(ui, f, arr(&c[f]).map(s).collect::<Vec<_>>().join(" "));
                    }
                }
                let res = &c["resources"];
                for (label, part) in [("Requests", "requests"), ("Limits", "limits")] {
                    if let Some(o) = res[part].as_object() {
                        kv(ui, label, o.iter().map(|(k, v)| format!("{k}: {}", s(v))).collect::<Vec<_>>().join(", "));
                    }
                }
                if let Some(env) = c["env"].as_array() {
                    kv(ui, "Env vars", env.len().to_string());
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("📜 Logs").clicked() {
                    acts.push(Act::Logs(Some(name.into())));
                }
                if !init && ui.small_button("⌨ Terminal").clicked() {
                    acts.push(Act::Shell(Some(name.into())));
                }
                for p in arr(&c["ports"]) {
                    let port = p["containerPort"].as_u64().unwrap_or(0) as u16;
                    if ui.small_button(format!("↔ {port}/{}", s(&p["protocol"]))).on_hover_text("Port forward").clicked() {
                        acts.push(Act::Forward(format!("pod/{pod_name}"), port));
                    }
                }
            });
        }
    }

    fn node(&mut self, ui: &mut Ui, obj: &Value, m: &Metrics) {
        let (meta, spec, status) = (&obj["metadata"], &obj["spec"], &obj["status"]);
        let name = s(&meta["name"]);
        conditions(ui, &status["conditions"]);
        section(ui, "Node");
        grid(ui, "node", |ui| {
            let roles: Vec<&str> = meta["labels"].as_object().into_iter().flatten().filter_map(|(k, _)| k.strip_prefix("node-role.kubernetes.io/")).collect();
            kv(ui, "Roles", if roles.is_empty() { "<none>".into() } else { roles.join(", ") });
            kv(ui, "Schedulable", (!spec["unschedulable"].as_bool().unwrap_or(false)).to_string());
            kv(ui, "Addresses", arr(&status["addresses"]).map(|a| format!("{}: {}", s(&a["type"]), s(&a["address"]))).collect::<Vec<_>>().join(", "));
            let info = &status["nodeInfo"];
            for (label, f) in [("OS", "osImage"), ("Kernel", "kernelVersion"), ("Runtime", "containerRuntimeVersion"), ("Kubelet", "kubeletVersion"), ("Arch", "architecture")] {
                kv_opt(ui, label, &info[f]);
            }
            let taints: Vec<String> = arr(&spec["taints"]).map(|t| format!("{}={}:{}", s(&t["key"]), s(&t["value"]), s(&t["effect"]))).collect();
            if !taints.is_empty() {
                kv(ui, "Taints", taints.join(", "));
            }
        });
        section(ui, "Resources");
        let (cap, alloc) = (&status["capacity"], &status["allocatable"]);
        let usage = m.nodes.get(name);
        egui::Grid::new("node-res").num_columns(4).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
            for h in ["", "Capacity", "Allocatable", "In use"] {
                ui.label(RichText::new(h).weak());
            }
            ui.end_row();
            for key in ["cpu", "memory", "pods", "ephemeral-storage"] {
                let fmt = |v: &Value| match key {
                    "memory" | "ephemeral-storage" => fmt_bytes(parse_qty(s(v))),
                    _ => s(v).to_string(),
                };
                ui.label(key);
                ui.label(fmt(&cap[key]));
                ui.label(fmt(&alloc[key]));
                let used = match (key, usage) {
                    ("cpu", Some(u)) => format!("{} ({:.0}%)", fmt_cpu(u.0), 100.0 * u.0 / parse_qty(s(&alloc["cpu"])).max(1e-9)),
                    ("memory", Some(u)) => format!("{} ({:.0}%)", fmt_bytes(u.1), 100.0 * u.1 / parse_qty(s(&alloc["memory"])).max(1.0)),
                    _ => String::new(),
                };
                ui.label(used);
                ui.end_row();
            }
        });
    }

    fn related_ui(&self, ui: &mut Ui, acts: &mut Vec<Act>) {
        match &self.related {
            Some(Ok(rels)) if !rels.is_empty() => {
                for kind in ["ReplicaSet", "Job", "Pod"] {
                    let items: Vec<&Rel> = rels.iter().filter(|r| r.kind == kind).collect();
                    if items.is_empty() {
                        continue;
                    }
                    section(ui, &format!("{kind}s ({})", items.len()));
                    egui::Grid::new(("related", kind)).num_columns(3).striped(true).spacing([12.0, 3.0]).show(ui, |ui| {
                        for r in items {
                            let color = cell_color("Status", &r.status).unwrap_or(ui.visuals().weak_text_color());
                            watch::dot(ui, color);
                            obj_link(ui, acts, &r.group, &r.kind, &r.ns, &r.name);
                            find::label(ui, RichText::new(&r.status).color(color));
                            ui.end_row();
                        }
                    });
                }
            }
            Some(Err(e)) => {
                ui.colored_label(RED, format!("Related objects: {e}"));
            }
            _ => {}
        }
    }

    fn events_ui(&self, ui: &mut Ui) {
        match &self.events {
            Some(Ok(evs)) if !evs.is_empty() => {
                section(ui, &format!("Events ({})", evs.len()));
                for e in evs.iter().take(50) {
                    ui.horizontal_wrapped(|ui| {
                        let c = if e.kind == "Warning" { ORANGE } else { GREEN };
                        find::label(ui, RichText::new(&e.reason).font(ui_kit::semibold(12.5)).color(ui_kit::status_fg(ui, c)));
                        ui.label(RichText::new(format!("{} ago{}", e.age, if e.count > 1 { format!(" ×{}", e.count) } else { String::new() })).font(ui_kit::mono(11.5)).color(ui_kit::tokens(ui).dim));
                    });
                    find::wrapped(ui, &e.message);
                    ui.add_space(4.0);
                }
            }
            Some(Err(e)) => {
                ui.colored_label(RED, format!("Events: {e}"));
            }
            _ => {}
        }
    }
}

fn selector(sel: &Value) -> String {
    sel["matchLabels"].as_object().into_iter().flatten().map(|(k, v)| format!("{k}={}", s(v))).collect::<Vec<_>>().join(", ")
}

fn images(template: &Value) -> String {
    arr(&template["spec"]["containers"]).map(|c| s(&c["image"])).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_one_key_at_a_time() {
        let data = json!({"user": "YWRtaW4=", "bin": "/w=="});
        let set = |orig: Option<&str>, key: &str, value: &str| Edit::Set(KeyEdit { orig: orig.map(String::from), key: key.into(), value: value.into() });
        assert_eq!(decode_value("YWRtaW4=", true).as_deref(), Some("admin"));
        assert_eq!(decode_value("/w==", true), None); // binary: not editable, kept as is
        assert_eq!(apply_edit(&data, &set(Some("user"), "user", "root"), true).unwrap(), json!({"user": "cm9vdA==", "bin": "/w=="}));
        assert_eq!(apply_edit(&data, &set(Some("user"), "login", "root"), true).unwrap(), json!({"login": "cm9vdA==", "bin": "/w=="}));
        assert_eq!(apply_edit(&data, &set(None, "new", "x"), true).unwrap(), json!({"user": "YWRtaW4=", "bin": "/w==", "new": "eA=="}));
        assert!(apply_edit(&data, &set(None, "bin", "x"), true).is_err()); // would overwrite another key
        assert!(apply_edit(&data, &set(None, " ", "x"), true).is_err());
        assert_eq!(apply_edit(&data, &Edit::Remove("bin".into()), true).unwrap(), json!({"user": "YWRtaW4="}));
        assert_eq!(apply_edit(&json!({"a": "1"}), &set(Some("a"), "a", "2"), false).unwrap(), json!({"a": "2"}));
    }

    #[test]
    fn finds_pod_references() {
        let spec = json!({
            "volumes": [{"configMap": {"name": "cfg"}}, {"secret": {"secretName": "tls"}}, {"persistentVolumeClaim": {"claimName": "data"}},
                        {"projected": {"sources": [{"secret": {"name": "tls"}}]}}],
            "containers": [{"envFrom": [{"configMapRef": {"name": "env"}}], "env": [{"valueFrom": {"secretKeyRef": {"name": "db"}}}]}],
            "imagePullSecrets": [{"name": "reg"}]
        });
        let names: Vec<String> = pod_refs(&spec).into_iter().map(|(_, k, n)| format!("{k}/{n}")).collect();
        assert_eq!(names, ["ConfigMap/cfg", "ConfigMap/env", "PersistentVolumeClaim/data", "Secret/db", "Secret/reg", "Secret/tls"]);
    }

    #[test]
    fn groups_repeated_events() {
        let ev = |r: &str, n| Ev { kind: "Warning".into(), reason: r.into(), message: "m".into(), age: "1m".into(), count: n };
        let g = group_events(&[ev("BackOff", 2), ev("Failed", 1), ev("BackOff", 3)]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].count, 5);
    }
}
