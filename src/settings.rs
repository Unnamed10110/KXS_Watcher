//! Settings (persisted), the settings window and the cluster color pickers.

use super::*;

/// A dock tab kept across restarts: logs (reopened once their cluster connects) and local
/// terminals. Pod and node shells and editors are not: a node shell starts a privileged pod.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Docked {
    Logs { cluster: String, state: tabs::LogState },
    Terminal { context: String },
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Extra kubeconfig files or folders (folders are scanned one level deep).
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) theme: theme::Theme,
    pub(crate) density: theme::Density,
    /// Base text size in points.
    pub(crate) text_size: f32,
    /// Font for names, YAML and logs (`theme::MONO_FONTS`).
    pub(crate) mono_font: String,
    pub(crate) pinned: Vec<String>,
    pub(crate) open: Vec<String>,
    pub(crate) zoom: f32,
    /// Image for node-shell pods (needs `sleep`; nsenter runs the node's own shell).
    pub(crate) node_shell_image: String,
    /// Open page and object tabs per cluster (context id), restored on reconnect.
    pub(crate) tabs: HashMap<String, cluster::SavedTabs>,
    /// Accent color per cluster (context id), handed out from `ACCENTS` on first use.
    pub(crate) accents: HashMap<String, Color32>,
    /// Names given to clusters (tab rename), by context id.
    pub(crate) aliases: HashMap<String, String>,
    /// The cluster shown at exit (context id).
    pub(crate) current: Option<String>,
    /// Font size of the tabs, sidebar, lists, details, logs, editor and terminal.
    pub(crate) sizes: ui_kit::Sizes,
    /// Dock tabs open at exit, in order.
    pub(crate) docked: Vec<Docked>,
    /// Lines of scrollback of the terminals opened from now on.
    pub(crate) terminal_scrollback: usize,
}

/// Cluster colors, readable on dark and light themes. Rose is last, so red stays a deliberate pick (prod).
pub(crate) const ACCENTS: [Color32; 10] = [
    Color32::from_rgb(124, 131, 255), // indigo
    Color32::from_rgb(47, 191, 159),  // teal
    Color32::from_rgb(245, 165, 36),  // amber
    Color32::from_rgb(56, 189, 248),  // sky
    Color32::from_rgb(167, 139, 250), // violet
    Color32::from_rgb(76, 207, 138),  // green
    Color32::from_rgb(251, 146, 60),  // orange
    Color32::from_rgb(232, 121, 249), // orchid
    Color32::from_rgb(148, 163, 184), // slate
    Color32::from_rgb(240, 80, 122),  // rose
];

impl Settings {
    /// The cluster's accent; a new cluster gets the first color no other cluster uses
    /// (a neon one while an AMOLED theme is on).
    pub(crate) fn accent(&mut self, id: &str) -> Color32 {
        let pool: Vec<Color32> = if self.theme.is_amoled() { theme::NEON.iter().map(|n| n.0).collect() } else { ACCENTS.to_vec() };
        let n = self.accents.len();
        let free = pool.iter().copied().find(|c| !self.accents.values().any(|u| u == c));
        *self.accents.entry(id.to_string()).or_insert_with(|| free.unwrap_or(pool[n % pool.len()]))
    }

    pub(crate) fn apply_look(&self, ctx: &egui::Context) {
        theme::apply(ctx, self.theme, self.density, self.text_size);
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            paths: vec![kubeconfig::default_dir()],
            theme: theme::Theme::Dark,
            density: theme::Density::Comfortable,
            text_size: 13.0,
            mono_font: "Auto".into(),
            pinned: vec![],
            open: vec![],
            zoom: 1.0,
            node_shell_image: "busybox:1.36".into(),
            tabs: HashMap::new(),
            accents: HashMap::new(),
            aliases: HashMap::new(),
            current: None,
            sizes: ui_kit::Sizes::default(),
            docked: vec![],
            terminal_scrollback: 5_000,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SettingsPage {
    General,
    Appearance,
    Kubeconfig,
    Terminal,
    Shortcuts,
}

impl App {
    pub(crate) fn settings_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let (mut rescan, mut pick, mut fonts) = (false, None, false);
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(false).fixed_size([900.0, 560.0]).show(ctx, |ui| {
            let t = ui_kit::tokens(ui);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(190.0);
                    for (p, name) in [(SettingsPage::General, "General"), (SettingsPage::Appearance, "Appearance"), (SettingsPage::Kubeconfig, "Kubeconfig sources"), (SettingsPage::Terminal, "Terminal & node shell"), (SettingsPage::Shortcuts, "Keyboard shortcuts")] {
                        if ui_kit::nav_item(ui, None, name, None, self.settings_page == p, 0.0).clicked() {
                            self.settings_page = p;
                        }
                    }
                });
                let (sep, _) = ui.allocate_exact_size(vec2(1.0, 540.0), Sense::hover());
                ui.painter().rect_filled(sep, CornerRadius::ZERO, t.line);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.set_width(680.0);
                    egui::ScrollArea::vertical().id_salt("settings-page").max_height(540.0).show(ui, |ui| {
                        ui.set_width(660.0);
                        let heading = |ui: &mut Ui, title: &str, sub: &str| {
                            ui.label(RichText::new(title).font(ui_kit::semibold(20.0)).color(t.text));
                            ui.label(RichText::new(sub).color(t.muted));
                            ui.add_space(10.0);
                        };
                        match self.settings_page {
                            SettingsPage::General => {
                                heading(ui, "General", "How KXS Watcher behaves.");
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Zoom").font(ui_kit::semibold(13.0)));
                                    if ui.add(egui::Slider::new(&mut self.settings.zoom, 0.7..=2.0).step_by(0.05)).changed() {
                                        ctx.set_zoom_factor(self.settings.zoom);
                                    }
                                });
                                ui.add_space(8.0);
                                ui.label(RichText::new("Open clusters, their tabs and the namespace filter come back on the next start.").color(t.muted));
                                ui.label(RichText::new(format!("Helm CLI: {}", if cluster::has_helm() { "found" } else { "not found on PATH (Helm view hidden)" })).color(t.muted));
                                ui.add_space(8.0);
                                ui.label(RichText::new(format!("KXS Watcher {}", env!("CARGO_PKG_VERSION"))).color(t.muted));
                            }
                            SettingsPage::Appearance => {
                                heading(ui, "Appearance", "Theme, density and cluster colors. Changes apply right away and are saved.");
                                ui.label(RichText::new("Theme").font(ui_kit::semibold(14.0)));
                                ui.add_space(4.0);
                                if theme::tiles(ui, &mut self.settings.theme) {
                                    self.settings.apply_look(ctx);
                                }
                                ui.add_space(8.0);
                                ui.horizontal_top(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new("Density").font(ui_kit::semibold(14.0)));
                                        ui.horizontal(|ui| {
                                            for (d, name) in [(theme::Density::Comfortable, "Comfortable"), (theme::Density::Compact, "Compact")] {
                                                let kind = if self.settings.density == d { Btn::On } else { Btn::Ghost };
                                                if ui_kit::button(ui, kind, None, name).clicked() {
                                                    self.settings.density = d;
                                                    self.settings.apply_look(ctx);
                                                }
                                            }
                                        });
                                    });
                                    ui.add_space(28.0);
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new("Text size").font(ui_kit::semibold(14.0)));
                                        if ui.add(egui::Slider::new(&mut self.settings.text_size, 11.0..=16.0).step_by(0.5).suffix(" px")).changed() {
                                            self.settings.apply_look(ctx);
                                        }
                                    });
                                    ui.add_space(28.0);
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new("Names and logs").font(ui_kit::semibold(14.0)));
                                        egui::ComboBox::from_id_salt("mono-font").selected_text(if self.settings.mono_font == "Auto" { format!("Auto ({})", self.fonts.1) } else { self.settings.mono_font.clone() }).show_ui(ui, |ui| {
                                            for f in theme::MONO_FONTS {
                                                fonts |= ui.selectable_value(&mut self.settings.mono_font, f.to_string(), f).changed();
                                            }
                                        });
                                    });
                                });
                                ui.label(RichText::new(format!("Interface font: {} · install Geist or JetBrains Mono and they are picked up on the next start.", self.fonts.0)).small().color(t.dim));
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Font size by area").font(ui_kit::semibold(14.0)));
                                    ui.label(RichText::new("On top of the text size; the rest of the window keeps it.").color(t.muted));
                                });
                                ui.add_space(4.0);
                                egui::Grid::new("area-sizes").num_columns(3).spacing([18.0, 6.0]).show(ui, |ui| {
                                    for (area, name, what) in ui_kit::Area::ALL {
                                        ui.label(name);
                                        let v = self.settings.sizes.get_mut(area);
                                        ui.add(egui::Slider::new(v, 0.7..=1.6).step_by(0.05).custom_formatter(|v, _| format!("{:.0} %", v * 100.0)).custom_parser(|s| s.trim_end_matches(['%', ' ']).parse::<f64>().ok().map(|p| p / 100.0)));
                                        ui.label(RichText::new(what).color(t.muted));
                                        ui.end_row();
                                    }
                                });
                                if self.settings.sizes != ui_kit::Sizes::default() && ui_kit::button(ui, Btn::Ghost, None, "Reset font sizes").clicked() {
                                    self.settings.sizes = ui_kit::Sizes::default();
                                }
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Cluster colors").font(ui_kit::semibold(14.0)));
                                    ui.label(RichText::new("Tabs, selections, logs and terminals of a cluster take its color.").color(t.muted));
                                });
                                ui.add_space(4.0);
                                let mut ids: Vec<Ctx> = self.contexts.iter().filter(|c| self.settings.accents.contains_key(&c.id())).cloned().collect();
                                ids.sort_by(|a, b| a.name.cmp(&b.name));
                                if ids.is_empty() {
                                    ui.label(RichText::new("Open a cluster to give it a color.").color(t.muted));
                                }
                                let (mut recolor, mut custom) = (None, None);
                                for c in &ids {
                                    let cur = self.settings.accents.get(&c.id()).copied();
                                    ui.horizontal_top(|ui| {
                                        ui_kit::dot(ui, cur.unwrap_or(t.dim), 5.0);
                                        ui.vertical(|ui| {
                                            ui.set_width(220.0);
                                            ui.label(RichText::new(&c.name).font(ui_kit::semibold(13.0)));
                                            ui.label(RichText::new(c.file.display().to_string()).font(ui_kit::mono(11.0)).color(t.dim));
                                        });
                                        ui.vertical(|ui| {
                                            if let Some(col) = color_choices(ui, cur) {
                                                recolor = Some((c.id(), col));
                                            }
                                        });
                                        if ui_kit::button(ui, Btn::Normal, None, "Custom…").clicked() {
                                            custom = Some((c.id(), c.name.clone()));
                                        }
                                    });
                                    ui.add_space(6.0);
                                }
                                if let Some((id, col)) = recolor {
                                    self.settings.accents.insert(id, col);
                                }
                                if custom.is_some() {
                                    self.custom_accent = custom;
                                }
                            }
                            SettingsPage::Kubeconfig => {
                                heading(ui, "Kubeconfig sources", "One file or folder per line. ~/.kube/config and $KUBECONFIG are always included; folders are scanned one level deep.");
                                ui.add(egui::TextEdit::multiline(&mut self.paths_text).desired_rows(8).desired_width(f32::INFINITY).code_editor());
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if ui_kit::button(ui, Btn::Primary, None, "Save & rescan").clicked() {
                                        self.settings.paths = self.paths_text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect();
                                        rescan = true;
                                    }
                                    if ui_kit::button(ui, Btn::Normal, Some(Icon::Plus), "Add files…").on_hover_text("Pick kubeconfig files on disk").clicked() {
                                        pick = Some(false);
                                    }
                                    if ui_kit::button(ui, Btn::Normal, Some(Icon::Folder), "Add folder…").on_hover_text("Every kubeconfig in the folder (one level deep)").clicked() {
                                        pick = Some(true);
                                    }
                                });
                            }
                            SettingsPage::Terminal => {
                                heading(ui, "Terminal & node shell", "Terminals run kubectl pinned to their cluster; no credentials are copied.");
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Node shell image").font(ui_kit::semibold(13.0))).on_hover_text("Used for node terminals: a privileged pod with this image runs on the node while the terminal is open");
                                    ui.add(egui::TextEdit::singleline(&mut self.settings.node_shell_image).desired_width(260.0));
                                });
                                ui.label(RichText::new("The image needs `sleep`; nsenter runs the node's own shell.").color(t.muted));
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Scrollback").font(ui_kit::semibold(13.0))).on_hover_text("Lines each terminal keeps; they cost memory for as long as they are kept");
                                    ui.add(egui::DragValue::new(&mut self.settings.terminal_scrollback).range(500..=50_000).speed(100).suffix(" lines"));
                                });
                                ui.label(RichText::new("Applies to the terminals opened from now on.").color(t.muted));
                            }
                            SettingsPage::Shortcuts => {
                                heading(ui, "Keyboard shortcuts", "");
                                egui::Grid::new("shortcuts").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
                                    for (k, what) in [
                                        ("Ctrl T", "New tab"),
                                        ("Ctrl W", "Close the tab (the dock tab under the pointer)"),
                                        ("Ctrl Tab", "Next tab (Shift: previous)"),
                                        ("Ctrl 1…9", "Go to cluster 1…8, 9 for the last"),
                                        ("Ctrl K", "Find text in the current view"),
                                        ("Ctrl F", "Focus the list filter (find in logs and YAML)"),
                                        ("Ctrl A", "Select every row shown"),
                                        ("Esc", "Clear the selection and hide the details"),
                                        ("Ctrl S", "Save in the YAML editor"),
                                        ("Enter / Shift Enter", "Next / previous match"),
                                        ("Double-click", "Open as a tab (sidebar: keep the page tab)"),
                                        ("Middle-click", "Open in the background, or close a tab"),
                                    ] {
                                        ui_kit::kbd(ui, k);
                                        ui.label(what);
                                        ui.end_row();
                                    }
                                });
                            }
                        }
                    });
                });
            });
        });
        self.show_settings = open;
        if rescan {
            self.rescan();
        }
        if let Some(folder) = pick {
            self.pick_kubeconfigs(ctx, folder);
        }
        if fonts {
            self.fonts = theme::install_fonts(ctx, &self.settings.mono_font);
        }
    }

    /// Any color for a cluster; changes apply live.
    pub(crate) fn custom_accent_ui(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.custom_accent.clone() else { return };
        let (mut open, mut done) = (true, false);
        egui::Window::new(format!("Cluster color · {name}")).id(egui::Id::new("custom-accent")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let mut col = self.settings.accent(&id);
            if egui::color_picker::color_picker_color32(ui, &mut col, egui::color_picker::Alpha::Opaque) {
                self.settings.accents.insert(id.clone(), col);
            }
            ui.add_space(4.0);
            done = ui_kit::button(ui, Btn::Primary, None, "Done").clicked();
        });
        if !open || done {
            self.custom_accent = None;
        }
    }
}

/// Soft and neon swatches, one row each; returns the color clicked.
pub(crate) fn color_choices(ui: &mut Ui, cur: Option<Color32>) -> Option<Color32> {
    let dim = ui_kit::tokens(ui).dim;
    let mut picked = None;
    let neon: Vec<Color32> = theme::NEON.iter().map(|n| n.0).collect();
    for (label, colors) in [("Soft", ACCENTS.to_vec()), ("Neon", neon)] {
        ui.horizontal(|ui| {
            ui.add_sized([34.0, 22.0], egui::Label::new(RichText::new(label).size(11.5).color(dim)));
            for c in colors {
                if swatch(ui, c, cur == Some(c)).clicked() {
                    picked = Some(c);
                }
            }
        });
    }
    picked
}

/// A color choice; ringed when selected.
pub(crate) fn swatch(ui: &mut Ui, color: Color32, on: bool) -> egui::Response {
    let t = ui_kit::tokens(ui);
    let (rect, r) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    ui.painter().rect_filled(rect.shrink(2.0), CornerRadius::same(5), color);
    if on {
        ui.painter().rect_stroke(rect, CornerRadius::same(7), Stroke::new(2.0, t.text), StrokeKind::Inside);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip_and_older_or_newer_files() {
        let mut s = Settings { zoom: 1.3, ..Settings::default() };
        s.accents.insert("a|qa".into(), Color32::from_rgb(1, 2, 3));
        s.docked = vec![Docked::Terminal { context: "a|qa".into() }, Docked::Logs { cluster: "a|qa".into(), state: tabs::LogState::default() }];
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!((back.zoom, back.docked.len(), back.accents.len()), (1.3, 2, 1));
        assert_eq!(back.docked, s.docked);
        // A file from an older version has fewer fields, a newer one more: both load.
        let old: Settings = serde_json::from_str(r#"{"zoom":1.5}"#).unwrap();
        assert_eq!((old.zoom, old.text_size, old.mono_font.as_str()), (1.5, 13.0, "Auto"));
        let newer: Settings = serde_json::from_str(r#"{"zoom":1.2,"a_setting_of_the_future":{"x":1}}"#).unwrap();
        assert_eq!(newer.zoom, 1.2);
    }
}
