//! The Helm page: releases, values, manifest, notes, history, rollback and uninstall (through the CLI).

use super::*;

#[derive(Default)]
pub(super) struct Helm {
    pub(super) rows: Vec<Value>,
    pub(super) load: Option<Pending<Res>>,
    pub(super) error: Option<String>,
    pub(super) search: String,
    /// (namespace, release)
    pub(super) sel: Option<(String, String)>,
    pub(super) history: Vec<Value>,
    pub(super) hist_load: Option<Pending<Res>>,
    pub(super) view: Option<(String, Pending<Res>)>,
    pub(super) busy: Option<Pending<Res>>,
}

impl Ready {
    pub(super) fn helm_refresh(&mut self, ctx: &egui::Context) {
        let args = ["list", "--all-namespaces", "--output", "json"].map(String::from).to_vec();
        self.helm.load = Some(Pending::spawn(ctx, ops::helm(self.kctx.clone(), args)));
    }

    pub(super) fn helm_args(&self, ns: &str, args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).chain(["--namespace".into(), ns.into()]).collect()
    }

    pub(super) fn helm_poll(&mut self, ctx: &egui::Context, out: &mut Out) {
        if let Some(r) = take(&mut self.helm.load) {
            match r.and_then(|s| serde_json::from_str::<Vec<Value>>(&s).map_err(|e| e.to_string())) {
                Ok(rows) => (self.helm.rows, self.helm.error) = (rows, None),
                Err(e) => self.helm.error = Some(e),
            }
        }
        if let Some(r) = take(&mut self.helm.hist_load) {
            self.helm.history = r.ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            self.helm.history.reverse();
        }
        if let Some((title, p)) = &mut self.helm.view
            && let Some(r) = p.poll() {
                match r {
                    Ok(text) => out.tab(Body::Yaml(Box::new(YamlTab::view(title.clone(), text)))),
                    Err(e) => out.toast(Err(e)),
                }
                self.helm.view = None;
            }
        if let Some(r) = take(&mut self.helm.busy) {
            out.toast(r);
            self.helm_refresh(ctx);
            self.helm.history.clear();
        }
    }

    /// Returns true when the namespace filter changed.
    pub(super) fn helm_ui(&mut self, ui: &mut Ui) -> bool {
        let ctx = ui.ctx().clone();
        let mut ns_changed = false;
        ui.horizontal(|ui| {
            ui.heading("Helm Releases");
            if ui.button("⟳ Refresh").clicked() {
                self.helm_refresh(&ctx);
            }
            if self.helm.load.is_some() || self.helm.busy.is_some() {
                crate::ui_kit::spinner(ui);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.helm.search).hint_text("🔍 Filter…").desired_width(200.0));
                ns_changed = self.ns_picker(ui); // releases are listed cluster-wide and filtered here
            });
        });
        if let Some(e) = &self.helm.error {
            ui.colored_label(RED, e);
        }
        let s = |v: &Value, k: &str| v[k].as_str().map(String::from).unwrap_or_else(|| v[k].to_string());
        let search = crate::find::Matcher::plain(&self.helm.search, false);
        let rows: Vec<&Value> = self
            .helm
            .rows
            .iter()
            .filter(|r| self.ns_sel.is_empty() || self.ns_sel.contains(&s(r, "namespace")))
            .filter(|r| search.hit(&s(r, "name")) || search.hit(&s(r, "chart")))
            .collect();
        let mut clicked = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let cols = ["name", "namespace", "chart", "app_version", "revision", "status", "updated"];
        let table_h = if self.helm.sel.is_some() { ui.available_height() * 0.5 } else { ui.available_height() };
        TableBuilder::new(ui)
            .id_salt("helm")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .max_scroll_height(table_h)
            .columns(Column::initial(140.0).clip(true), cols.len() - 1)
            .column(Column::remainder())
            .header(row_h, |mut h| {
                for c in ["Name", "Namespace", "Chart", "App version", "Revision", "Status", "Updated"] {
                    h.col(|ui| {
                        ui.strong(c);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let r = rows[row.index()];
                    let key = (s(r, "namespace"), s(r, "name"));
                    row.set_selected(self.helm.sel.as_ref() == Some(&key));
                    for c in cols {
                        row.col(|ui| {
                            let mut t = s(r, c);
                            if c == "updated" {
                                t.truncate(19);
                            }
                            let color = if c == "status" { cell_color("Status", &t) } else { None };
                            ui.add(egui::Label::new(color.map_or(RichText::new(&t), |col| RichText::new(&t).color(col))).truncate().selectable(false));
                        });
                    }
                    if row.response().clicked() {
                        clicked = Some(key);
                    }
                });
            });
        if let Some(key) = clicked {
            self.helm.history.clear();
            self.helm.hist_load = Some(Pending::spawn(&ctx, ops::helm(self.kctx.clone(), self.helm_args(&key.0, &["history", &key.1, "--output", "json"]))));
            self.helm.sel = Some(key);
        }

        let Some((ns, name)) = self.helm.sel.clone() else { return ns_changed };
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("⎈ {name}")).strong());
            for (text, args) in [("Values", vec!["get", "values", &name]), ("All values", vec!["get", "values", &name, "--all"]), ("Manifest", vec!["get", "manifest", &name]), ("Notes", vec!["get", "notes", &name])] {
                if ui.button(text).clicked() {
                    let p = Pending::spawn(&ctx, ops::helm(self.kctx.clone(), self.helm_args(&ns, &args)));
                    self.helm.view = Some((format!("{text} {name}"), p));
                }
            }
            if ui.button(RichText::new("🗑 Uninstall").color(RED)).clicked() {
                self.confirm = Some((format!("Uninstall Helm release {name} from {ns}?"), Confirm::Helm(format!("Uninstalled {name}"), self.helm_args(&ns, &["uninstall", &name]))));
            }
            if ui.button("×").clicked() {
                self.helm.sel = None;
            }
        });
        ui.label(RichText::new("History").weak());
        egui::ScrollArea::vertical().id_salt("helm-hist").show(ui, |ui| {
            egui::Grid::new("helm-history").num_columns(6).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                for h in ["Revision", "Updated", "Status", "Chart", "Description", ""] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                let latest = self.helm.history.first().map(|h| h["revision"].clone());
                for h in self.helm.history.clone() {
                    let rev = h["revision"].to_string();
                    ui.label(&rev);
                    ui.label(s(&h, "updated").chars().take(19).collect::<String>());
                    ui.colored_label(cell_color("Status", &s(&h, "status")).unwrap_or(ui.visuals().text_color()), s(&h, "status"));
                    ui.label(s(&h, "chart"));
                    ui.label(s(&h, "description"));
                    if Some(&h["revision"]) != latest.as_ref() && ui.small_button("↺ Rollback").clicked() {
                        self.confirm = Some((format!("Roll back {name} to revision {rev}?"), Confirm::Helm(format!("Rolled back {name} to {rev}"), self.helm_args(&ns, &["rollback", &name, &rev]))));
                    }
                    ui.end_row();
                }
            });
        });
        ns_changed
    }
}
