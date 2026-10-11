//! The top bar (cluster pills, search box), the cluster catalog, the welcome page and toasts.

use super::*;
use crate::settings::color_choices;

impl App {
    /// Brand, open clusters as pills, the search box (Ctrl+K), warnings, theme and settings.
    pub(crate) fn top_bar(&mut self, ui: &mut Ui, find_req: &mut Option<bool>) {
        let ctx = ui.ctx().clone();
        let t = ui_kit::tokens(ui);
        let bar = ui.max_rect();
        let cur_accent = self.clusters.get(self.cur).and_then(|x| x.cluster.as_ref()).and_then(|id| self.settings.accents.get(id)).copied().unwrap_or(t.accent);
        let (mut select, mut close, mut recolor, mut custom) = (None, None, None, None);
        let (mut moved, mut rename, mut unalias, mut renamed) = (None, None, None, false);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add_space(6.0);
            ui_kit::icon(ui, Icon::Kube, 20.0, cur_accent);
            ui.label(RichText::new("KXS").font(ui_kit::semibold(14.0)).color(t.text).extra_letter_spacing(1.2));
            ui.add_space(6.0);
            let (sep, _) = ui.allocate_exact_size(vec2(1.0, 20.0), Sense::hover());
            ui.painter().rect_filled(sep, CornerRadius::ZERO, t.line_strong);
            ui.add_space(4.0);
            let n = self.clusters.len();
            let mut items = vec![];
            let tabs_size = ui_kit::area(ui_kit::Area::Tabs); // the cluster pills are tabs too
            for (i, tab) in self.clusters.iter().enumerate() {
                let Some(c) = Self::cluster_of(tab) else { continue };
                let id = c.kctx.id();
                let accent = self.settings.accents.get(&id).copied().unwrap_or(t.accent);
                if let Some(rn) = self.renaming.as_mut().filter(|rn| rn.key == id) {
                    let (rect, done) = rn.ui(ui, 150.0);
                    items.push((rect, false, false));
                    if let Some(name) = done {
                        // Empty, or the context's own name, goes back to the context name.
                        match name {
                            Some(n) if !n.is_empty() && n != c.kctx.name => self.settings.aliases.insert(id.clone(), n),
                            Some(_) => self.settings.aliases.remove(&id),
                            None => None,
                        };
                        renamed = true;
                    }
                    continue;
                }
                let label = self.settings.aliases.get(&id).cloned().unwrap_or_else(|| c.kctx.name.clone());
                let (main, x) = cluster_pill(ui, &label, accent, i == self.cur);
                items.push((main.rect, main.dragged(), main.drag_stopped()));
                if main.clicked() {
                    select = Some(i);
                }
                if main.double_clicked() {
                    rename = Some((id.clone(), label.clone()));
                }
                if x.clicked() || main.middle_clicked() {
                    close = Some(i);
                }
                main.context_menu(|ui| {
                    if ui.button("Rename…").clicked() {
                        rename = Some((id.clone(), label.clone()));
                        ui.close();
                    }
                    if self.settings.aliases.contains_key(&id) && ui.button("Reset name").clicked() {
                        unalias = Some(id.clone());
                        ui.close();
                    }
                    moved = ui_kit::move_menu(ui, i, n).or(moved);
                    ui.separator();
                    ui.label(RichText::new("Cluster color").weak());
                    if let Some(col) = color_choices(ui, Some(accent)) {
                        recolor = Some((c.kctx.id(), col));
                        ui.close();
                    }
                    if ui.button("🎨 Custom color…").clicked() {
                        custom = Some((c.kctx.id(), c.kctx.name.clone()));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Close").clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
            }
            drop(tabs_size);
            if let Some(m) = ui_kit::reorder(ui, &items) {
                moved = Some(m);
            }
            if ui_kit::icon_button(ui, Icon::Plus, 30.0, "Connect a cluster").clicked() {
                self.show_catalog = !self.show_catalog;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(6.0);
                if ui_kit::icon_button(ui, Icon::Sliders, 34.0, "Settings").clicked() {
                    self.show_settings = true;
                }
                let r = ui_kit::icon_button(ui, Icon::Contrast, 34.0, "Theme");
                egui::Popup::menu(&r).show(|ui| {
                    for th in theme::ALL {
                        if ui.selectable_label(self.settings.theme == th, th.name()).clicked() {
                            self.settings.theme = th;
                            self.settings.apply_look(&ctx);
                        }
                    }
                });
                let warnings = self.clusters.get(self.cur).and_then(Self::cluster_of).and_then(|c| c.warning_count()).unwrap_or(0);
                let r = ui_kit::icon_button(ui, Icon::Bell, 34.0, &format!("Warnings: {warnings}"));
                if warnings > 0 {
                    let pos = r.rect.right_top() + vec2(-8.0, 9.0);
                    let galley = ui.painter().layout_no_wrap(warnings.to_string(), ui_kit::semibold(10.0), Color32::from_rgb(26, 18, 4));
                    let pill = Rect::from_center_size(pos, vec2((galley.size().x + 8.0).max(16.0), 16.0));
                    ui.painter().rect_filled(pill, CornerRadius::same(8), watch::ORANGE);
                    ui.painter().galley(pill.center() - galley.size() / 2.0, galley, Color32::BLACK);
                }
                if r.clicked()
                    && let Some(Body::Cluster(c)) = self.clusters.get_mut(self.cur).map(|t| &mut t.body) {
                        c.show_overview(&ctx);
                    }
            });
        });
        // The search box sits in the middle of the bar.
        let w = (bar.width() * 0.3).clamp(220.0, 440.0);
        let rect = Rect::from_center_size(bar.center(), vec2(w, 30.0));
        let r = ui.interact(rect, egui::Id::new("top-search"), Sense::click());
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(8), if r.hovered() { t.hover } else { t.raise }, Stroke::new(1.0, t.line_strong), StrokeKind::Inside);
        ui_kit::paint_icon(ui, Rect::from_center_size(Pos2::new(rect.left() + 18.0, rect.center().y), vec2(15.0, 15.0)), Icon::Search, t.muted);
        p.text(Pos2::new(rect.left() + 34.0, rect.center().y), Align2::LEFT_CENTER, "Search in this view", FontId::proportional(13.0), t.muted);
        let k = Rect::from_center_size(Pos2::new(rect.right() - 30.0, rect.center().y), vec2(42.0, 18.0));
        p.rect_stroke(k, CornerRadius::same(4), Stroke::new(1.0, t.line_strong), StrokeKind::Inside);
        p.text(k.center(), Align2::CENTER_CENTER, "Ctrl K", ui_kit::mono(10.5), t.muted);
        if r.on_hover_text("Find text in the current view (Ctrl+K)").clicked() {
            *find_req = Some(false);
        }
        if let Some(i) = select {
            self.cur = i;
        }
        if renamed {
            self.renaming = None;
        }
        if let Some((id, text)) = rename {
            self.renaming = Some(ui_kit::Rename::new(id, text));
        }
        if let Some(id) = unalias {
            self.settings.aliases.remove(&id);
        }
        if let Some((from, to)) = moved {
            ui_kit::move_item(&mut self.clusters, from, to, &mut self.cur);
        }
        if let Some((id, col)) = recolor {
            self.settings.accents.insert(id, col);
        }
        if custom.is_some() {
            self.custom_accent = custom;
        }
        if let Some(i) = close {
            self.close_cluster(i);
        }
    }

    /// The cluster list: pinned, then per kubeconfig file. Returns true when a cluster was opened.
    pub(crate) fn catalog(&mut self, ui: &mut Ui) -> bool {
        let ctx = ui.ctx().clone();
        let t = ui_kit::tokens(ui);
        ui.horizontal(|ui| {
            if ui_kit::button(ui, Btn::Normal, Some(Icon::Plus), "Add kubeconfig…").on_hover_text("Pick kubeconfig files on disk").clicked() {
                self.pick_kubeconfigs(&ctx, false);
            }
            if ui_kit::button(ui, Btn::Normal, Some(Icon::Folder), "Folder…").on_hover_text("Add a folder: every kubeconfig in it (one level deep)").clicked() {
                self.pick_kubeconfigs(&ctx, true);
            }
            if self.picking.is_some() {
                crate::ui_kit::spinner(ui);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui_kit::icon_button(ui, Icon::Activity, 30.0, "Rescan kubeconfig files").clicked() {
                    self.rescan();
                }
            });
        });
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter contexts…").desired_width(f32::INFINITY));
        ui.add_space(2.0);
        let open = self.open_ids();
        let (mut to_open, mut toggle_pin, mut terminal, mut recolor, mut custom) = (None, None, None, None, None);
        let filter = crate::find::Matcher::plain(&self.filter, false);
        let visible: Vec<&Ctx> = self.contexts.iter().filter(|c| filter.hit(&c.name) || filter.hit(&c.server)).collect();
        let pinned: Vec<&Ctx> = visible.iter().copied().filter(|c| self.settings.pinned.contains(&c.id())).collect();
        let mut files: Vec<&PathBuf> = visible.iter().map(|c| &c.file).collect();
        files.dedup();

        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(420.0).show(ui, |ui| {
            let mut entry = |ui: &mut Ui, c: &Ctx| {
                let is_open = open.contains(&c.id());
                let is_pinned = self.settings.pinned.contains(&c.id());
                let accent = self.settings.accents.get(&c.id()).copied().unwrap_or(t.dim);
                let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
                let fill = if is_open { mix(t.bg, accent, 0.12) } else if r.hovered() { t.hover } else { Color32::TRANSPARENT };
                ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
                ui.painter().circle_filled(Pos2::new(rect.left() + 14.0, rect.center().y), 4.5, accent);
                let shown = self.settings.aliases.get(&c.id()).map_or(c.name.as_str(), String::as_str);
                ui.painter().text(Pos2::new(rect.left() + 28.0, rect.top() + 13.0), Align2::LEFT_CENTER, shown, ui_kit::semibold(13.0), t.text);
                ui.painter().text(Pos2::new(rect.left() + 28.0, rect.top() + 28.0), Align2::LEFT_CENTER, &c.server, FontId::proportional(11.5), t.dim);
                if is_open {
                    ui.painter().text(Pos2::new(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, "open", FontId::proportional(11.5), t.muted);
                }
                let r = r.on_hover_text(format!("{}\n{}", c.server, c.file.display()));
                if r.clicked() {
                    to_open = Some(c.clone());
                }
                r.context_menu(|ui| {
                    if ui.button("☸ Connect").clicked() {
                        to_open = Some(c.clone());
                        ui.close();
                    }
                    if ui.button("⌨ Open terminal").clicked() {
                        terminal = Some(c.clone());
                        ui.close();
                    }
                    if ui.button(if is_pinned { "📌 Unpin" } else { "📌 Pin to top" }).clicked() {
                        toggle_pin = Some(c.id());
                        ui.close();
                    }
                    if ui.button("Copy context name").clicked() {
                        ui.ctx().copy_text(c.name.clone());
                        ui.close();
                    }
                    ui.separator();
                    ui.label(RichText::new("Cluster color").weak());
                    if let Some(col) = color_choices(ui, self.settings.accents.get(&c.id()).copied()) {
                        recolor = Some((c.id(), col));
                        ui.close();
                    }
                    // A window, not a submenu: context menus close on any click, even inside a picker.
                    if ui.button("🎨 Custom color…").clicked() {
                        custom = Some((c.id(), c.name.clone()));
                        ui.close();
                    }
                });
            };
            if !pinned.is_empty() {
                ui_kit::section_label(ui, "Pinned");
                for c in &pinned {
                    entry(ui, c);
                }
                ui.add_space(4.0);
            }
            for f in files {
                let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ui_kit::section_label(ui, &name).on_hover_text(f.display().to_string());
                for c in visible.iter().filter(|c| &c.file == f) {
                    entry(ui, c);
                }
                ui.add_space(4.0);
            }
            if self.contexts.is_empty() {
                ui.label(RichText::new("No contexts found. Add kubeconfig files or folders above.").color(t.muted));
            }
        });

        if let Some(id) = toggle_pin {
            if let Some(i) = self.settings.pinned.iter().position(|p| *p == id) {
                self.settings.pinned.remove(i);
            } else {
                self.settings.pinned.push(id);
            }
        }
        if let Some((id, col)) = recolor {
            self.settings.accents.insert(id, col);
        }
        if custom.is_some() {
            self.custom_accent = custom;
        }
        if let Some(c) = terminal {
            let mut out = Out::default();
            out.local_term(&ctx, &c);
            self.settings.accent(&c.id());
            out.tabs.iter_mut().for_each(|t| t.cluster = Some(c.id()));
            self.absorb(out);
        }
        match to_open {
            Some(c) => {
                self.open_cluster(&ctx, c);
                true
            }
            None => false,
        }
    }

    pub(crate) fn welcome(&mut self, ui: &mut Ui) {
        let t = ui_kit::tokens(ui);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() / 7.0).max(24.0));
            ui_kit::icon(ui, Icon::Kube, 48.0, t.accent);
            ui.add_space(8.0);
            ui.label(RichText::new("KXS Watcher").font(ui_kit::semibold(26.0)).color(t.text));
            ui.label(RichText::new("Fast, native Kubernetes & k3s manager").color(t.muted));
            ui.add_space(18.0);
        });
        let w = 560.0_f32.min(ui.available_width() - 32.0);
        ui.horizontal(|ui| {
            ui.add_space((ui.available_width() - w) / 2.0);
            ui.vertical(|ui| {
                ui.set_width(w);
                let mut opened = false;
                ui_kit::card(ui, Some("Connect a cluster"), |ui| opened = self.catalog(ui));
                let _ = opened;
            });
        });
    }

    pub(crate) fn catalog_ui(&mut self, ctx: &egui::Context) {
        if !self.show_catalog {
            return;
        }
        let mut open = true;
        let mut opened = false;
        egui::Window::new("Clusters").open(&mut open).collapsible(false).resizable(false).default_width(420.0).anchor(Align2::LEFT_TOP, [16.0, 56.0]).show(ctx, |ui| {
            opened = self.catalog(ui);
        });
        self.show_catalog = open && !opened;
    }

    pub(crate) fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.2.elapsed() < Duration::from_secs(if t.1 { 10 } else { 5 }));
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts")).anchor(Align2::RIGHT_BOTTOM, [-14.0, -14.0]).order(egui::Order::Foreground).show(ctx, |ui| {
            for (msg, err, _) in &self.toasts {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(460.0);
                    ui.colored_label(if *err { RED } else { GREEN }, msg);
                });
            }
        });
        crate::repaint::after(ctx, Duration::from_millis(500));
    }
}

/// A cluster in the top bar: color dot, name, close button. Returns (pill, close).
pub(crate) fn cluster_pill(ui: &mut Ui, name: &str, accent: Color32, current: bool) -> (egui::Response, egui::Response) {
    let t = ui_kit::tokens(ui);
    let font = if current { ui_kit::semibold(13.0) } else { ui_kit::prop(13.0) };
    let fg = if current { t.text } else { t.muted };
    let galley = ui.painter().layout_no_wrap(name.to_owned(), font, fg);
    let (rect, main) = ui.allocate_exact_size(vec2(galley.size().x + 50.0, ui_kit::sz(30.0)), Sense::click_and_drag());
    let fill = if current { mix(t.chrome, accent, if t.neon { 0.1 } else { 0.16 }) } else if main.hovered() { t.hover } else { Color32::TRANSPARENT };
    let stroke = if current { Stroke::new(1.0, if t.neon { accent } else { mix(t.chrome, accent, 0.5) }) } else { Stroke::NONE };
    ui.painter().rect(rect, CornerRadius::same(8), fill, stroke, StrokeKind::Inside);
    ui.painter().circle_filled(Pos2::new(rect.left() + 13.0, rect.center().y), 4.0, accent);
    ui.painter().galley(Pos2::new(rect.left() + 24.0, rect.center().y - galley.size().y / 2.0), galley, fg);
    let xr = Rect::from_center_size(Pos2::new(rect.right() - 14.0, rect.center().y), vec2(20.0, 20.0));
    let x = ui.interact(xr, main.id.with("close"), Sense::click());
    if x.hovered() {
        ui.painter().rect_filled(xr, CornerRadius::same(5), t.tag);
    }
    ui_kit::paint_icon(ui, xr.shrink(5.0), Icon::X, if x.hovered() { t.text } else { t.dim });
    (main.on_hover_text("Drag to move · double-click to rename · right-click for color · middle-click to close"), x.on_hover_text(format!("Close {name}")))
}
