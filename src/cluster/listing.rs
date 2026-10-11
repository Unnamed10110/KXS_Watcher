//! The pages that list things: resource lists, the new-tab launcher and the content search.

use super::*;

impl Ready {
    /// A new tab: every view and resource kind, filtered by typing; Enter opens the first.
    pub(super) fn launcher_ui(&mut self, ui: &mut Ui, pt: &mut PageTab) -> Option<Page> {
        let t = ui_kit::tokens(ui);
        let has = |g: &str, k: &str| self.kinds.iter().any(|x| x.is(g, k));
        let mut entries: Vec<(&str, String, Page)> = vec![("Cluster", "Overview".into(), Page::Overview)];
        if has("", "Node") {
            entries.push(("Cluster", "Nodes".into(), kind_page("", "Node")));
        }
        if has("", "ConfigMap") || has("", "Secret") {
            entries.push(("Cluster", "Search configs & secrets".into(), Page::Search));
        }
        entries.push(("Cluster", "Namespaces".into(), kind_page("", "Namespace")));
        entries.push(("Cluster", "Events".into(), kind_page("", "Event")));
        if has_helm() {
            entries.push(("Cluster", "Helm releases".into(), Page::Helm));
        }
        entries.push(("Workloads", "Workloads overview".into(), Page::Workloads));
        for (section, items) in NAV {
            for (g, k, text) in items.iter().filter(|(g, k, _)| has(g, k)) {
                entries.push((section, text.to_string(), kind_page(g, k)));
            }
        }
        entries.push(("Network", "Port Forwarding".into(), Page::Forwards));
        for (group, kinds) in &self.custom {
            for k in kinds {
                entries.push(("Custom resources", format!("{} · {group}", k.ar.kind), kind_page(group, &k.ar.kind)));
            }
        }
        let q = pt.launch_query.trim().to_string();
        let q = crate::find::Matcher::plain(&q, false);
        entries.retain(|(sec, name, _)| q.hit(name) || q.hit(sec));

        let mut pick = None;
        let w = 520.0_f32.min(ui.available_width());
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(w);
                ui.label(RichText::new("New tab").font(ui_kit::semibold(22.0)).color(t.text));
                ui.label(RichText::new("Open a view or a resource list here. Type to filter, Enter opens the first.").color(t.muted));
                ui.add_space(10.0);
                let r = ui_kit::search_field(ui, &mut pt.launch_query, "Pods, secrets, helm…", None, w);
                if std::mem::take(&mut self.focus_launcher) {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    pick = entries.first().map(|e| e.2.clone());
                }
                ui.add_space(8.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let mut last = "";
                    for (sec, name, page) in &entries {
                        if *sec != last {
                            ui.add_space(8.0);
                            ui_kit::section_label(ui, sec);
                            last = sec;
                        }
                        if ui_kit::nav_item(ui, None, name, None, false, 6.0).clicked() {
                            pick = Some(page.clone());
                        }
                    }
                    if entries.is_empty() {
                        ui.label(RichText::new("Nothing matches.").color(t.muted));
                    }
                });
            });
        });
        pick
    }

    /// Search inside Config Maps and Secrets; true when the namespace filter changed.
    /// A click shows the object in the side panel, a double-click opens its tab here;
    /// either way the query is carried over, so the details highlight the same text.
    pub(super) fn search_page(&mut self, ui: &mut Ui, pt: &mut PageTab) -> bool {
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

    /// A resource list page; returns true when the namespace filter changed.
    pub(super) fn list_page(&mut self, ui: &mut Ui, pt: &mut PageTab, out: &mut Out, acts: &mut Vec<(Vec<Target>, Act)>) -> bool {
        let Some(kind) = pt.list.as_ref().map(|l| l.kind.clone()) else {
            ui.label("This resource type is not available on this cluster.");
            return false;
        };
        if let Some(l) = &pt.list
            && pt.panel_restore.is_some()
            && l.data.locked().synced
        {
            let (ns, name) = pt.panel_restore.take().unwrap_or_default();
            let data = l.data.clone();
            let row = data.locked().rows.values().find(|r| r.namespace == ns && r.name == name).cloned();
            self.pick(ui.ctx(), pt, kind.clone(), data, row.map(Pick::Show), acts);
        }
        let t = ui_kit::tokens(ui);
        let mut ns_changed = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(label(&kind)).font(ui_kit::semibold(20.0)).color(t.text));
            if let Some(l) = &pt.list {
                let d = l.data.locked();
                let syncing = !d.synced && d.error.is_none();
                let n = if l.view.len() == d.rows.len() { d.rows.len().to_string() } else { format!("{} / {}", l.view.len(), d.rows.len()) };
                drop(d);
                ui_kit::chip(ui, &n, ui_kit::mono(12.0));
                if syncing {
                    crate::ui_kit::spinner(ui);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kind.can("create") && ui_kit::button(ui, Btn::Primary, Some(Icon::Plus), "Create").on_hover_text("Create resource from YAML").clicked() {
                    out.tab(Body::Yaml(Box::new(YamlTab::create(self.client.clone(), self.kinds.clone(), self.default_ns()))));
                }
                if ui_kit::button(ui, if self.wide { Btn::On } else { Btn::Normal }, None, "Wide").on_hover_text("Show extra columns (kubectl -o wide)").clicked() {
                    self.wide = !self.wide;
                }
                if let Some(l) = &pt.list {
                    ui.menu_button("⤓ Export", |ui| {
                        for (format, text) in [(crate::list::Export::Csv, "CSV…"), (crate::list::Export::Json, "JSON…")] {
                            if ui.button(text).on_hover_text("Save the rows shown, with the columns shown").clicked() {
                                let body = l.export(self.wide, &self.metrics.locked(), format);
                                let dialog = rfd::AsyncFileDialog::new().set_title("Export the list").set_file_name(format!("{}.{}", l.kind.ar.plural, format.extension()));
                                self.actions.push(Pending::spawn(ui.ctx(), async move {
                                    let Some(f) = dialog.save_file().await else { return Ok(String::new()) };
                                    let path = f.path().to_path_buf();
                                    std::fs::write(&path, body).map(|_| format!("Saved {}", path.display())).map_err(|e| e.to_string())
                                }));
                                ui.close();
                            }
                        }
                    });
                }
                if kind.namespaced {
                    ns_changed = self.ns_picker(ui);
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if let Some(l) = &mut pt.list {
                let r = ui_kit::search_field(ui, &mut l.search, "Filter by name", Some("Ctrl F"), 280.0)
                    .on_hover_text("Names only: jpts*|ingenico*|as400* keeps the names containing jpts, ingenico or as400 (| or, * anything).
Ctrl+K finds text in every column.");
                if std::mem::take(&mut self.focus_filter) {
                    r.request_focus();
                }
                if ui_kit::button(ui, if l.filter_re { Btn::On } else { Btn::Normal }, None, ".*").on_hover_text("Filter with a regular expression").clicked() {
                    l.filter_re = !l.filter_re;
                }
                if let Some(e) = &l.filter_error {
                    ui.colored_label(RED, "invalid regex").on_hover_text(e);
                } else if !l.search.trim().is_empty() {
                    let (n, rows) = (l.filter_hits, l.view.len());
                    let text = format!(" {n} match{} in {rows} row{} ", if n == 1 { "" } else { "es" }, if rows == 1 { "" } else { "s" });
                    ui.label(RichText::new(text).strong().color(crate::find::HIT).background_color(crate::find::hit_bg(false).gamma_multiply(0.6)));
                }
            }
            ui.add_space(8.0);
            // A selection swaps the status filter for its actions: same row, nothing jumps.
            if !self.bulk_actions(ui, pt, acts)
                && let Some(l) = &mut pt.list
                    && let Some(counts) = l.status_counts() {
                        let total: usize = counts.iter().map(|c| c.1).sum();
                        egui::Frame::new().fill(t.chrome).stroke(egui::Stroke::new(1.0, t.line)).corner_radius(egui::CornerRadius::same(8)).inner_margin(egui::Margin::same(2)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if ui_kit::segment(ui, &format!("All {total}"), None, l.status.is_none()).clicked() {
                                    l.status = None;
                                }
                                for (b, n) in counts {
                                    if n == 0 && l.status != Some(b) {
                                        continue;
                                    }
                                    if ui_kit::segment(ui, &format!("{b} {n}"), Some(crate::list::bucket_color(b)), l.status == Some(b)).clicked() {
                                        l.status = if l.status == Some(b) { None } else { Some(b) };
                                    }
                                }
                            });
                        });
                    }
        });
        ui.add_space(6.0);
        let Some(l) = &mut pt.list else { return ns_changed };
        if let Some(e) = l.data.locked().visible_error() {
            let hint = if e.contains("forbidden") && kind.namespaced && self.ns_sel.is_empty() { " — try selecting a namespace" } else { "" };
            ui.colored_label(RED, format!("{e}{hint}"));
        }
        let (m, data) = (self.metrics.locked(), l.data.clone());
        let pick = l.table(ui, &m, self.wide, &mut self.find);
        drop(m);
        self.pick(ui.ctx(), pt, kind, data, pick, acts);
        ns_changed
    }
}
