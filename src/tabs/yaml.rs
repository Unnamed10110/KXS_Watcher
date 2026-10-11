//! The YAML editor tab: edit an object, create resources, or view text.

use super::*;

pub enum YamlMode {
    Edit { ar: ApiResource, ns: String, name: String },
    Create { kinds: Arc<Vec<Kind>>, ns: String },
    View,
}

pub struct YamlTab {
    pub title: String,
    client: Option<Client>,
    mode: YamlMode,
    text: String,
    load: Option<Pending<Res<Value>>>,
    busy: Option<Pending<Res>>,
    status: Option<Res>,
    pub find: Find,
    /// The text last colored, with the theme and font it was colored for (a frame asks again).
    colors: Option<(String, bool, FontId, LayoutJob)>,
}

pub const CREATE_TEMPLATE: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: example\ndata:\n  key: value\n";

impl YamlTab {
    fn with(title: String, client: Option<Client>, mode: YamlMode, text: String, load: Option<Pending<Res<Value>>>) -> Self {
        YamlTab { title, client, mode, text, load, busy: None, status: None, find: Find::default(), colors: None }
    }

    pub fn edit(ctx: &egui::Context, client: Client, ar: ApiResource, ns: String, name: String) -> Self {
        let load = Pending::spawn(ctx, ops::get(client.clone(), ar.clone(), ns.clone(), name.clone()));
        Self::with(format!("Edit {} {name}", ar.kind), Some(client), YamlMode::Edit { ar, ns, name }, String::new(), Some(load))
    }

    pub fn create(client: Client, kinds: Arc<Vec<Kind>>, ns: String) -> Self {
        Self::with(format!("Create resource ({ns})"), Some(client), YamlMode::Create { kinds, ns }, CREATE_TEMPLATE.into(), None)
    }

    pub fn view(title: String, text: String) -> Self {
        Self::with(title, None, YamlMode::View, text, None)
    }

    fn reload(&mut self, ctx: &egui::Context) {
        if let (Some(c), YamlMode::Edit { ar, ns, name }) = (&self.client, &self.mode) {
            self.load = Some(Pending::spawn(ctx, ops::get(c.clone(), ar.clone(), ns.clone(), name.clone())));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if let Some(r) = take(&mut self.load) {
            match r {
                Ok(obj) => self.text = ops::to_yaml(&obj),
                Err(e) => self.status = Some(Err(e)),
            }
        }
        if let Some(r) = take(&mut self.busy) {
            if r.is_ok() {
                self.reload(ui.ctx()); // fresh resourceVersion for the next save
            }
            self.status = Some(r);
        }

        ui.horizontal(|ui| {
            let label = match self.mode {
                YamlMode::Edit { .. } => Some("💾 Save"),
                YamlMode::Create { .. } => Some("▶ Create / Apply"),
                YamlMode::View => None,
            };
            if let (Some(label), Some(client)) = (label, self.client.clone()) {
                let clicked = ui.add_enabled(self.busy.is_none() && self.load.is_none(), egui::Button::new(label)).clicked()
                    || ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
                if clicked {
                    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = Res> + Send>> = match &self.mode {
                        YamlMode::Edit { ar, ns, name } => Box::pin(ops::replace_yaml(client, ar.clone(), ns.clone(), name.clone(), self.text.clone())),
                        YamlMode::Create { kinds, ns } => Box::pin(ops::apply_yaml(client, kinds.clone(), ns.clone(), self.text.clone())),
                        YamlMode::View => unreachable!(),
                    };
                    self.busy = Some(Pending::spawn(ui.ctx(), fut));
                }
            }
            if matches!(self.mode, YamlMode::Edit { .. }) && ui.button("⟳ Reload").on_hover_text("Discard edits, load current object").clicked() {
                self.status = None;
                self.reload(ui.ctx());
            }
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(self.text.clone());
            }
            if self.busy.is_some() || self.load.is_some() {
                crate::ui_kit::spinner(ui);
            }
            match &self.status {
                Some(Ok(m)) => ui.colored_label(GREEN, m),
                Some(Err(e)) => ui.colored_label(RED, e),
                None => ui.label(""),
            };
        });
        self.find.bar(ui, |_| false);
        ui.separator();

        let matcher = self.find.matcher();
        let cur = self.find.current;
        let colors = &mut self.colors;
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            let (dark, font) = (ui.visuals().dark_mode, TextStyle::Monospace.resolve(ui.style()));
            if !colors.as_ref().is_some_and(|(t, d, f, _)| t == buf.as_str() && *d == dark && *f == font) {
                let job = crate::yamlhl::job(buf.as_str(), &font, ui.visuals().text_color(), dark);
                *colors = Some((buf.as_str().to_owned(), dark, font, job));
            }
            let mut job = colors.as_ref().map(|c| c.3.clone()).unwrap_or_default();
            if let Some(m) = &matcher {
                find::overlay(&mut job, &m.ranges(buf.as_str()), Some(cur)); // from `buf`: `self.text` may be a frame stale
            }
            job.wrap.max_width = f32::INFINITY;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        egui::ScrollArea::both().id_salt(("yaml", &self.title)).auto_shrink(false).show(ui, |ui| {
            let out = egui::TextEdit::multiline(&mut self.text)
                .code_editor()
                .interactive(!matches!(self.mode, YamlMode::View))
                .desired_width(f32::INFINITY)
                .desired_rows(30)
                .layouter(&mut layouter)
                .show(ui);
            if let Some(m) = &matcher {
                let hits = m.ranges(&self.text);
                self.find.set_total(hits.len());
                if std::mem::take(&mut self.find.scroll) && let Some(r) = hits.get(self.find.current) {
                    let ci = self.text[..r.start].chars().count();
                    let rect = out.galley.pos_from_cursor(egui::text::CCursor::new(ci)).translate(out.galley_pos.to_vec2()).expand(24.0);
                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                }
            }
        });
    }
}
