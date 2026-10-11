//! What the user does with objects: bulk actions, menu picks, running an action, the confirmation dialog.

use super::*;

pub(super) enum Confirm {
    Act(Vec<Target>, Act),
    Helm(String, Vec<String>),
}

impl Ready {
    /// Actions on the selected rows (in place of the status filter); false when nothing is selected.
    pub(super) fn bulk_actions(&mut self, ui: &mut Ui, pt: &mut PageTab, acts: &mut Vec<(Vec<Target>, Act)>) -> bool {
        let Some(l) = &mut pt.list else { return false };
        let sel = l.selected();
        if sel.is_empty() {
            return false;
        }
        let kind = l.kind.clone();
        let targets: Vec<Target> = sel.iter().map(|r| Target { kind: kind.clone(), ns: r.namespace.clone(), name: r.name.clone() }).collect();
        let t = ui_kit::tokens(ui);
        ui.label(RichText::new(format!("{} selected", sel.len())).font(ui_kit::semibold(13.0)).color(t.text));
        let mut act = |a: Act| acts.push((targets.clone(), a));
        if details::has_logs(&kind) && ui_kit::button(ui, Btn::Normal, Some(Icon::Doc), "Logs").on_hover_text("One tab, lines interleaved by time").clicked() {
            act(Act::Logs(None));
        }
        if details::can_restart(&kind) && ui_kit::button(ui, Btn::Normal, None, "Restart").clicked() {
            act(Act::Restart);
        }
        if details::can_scale(&kind) {
            ui.menu_button("Scale ⏷", |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.scale_to).range(0..=1000));
                    if ui.button("Apply").clicked() {
                        act(Act::Scale(self.scale_to));
                        ui.close();
                    }
                });
            });
        }
        if ui_kit::button(ui, Btn::Normal, Some(Icon::Edit), "Edit YAML").clicked() {
            act(Act::Edit);
        }
        if ui_kit::button(ui, Btn::Normal, None, "Copy names").clicked() {
            ui.ctx().copy_text(sel.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join("\n"));
        }
        if kind.can("delete") && ui_kit::button(ui, Btn::Danger, Some(Icon::Trash), "Delete").clicked() {
            act(Act::Delete);
        }
        if ui_kit::button(ui, Btn::Ghost, Some(Icon::X), "Clear").on_hover_text("Esc").clicked() {
            l.sel.clear();
        }
        true
    }

    pub(super) fn pick(&mut self, ctx: &egui::Context, pt: &mut PageTab, kind: Kind, data: Shared, pick: Option<Pick>, acts: &mut Vec<(Vec<Target>, Act)>) {
        match pick {
            // Double-click: the object's tab opens inside this page's tab.
            Some(Pick::Open(r, focus)) => pt.open_obj(ctx, &self.client, kind, r.namespace.clone(), r.name.clone(), focus),
            Some(Pick::Show(r)) => {
                if pt.panel.as_ref().is_none_or(|p| p.uid.as_ref() != Some(&r.uid)) {
                    let details = Details::new(ctx, self.client.clone(), kind, r.namespace.clone(), r.name.clone());
                    pt.panel = Some(ObjTab { id: next_id(), title: r.name.clone(), details, data, uid: Some(r.uid.clone()), _watch: None });
                }
            }
            Some(Pick::Hide) => pt.panel = None,
            Some(Pick::Menu(rows, a)) => acts.push((rows.iter().map(|r: &Arc<Row>| Target { kind: kind.clone(), ns: r.namespace.clone(), name: r.name.clone() }).collect(), a)),
            None => {}
        }
    }

    /// Entry point for every action: confirmation when destructive, fan-out for several targets.
    pub(super) fn act(&mut self, ctx: &egui::Context, targets: Vec<Target>, a: Act, out: &mut Out) {
        if let Act::Open { group, kind, ns, name } = &a {
            match self.kind(group, kind) {
                Some(k) => self.open_in_kind_tab(ctx, k, ns.clone(), name.clone()),
                None => out.toast(Err(format!("{kind} is not available on this cluster"))),
            }
            return;
        }
        let Some(first) = targets.first() else { return };
        if a == Act::NodeShell {
            let msg = format!("Open a terminal on node {}?\n\nA privileged pod is created in kube-system on that node and deleted when the terminal closes.", first.name);
            self.confirm = Some((msg, Confirm::Act(targets, a)));
        } else if a.needs_confirm() {
            let what = match targets.as_slice() {
                [t] => format!("{} {}{}", t.kind.ar.kind, t.name, if t.ns.is_empty() { String::new() } else { format!(" in {}", t.ns) }),
                ts => {
                    let names: Vec<&str> = ts.iter().take(10).map(|t| t.name.as_str()).collect();
                    format!("{} {}s: {}{}", ts.len(), first.kind.ar.kind, names.join(", "), if ts.len() > 10 { ", …" } else { "" })
                }
            };
            let verb = a.label().trim_start_matches(|c: char| !c.is_alphabetic()).to_string();
            self.confirm = Some((format!("{verb} {what}?"), Confirm::Act(targets, a)));
        } else {
            self.exec(ctx, targets, a, out);
        }
    }

    pub(super) fn exec(&mut self, ctx: &egui::Context, targets: Vec<Target>, a: Act, out: &mut Out) {
        let c = self.client.clone();
        match &a {
            Act::Details => {
                let focus = targets.len() == 1;
                for t in targets.into_iter().take(20) {
                    self.open_obj(ctx, self.cur, t.kind, t.ns, t.name, focus);
                }
            }
            Act::Edit => {
                for t in targets.into_iter().take(20) {
                    out.tab(Body::Yaml(Box::new(YamlTab::edit(ctx, c.clone(), t.kind.ar, t.ns, t.name))));
                }
            }
            Act::Logs(container) => {
                let t: Vec<_> = targets.into_iter().map(|t| (t.kind, t.ns, t.name)).collect();
                out.tab(Body::Logs(Box::new(LogTab::new(ctx, c, t, container.clone()))));
            }
            Act::Shell(container) => {
                for t in targets.iter().take(1) {
                    out.term(ctx, format!("Shell {}", t.name), tabs::pod_shell(&self.kctx, &t.ns, &t.name, container.as_deref()), None);
                }
            }
            Act::Drain => {
                for t in targets.iter().take(1) {
                    out.term(ctx, format!("Drain {}", t.name), tabs::drain(&self.kctx, &t.name), None);
                }
            }
            Act::NodeShell => {
                for t in targets.iter().take(1) {
                    let job = ops::node_shell(c.clone(), t.name.clone(), out.node_image.clone());
                    self.node_shells.push((t.name.clone(), Pending::spawn(ctx, job)));
                    out.toast(Ok(format!("Starting a shell on {}…", t.name)));
                }
            }
            Act::Kubectl(k) => {
                let lines: Vec<String> = targets.iter().map(|t| k.line(&self.kctx, t)).collect();
                ctx.copy_text(lines.join("\n"));
                out.toast(Ok(format!("Copied {}", if lines.len() == 1 { "the command".to_string() } else { format!("{} commands", lines.len()) })));
            }
            Act::Forward(target, port) => {
                for t in targets.iter().take(1) {
                    self.forwards.push(ops::port_forward(&self.kctx, &t.ns, target, *port, ctx));
                    out.toast(Ok(format!("Forwarding {target}:{port}; see Network › Port Forwarding")));
                }
            }
            _ => {
                let mut jobs: Vec<Job> = targets.iter().filter_map(|t| self.job(t, &a)).collect();
                let job = match jobs.len() {
                    0 => return,
                    1 => jobs.remove(0),
                    n => {
                        // One summary toast instead of one per object.
                        let verb = a.label().trim_start_matches(|c: char| !c.is_alphabetic()).to_string();
                        Box::pin(ops::all(jobs, format!("{verb}: {n} {}s", targets[0].kind.ar.kind)))
                    }
                };
                self.actions.push(Pending::spawn(ctx, job));
            }
        }
    }

    /// The API call behind one API action on one object.
    pub(super) fn job(&self, t: &Target, a: &Act) -> Option<Job> {
        let (c, ar, ns, name) = (self.client.clone(), t.kind.ar.clone(), t.ns.clone(), t.name.clone());
        let patch = |p: Value, done: String| -> Job { Box::pin(ops::merge_patch(c.clone(), ar.clone(), ns.clone(), name.clone(), p, done)) };
        Some(match a {
            Act::Delete => Box::pin(ops::delete(c.clone(), ar.clone(), ns.clone(), name.clone())),
            Act::Restart => patch(ops::restart_patch(), format!("Restarting {name}")),
            Act::Scale(n) => patch(ops::scale_patch(*n), format!("Scaled {name} to {n}")),
            Act::Cordon(b) => patch(json!({"spec": {"unschedulable": b}}), format!("{} {name}", if *b { "Cordoned" } else { "Uncordoned" })),
            Act::Suspend(b) => patch(json!({"spec": {"suspend": b}}), format!("{} {name}", if *b { "Suspended" } else { "Resumed" })),
            Act::Trigger => Box::pin(ops::trigger_cronjob(c.clone(), ns.clone(), name.clone())),
            _ => return None,
        })
    }

    pub(super) fn confirm_ui(&mut self, ctx: &egui::Context, out: &mut Out) {
        let Some((msg, _)) = &self.confirm else { return };
        let mut answer = None;
        let resp = egui::Modal::new(egui::Id::new(("confirm", self.kctx.id()))).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading("Please confirm");
            ui.add(egui::Label::new(msg).wrap());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Confirm").color(RED).strong()).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if resp.should_close() && answer.is_none() {
            answer = Some(false);
        }
        let Some(ok) = answer else { return };
        let Some((_, c)) = self.confirm.take() else { return };
        if ok {
            match c {
                Confirm::Act(t, a) => self.exec(ctx, t, a, out),
                Confirm::Helm(done, args) => {
                    let kctx = self.kctx.clone();
                    self.helm.busy = Some(Pending::spawn(ctx, async move { ops::helm(kctx, args).await.map(|_| done) }));
                }
            }
        }
    }
}
