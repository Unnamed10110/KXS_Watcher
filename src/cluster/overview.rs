//! The overview and workloads pages: cards, status breakdowns, recent warnings.

use super::*;

/// One overview number: title, value, a usage bar and a footnote.
pub(super) fn stat_card(ui: &mut Ui, title: &str, value: &str, sub: &str, frac: Option<f32>, foot: &str, warn: bool) {
    let t = ui_kit::tokens(ui);
    let (fill, line) = if warn { (mix(t.card, ORANGE, 0.05), mix(t.card, ORANGE, 0.3)) } else { (t.card, t.line) };
    egui::Frame::new().fill(fill).stroke(egui::Stroke::new(1.0, line)).corner_radius(egui::CornerRadius::same(12)).inner_margin(egui::Margin::symmetric(18, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        ui_kit::section_label(ui, title);
        ui.horizontal(|ui| {
            ui.label(RichText::new(value).font(ui_kit::semibold(28.0)).color(if warn { ui_kit::status_fg(ui, ORANGE) } else { t.text }));
            ui.label(RichText::new(sub).color(t.muted));
        });
        let w = ui.available_width();
        match frac {
            Some(f) => ui_kit::bar(ui, f, w, if f > 0.85 { RED } else { t.accent }),
            None => ui.allocate_exact_size(egui::vec2(w, 6.0), egui::Sense::hover()).1,
        };
        ui.add(egui::Label::new(RichText::new(foot).size(ui_kit::sz(12.0)).color(t.dim)).truncate());
    });
}

/// Status breakdown for the workloads overview cards.
pub(super) fn breakdown(d: &ListData) -> Vec<(String, usize)> {
    let idx = |n: &str| d.cols.iter().position(|c| c.name == n);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in d.rows.values() {
        let cell = |i: usize| r.cells.get(i).map_or("", |s| s.as_str());
        let key = if let Some(i) = idx("Status") {
            cell(i).to_string()
        } else if let (Some(des), Some(rdy)) = (idx("Desired"), idx("Ready")) {
            if cell(des) == cell(rdy) { "Ready" } else { "Not ready" }.into()
        } else if let Some(i) = idx("Ready").or(idx("Completions")) {
            match cell(i).split_once('/') {
                Some((a, b)) if a.trim() == b.trim() => "Ready".into(),
                Some(_) => "Not ready".into(),
                None => cell(i).to_string(),
            }
        } else if let Some(i) = idx("Suspend") {
            if cell(i) == "True" { "Suspended" } else { "Active" }.into()
        } else {
            "Total".into()
        };
        *counts.entry(key).or_default() += 1;
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

impl Ready {
    /// Cluster overview: usage cards, nodes, the busiest pods and warning events.
    /// Returns true when the namespace filter changed.
    pub(super) fn overview(&mut self, ui: &mut Ui, acts: &mut Vec<(Vec<Target>, Act)>) -> bool {
        let t = ui_kit::tokens(ui);
        let mut ns_changed = false;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(self.alias.as_deref().unwrap_or(&self.kctx.name)).font(ui_kit::semibold(24.0)).color(t.text));
                    let nodes = self.metrics.locked().alloc.len();
                    let nss = self.namespaces.locked().rows.len();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} · {nodes} nodes · {nss} namespaces · {}", self.version, self.kctx.server)).color(t.muted));
                        if self.has_metrics {
                            ui_kit::dot(ui, GREEN, 3.0);
                            ui.label(RichText::new("metrics-server live").color(t.muted));
                        }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ns_changed = self.ns_picker(ui));
            });
            ui.add_space(14.0);

            let m = self.metrics.locked();
            if let Some(e) = &m.error {
                ui.colored_label(RED, e);
            }
            if m.at.is_none() && m.error.is_none() {
                crate::ui_kit::spinner(ui);
            }
            let (mut a, mut u) = ((0.0, 0.0, 0.0), (0.0, 0.0));
            for v in m.alloc.values() {
                a = (a.0 + v.0, a.1 + v.1, a.2 + v.2);
            }
            for v in m.nodes.values() {
                u = (u.0 + v.0, u.1 + v.1);
            }
            let warnings = self.warnings.as_ref().map(|l| l.grouped_rows()).unwrap_or_default();
            let (warn_rows, warn_cols) = (&warnings.0, &warnings.1);
            let col = |name: &str| warn_cols.iter().position(|c| c.name == name);
            let cell = |r: &Row, i: Option<usize>| i.and_then(|i| r.cells.get(i)).cloned().unwrap_or_default();
            let (reason_i, obj_i, msg_i, seen_i) = (col("Reason"), col("Object"), col("Message"), col("Last Seen"));
            let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
            for (r, _) in warn_rows {
                *reasons.entry(cell(r, reason_i)).or_default() += 1;
            }
            let mut reasons: Vec<(String, usize)> = reasons.into_iter().collect();
            reasons.sort_by_key(|x| std::cmp::Reverse(x.1));
            let top_reasons = reasons.iter().take(3).map(|(r, n)| format!("{r} {n}")).collect::<Vec<_>>().join(" · ");

            ui.columns(4, |c| {
                if self.has_metrics {
                    let pct = |x: f64, y: f64| if y > 0.0 { (x / y) as f32 } else { 0.0 };
                    stat_card(&mut c[0], "CPU", &format!("{:.0}%", 100.0 * pct(u.0, a.0)), &format!("{} of {} cores", fmt_cpu(u.0), fmt_cpu(a.0)), Some(pct(u.0, a.0)), &format!("Allocatable on {} nodes", m.alloc.len()), false);
                    stat_card(&mut c[1], "Memory", &format!("{:.0}%", 100.0 * pct(u.1, a.1)), &format!("{} of {}", fmt_bytes(u.1), fmt_bytes(a.1)), Some(pct(u.1, a.1)), "Working set, all nodes", false);
                    stat_card(&mut c[2], "Pods", &m.pods.len().to_string(), &format!("of {} capacity", a.2), Some(pct(m.pods.len() as f64, a.2)), "Pods reporting metrics", false);
                } else {
                    stat_card(&mut c[0], "CPU", &fmt_cpu(a.0), "cores allocatable", None, "metrics-server not available", false);
                    stat_card(&mut c[1], "Memory", &fmt_bytes(a.1), "allocatable", None, "metrics-server not available", false);
                    stat_card(&mut c[2], "Pods", &format!("{}", a.2), "capacity", None, "Usage needs metrics-server", false);
                }
                let n = warn_rows.len();
                stat_card(&mut c[3], "Warnings", &n.to_string(), if n == 1 { "active warning" } else { "active warnings" }, None, if top_reasons.is_empty() { "All quiet" } else { &top_reasons }, n > 0);
            });
            ui.add_space(16.0);

            let right = (ui.available_width() * 0.36).clamp(320.0, 440.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width() - right - 16.0);
                    ui_kit::card(ui, Some(&format!("Nodes · {}", m.alloc.len())), |ui| {
                        egui::Grid::new("ov-nodes").num_columns(4).spacing([22.0, 10.0]).show(ui, |ui| {
                            for h in ["NODE", "CPU", "MEMORY", "PODS"] {
                                ui.label(RichText::new(h).font(ui_kit::semibold(11.0)).color(t.dim));
                            }
                            ui.end_row();
                            let mut names: Vec<&String> = m.alloc.keys().collect();
                            names.sort();
                            for n in names {
                                let (al, us) = (m.alloc[n], m.nodes.get(n).copied());
                                if ui.link(RichText::new(n).font(ui_kit::mono(12.5))).on_hover_text("Open node").clicked() {
                                    acts.push((vec![], Act::Open { group: String::new(), kind: "Node".into(), ns: String::new(), name: n.clone() }));
                                }
                                for (used, total) in [(us.map(|u| u.0), al.0), (us.map(|u| u.1), al.1)] {
                                    ui.horizontal(|ui| match used {
                                        Some(v) => {
                                            let f = (v / total.max(1e-9)) as f32;
                                            ui_kit::bar(ui, f, 90.0, if f > 0.85 { RED } else if f > 0.7 { ORANGE } else { t.accent });
                                            ui.label(RichText::new(format!("{:.0}%", 100.0 * f)).font(ui_kit::mono(12.0)).color(t.muted));
                                        }
                                        None => {
                                            ui.label(RichText::new("—").color(t.dim));
                                        }
                                    });
                                }
                                ui.label(RichText::new(format!("{}", al.2)).font(ui_kit::mono(12.0)).color(t.muted));
                                ui.end_row();
                            }
                        });
                    });
                    ui.add_space(16.0);
                    let mut top: Vec<(&(String, String), &(f64, f64))> = m.pods.iter().collect();
                    top.sort_by(|x, y| y.1.0.total_cmp(&x.1.0));
                    let max = top.first().map_or(1.0, |p| p.1.0.max(1e-9));
                    ui_kit::card(ui, Some("Top pods by CPU"), |ui| {
                        if top.is_empty() {
                            ui.label(RichText::new("No pod metrics yet.").color(t.muted));
                        }
                        egui::Grid::new("ov-top").num_columns(4).spacing([18.0, 8.0]).show(ui, |ui| {
                            for ((ns, name), (cpu, _)) in top.into_iter().take(6) {
                                if ui.link(RichText::new(name).font(ui_kit::mono(12.5))).on_hover_text("Open pod").clicked() {
                                    acts.push((vec![], Act::Open { group: String::new(), kind: "Pod".into(), ns: ns.clone(), name: name.clone() }));
                                }
                                ui.label(RichText::new(ns).color(t.dim));
                                ui_kit::bar(ui, (*cpu / max) as f32, 120.0, t.accent);
                                ui.label(RichText::new(fmt_cpu(*cpu)).font(ui_kit::mono(12.0)).color(t.muted));
                                ui.end_row();
                            }
                        });
                    });
                });
                ui.vertical(|ui| {
                    ui.set_width(right);
                    ui_kit::card(ui, Some("Warning events"), |ui| {
                        if warn_rows.is_empty() {
                            ui.horizontal(|ui| {
                                ui_kit::icon(ui, Icon::Check, 15.0, GREEN);
                                ui.label(RichText::new("No warning events").color(t.muted));
                            });
                        }
                        for (i, (r, n)) in warn_rows.iter().take(8).enumerate() {
                            if i > 0 {
                                ui.add_space(4.0);
                                ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), egui::Stroke::new(1.0, t.line));
                                ui.add_space(6.0);
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(cell(r, reason_i)).font(ui_kit::semibold(12.5)).color(ui_kit::status_fg(ui, ORANGE)));
                                if *n > 1 {
                                    ui_kit::chip(ui, &format!("×{n}"), ui_kit::mono(11.0));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(watch::date_cell(&cell(r, seen_i))).font(ui_kit::mono(11.5)).color(t.dim));
                                });
                            });
                            let obj = cell(r, obj_i);
                            if ui.link(RichText::new(&obj).font(ui_kit::mono(12.0))).clicked()
                                && let Some((k, name)) = obj.split_once('/') {
                                    let kd = self.kinds.iter().find(|x| x.ar.group.is_empty() && x.ar.kind.eq_ignore_ascii_case(k)).or_else(|| self.kinds.iter().find(|x| x.ar.kind.eq_ignore_ascii_case(k)));
                                    if let Some(kd) = kd {
                                        acts.push((vec![], Act::Open { group: kd.ar.group.clone(), kind: kd.ar.kind.clone(), ns: r.namespace.clone(), name: name.to_string() }));
                                    }
                                }
                            ui.add(egui::Label::new(RichText::new(cell(r, msg_i)).size(ui_kit::sz(12.5)).color(t.muted)).wrap());
                        }
                        if warn_rows.len() > 8 {
                            ui.label(RichText::new(format!("{} more under Events", warn_rows.len() - 8)).color(t.dim));
                        }
                    });
                });
            });
        });
        ns_changed
    }

    /// Returns (page to show, namespace filter changed).
    pub(super) fn workloads_ui(&mut self, ui: &mut Ui, pt: &mut PageTab) -> (Option<Page>, bool) {
        let mut goto = None;
        let mut ns_changed = false;
        ui.horizontal(|ui| {
            ui.heading("Workloads");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ns_changed = self.ns_picker(ui));
        });
        ui.add_space(6.0);
        // Frames can't be pre-measured for wrapping, so lay the cards out in rows ourselves.
        let per_row = ((ui.available_width() / 262.0) as usize).max(1);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in pt.workloads.chunks(per_row) {
                ui.horizontal_top(|ui| {
                    for l in row {
                        let d = l.data.locked();
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(236.0);
                            ui.set_min_height(130.0);
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    if ui.link(RichText::new(format!("{} ({})", label(&l.kind), d.rows.len())).strong().size(ui_kit::sz(16.0))).clicked() {
                                        goto = Some(kind_page(&l.kind.ar.group, &l.kind.ar.kind));
                                    }
                                    if !d.synced && d.error.is_none() {
                                        crate::ui_kit::spinner(ui);
                                    }
                                });
                                if let Some(e) = d.visible_error() {
                                    ui.colored_label(RED, e);
                                }
                                for (status, n) in l.breakdown(&d, breakdown) {
                                    ui.horizontal(|ui| {
                                        ui_kit::dot(ui, cell_color("Status", &status).unwrap_or(ui.visuals().weak_text_color()), 4.0);
                                        ui.label(format!("{status}: {n}"));
                                    });
                                }
                            });
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });
        (goto, ns_changed)
    }
}
