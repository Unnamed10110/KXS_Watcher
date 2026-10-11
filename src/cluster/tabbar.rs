//! Page and object tabs: opening, closing, switching, and the two tab rows.

use super::*;

impl Ready {
    pub(super) fn new_page(&mut self, ctx: &egui::Context, page: Page, preview: bool) -> PageTab {
        let mut t = PageTab::new(page, preview);
        t.load(ctx, &self.client, &self.kinds, &self.ns_sel);
        if t.page == Page::Helm && self.helm.rows.is_empty() && self.helm.load.is_none() {
            self.helm_refresh(ctx);
        }
        t
    }

    /// Ctrl+T or "+": a new tab that asks what to show.
    pub(super) fn new_tab(&mut self) {
        self.pages.push(PageTab::new(Page::Launcher, false));
        self.cur = self.pages.len() - 1;
        self.focus_launcher = true;
    }

    pub(super) fn close_tab(&mut self) {
        match self.pages.get_mut(self.cur) {
            Some(pt) if pt.active > 0 => {
                pt.subs.remove(pt.active - 1);
                pt.active -= 1; // its left neighbour, or the list
            }
            Some(_) => {
                let i = self.cur;
                ui_kit::remove_item(&mut self.pages, i, &mut self.cur);
            }
            None => {}
        }
    }

    /// The current new tab becomes `page` (or, when that page is already open, goes away for it).
    pub(super) fn replace_launcher(&mut self, ctx: &egui::Context, page: Page) {
        match self.pages.iter().position(|t| t.page == page) {
            Some(i) => {
                self.pages.remove(self.cur);
                self.cur = if i > self.cur { i - 1 } else { i };
            }
            None => {
                let t = self.new_page(ctx, page, false);
                self.pages[self.cur] = t;
            }
        }
        self.pages[self.cur].active = 0;
    }

    /// Sidebar click: focus an open page, else preview it (single) or open its own tab (double).
    pub(super) fn show_page(&mut self, ctx: &egui::Context, page: Page, pin: bool) {
        if self.pages.get(self.cur).is_some_and(|t| t.page == Page::Launcher) {
            return self.replace_launcher(ctx, page); // the new tab takes what the sidebar opens
        }
        let tabs: Vec<(&Page, bool)> = self.pages.iter().map(|t| (&t.page, t.preview)).collect();
        match nav_target(&tabs, &page, pin) {
            NavTo::Focus(i) => {
                self.cur = i;
                self.pages[i].preview &= !pin;
            }
            NavTo::Replace(i) => {
                self.pages[i] = self.new_page(ctx, page, true);
                self.cur = i;
            }
            NavTo::Push => {
                let t = self.new_page(ctx, page, !pin);
                self.pages.push(t);
                self.cur = self.pages.len() - 1;
            }
        }
        self.pages[self.cur].active = 0;
    }

    /// Object tab inside page tab `pi`.
    pub(super) fn open_obj(&mut self, ctx: &egui::Context, pi: usize, kind: Kind, ns: String, name: String, focus: bool) {
        let client = self.client.clone();
        if let Some(pt) = self.pages.get_mut(pi) {
            pt.open_obj(ctx, &client, kind, ns, name, focus);
            if focus {
                self.cur = pi;
            }
        }
    }

    /// Links (pod → node, pod → secret): the object opens inside its kind's tab.
    pub(super) fn open_in_kind_tab(&mut self, ctx: &egui::Context, kind: Kind, ns: String, name: String) {
        let page = kind_page(&kind.ar.group, &kind.ar.kind);
        let pi = match self.pages.iter().position(|t| t.page == page) {
            Some(i) => i,
            None => {
                let t = self.new_page(ctx, page, false);
                self.pages.push(t);
                self.pages.len() - 1
            }
        };
        self.open_obj(ctx, pi, kind, ns, name, true);
    }

    /// First tab row: one tab per page (Pods, Deployments…); the preview tab is in italics.
    pub(super) fn page_tabs_ui(&mut self, ui: &mut Ui, pages: &mut Vec<PageTab>) {
        let (mut close, mut keep_only) = (None, None);
        let (mut moved, mut rename, mut renamed) = (None, None, false);
        let t = ui_kit::tokens(ui);
        let n = pages.len();
        ui.scope(|ui| {
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal().id_salt(("pagetabs", self.kctx.id())).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(ui_kit::sz(48.0));
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let mut items = vec![];
                    for (i, pt) in pages.iter_mut().enumerate() {
                        ui.add_space(if i == 0 { 10.0 } else { 18.0 });
                        if let Some(rn) = self.renaming.as_mut().filter(|rn| rn.key == pt.id) {
                            let (rect, done) = rn.ui(ui, 160.0);
                            items.push((rect, false, false));
                            if let Some(name) = done {
                                // Empty goes back to the default name.
                                if let Some(name) = name {
                                    pt.name = (!name.is_empty()).then_some(name);
                                }
                                renamed = true;
                            }
                            continue;
                        }
                        let tip = if pt.preview { "Preview: the next single click in the sidebar replaces it. Double-click to keep it." } else { "Drag to move · double-click to rename · middle-click to close" };
                        let r = ui_kit::tab(ui, &pt.title(), i == self.cur, pt.preview, false).on_hover_text(tip);
                        items.push((r.rect, r.dragged(), r.drag_stopped()));
                        if r.double_clicked() {
                            if pt.preview {
                                pt.preview = false;
                            } else {
                                rename = Some((pt.id, pt.title()));
                            }
                        }
                        if r.clicked() {
                            self.cur = i;
                        }
                        if r.middle_clicked() || ui_kit::icon_button(ui, Icon::X, 20.0, "Close").clicked() {
                            close = Some(i);
                        }
                        r.context_menu(|ui| {
                            if ui.button("Rename…").clicked() {
                                rename = Some((pt.id, pt.title()));
                                ui.close();
                            }
                            if pt.name.is_some() && ui.button("Reset name").clicked() {
                                pt.name = None;
                                ui.close();
                            }
                            moved = ui_kit::move_menu(ui, i, n).or(moved);
                            ui.separator();
                            if pt.preview && ui.button("Keep open").clicked() {
                                pt.preview = false;
                                ui.close();
                            }
                            match ui_kit::close_menu(ui) {
                                Some(ui_kit::Close::This) => close = Some(i),
                                Some(ui_kit::Close::Others) => keep_only = Some(Some(i)),
                                Some(ui_kit::Close::All) => keep_only = Some(None),
                                None => {}
                            }
                        });
                    }
                    if let Some(m) = ui_kit::reorder(ui, &items) {
                        moved = Some(m);
                    }
                    ui.add_space(10.0);
                    if ui_kit::icon_button(ui, Icon::Plus, 24.0, "New tab (Ctrl+T)").clicked() {
                        pages.push(PageTab::new(Page::Launcher, false));
                        self.cur = pages.len() - 1;
                        self.focus_launcher = true;
                    }
                });
            });
        });
        ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top() - 1.0, egui::Stroke::new(1.0, t.line));
        if renamed {
            self.renaming = None;
        }
        if let Some((id, text)) = rename {
            self.renaming = Some(ui_kit::Rename::new(id, text));
        }
        if let Some((from, to)) = moved {
            ui_kit::move_item(pages, from, to, &mut self.cur);
        }
        if let Some(i) = close {
            ui_kit::remove_item(pages, i, &mut self.cur);
        }
        if let Some(keep) = keep_only {
            ui_kit::keep_only(pages, keep);
            self.cur = 0;
        }
    }

    /// Second tab row: the page's list plus the objects opened from it, as pills.
    pub(super) fn subtabs_ui(&mut self, ui: &mut Ui, pt: &mut PageTab) {
        if pt.subs.is_empty() {
            return;
        }
        let (mut close, mut keep_only, mut moved) = (None, None, None);
        ui.add_space(6.0);
        ui.scope(|ui| {
            ui.style_mut().always_scroll_the_only_direction = true;
            egui::ScrollArea::horizontal().id_salt(("subtabs", self.kctx.id(), pt.id)).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if ui_kit::pill_tab(ui, Some(Icon::List), "List", pt.active == 0, false, false).0.clicked() {
                        pt.active = 0;
                    }
                    let n = pt.subs.len();
                    let mut items = vec![];
                    for (i, t) in pt.subs.iter().enumerate() {
                        let (r, x) = ui_kit::pill_tab(ui, None, &t.title, pt.active == i + 1, true, true);
                        let r = r.on_hover_text("Drag to move · middle-click to close");
                        items.push((r.rect, r.dragged(), r.drag_stopped()));
                        if r.clicked() {
                            pt.active = i + 1;
                        }
                        if r.middle_clicked() || x.is_some_and(|x| x.clicked()) {
                            close = Some(i);
                        }
                        r.context_menu(|ui| {
                            moved = ui_kit::move_menu(ui, i, n).or(moved);
                            match ui_kit::close_menu(ui) {
                                Some(ui_kit::Close::This) => close = Some(i),
                                Some(ui_kit::Close::Others) => keep_only = Some(Some(i)),
                                Some(ui_kit::Close::All) => keep_only = Some(None),
                                None => {}
                            }
                        });
                    }
                    if let Some(m) = ui_kit::reorder(ui, &items) {
                        moved = Some(m);
                    }
                });
            });
        });
        ui.add_space(4.0);
        if let Some((from, to)) = moved {
            // `active` counts the list as 0; keep it on the same object tab.
            let mut cur = pt.active.checked_sub(1).unwrap_or(usize::MAX);
            ui_kit::move_item(&mut pt.subs, from, to, &mut cur);
            if pt.active > 0 {
                pt.active = cur + 1;
            }
        }
        if let Some(i) = close {
            pt.subs.remove(i);
            if pt.active > i {
                pt.active -= 1; // closing the active tab shows its left neighbour
            }
        }
        if let Some(keep) = keep_only {
            ui_kit::keep_only(&mut pt.subs, keep);
            pt.active = pt.subs.len().min(pt.active);
        }
    }
}
