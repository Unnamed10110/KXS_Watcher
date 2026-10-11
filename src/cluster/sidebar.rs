//! The sidebar: pages of a cluster, custom resources, the namespace picker.

use super::*;

/// Sidebar sections: (title, [(group, kind, label)]). Entries missing from discovery are hidden.
pub(super) const NAV: &[(&str, &[(&str, &str, &str)])] = &[
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

pub(super) const WORKLOADS: &[(&str, &str)] = &[("", "Pod"), ("apps", "Deployment"), ("apps", "DaemonSet"), ("apps", "StatefulSet"), ("apps", "ReplicaSet"), ("batch", "Job"), ("batch", "CronJob")];

/// Fallback for spotting custom resources when CRDs can't be listed.
pub(super) const BUILTIN_GROUPS: &[&str] = &[
    "", "apps", "batch", "autoscaling", "policy", "networking.k8s.io", "storage.k8s.io", "rbac.authorization.k8s.io", "scheduling.k8s.io", "node.k8s.io",
    "coordination.k8s.io", "admissionregistration.k8s.io", "apiextensions.k8s.io", "discovery.k8s.io", "events.k8s.io", "authentication.k8s.io",
    "authorization.k8s.io", "certificates.k8s.io", "flowcontrol.apiserver.k8s.io", "apiregistration.k8s.io", "metrics.k8s.io", "resource.k8s.io",
    "storagemigration.k8s.io", "internal.apiserver.k8s.io",
];

/// Two letters for the cluster avatar: "k8s-qa" → "QA", "rancher-desktop" → "RD".
pub(super) fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty() && !matches!(w.to_ascii_lowercase().as_str(), "k8s" | "k3s" | "kube" | "cluster" | "context")).collect();
    let s: String = match words.as_slice() {
        [] => name.chars().filter(char::is_ascii_alphanumeric).take(2).collect(),
        [w] => w.chars().take(2).collect(),
        [a, b, ..] => a.chars().take(1).chain(b.chars().take(1)).collect(),
    };
    s.to_uppercase()
}

/// Collapsible sidebar group header; returns whether it is open (kept per title).
pub(super) fn nav_section(ui: &mut Ui, title: &str, default_open: bool) -> bool {
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

impl Ready {
    /// Sidebar: the cluster card, pages with object counts, and the namespace filter at the bottom.
    /// Returns ((page, pin), namespace filter changed); a click previews a page, a double-click keeps it.
    pub(super) fn nav(&mut self, ui: &mut Ui, out: &mut Out) -> (Option<(Page, bool)>, bool) {
        crate::prof!("nav");
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
                    out.local_term(ui.ctx(), &self.kctx);
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
        let counts = self.counts.locked().clone();
        // Pages that list a kind count it live; the rest come from the background counts.
        let live: HashMap<(String, String), usize> = self
            .pages
            .iter()
            .filter_map(|p| p.list.as_ref())
            .filter_map(|l| {
                let d = l.data.locked();
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
            if has_helm() {
                item(ui, Some(Icon::Wheel), "Helm releases", (!self.helm.rows.is_empty()).then(|| self.helm.rows.len().to_string()), Page::Helm, 0.0);
            }
            ui.add_space(8.0);
        });
        (goto, ns_changed)
    }

    /// Multi-select namespace filter; true when the selection changed.
    pub(super) fn ns_picker(&mut self, ui: &mut Ui) -> bool {
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
            let d = self.namespaces.locked();
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
}
