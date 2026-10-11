//! A log tab: its state, toolbar, and the plumbing between the streams and the view.

use super::*;

/// A log tab as saved at exit: what it shows and how.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LogState {
    /// (group, kind, namespace, name) of each object whose pods are shown.
    pub targets: Vec<(String, String, String, String)>,
    pub container: String,
    pub all_containers: bool,
    pub previous: bool,
    pub show_ts: bool,
    pub show_src: bool,
    pub wrap: bool,
    pub follow: bool,
    /// Date range: (from, to) as typed.
    pub range: (String, String),
}

pub struct LogTab {
    pub title: String,
    pub(super) client: Client,
    /// What the tab was opened for, for `state`.
    pub(super) targets: Vec<(String, String, String, String)>,
    pub(super) init: Option<Pending<Res<Vec<PodInfo>>>>,
    pub(super) pods: Vec<PodInfo>,
    /// Chosen container for a single pod.
    pub(super) container: String,
    pub(super) all_containers: bool,
    pub(super) sources: Vec<Source>,
    pub(super) previous: bool,
    /// Date range shown: (from, to) as typed, re-read on each start so "2h" stays relative to the
    /// reload; empty = the last lines, following.
    pub(super) range: (String, String),
    pub(super) range_form: Option<RangeForm>,
    /// Save dialog + write (Save…, downloads): the file written, `None` when cancelled.
    pub(super) saving: Option<Pending<Res<Option<PathBuf>>>>,
    /// A running download: bytes written, cancel.
    pub(super) progress: Option<(Arc<AtomicU64>, Arc<AtomicBool>)>,
    /// A message shown in place of the stream status for a while: (text, is an error, when).
    pub(super) note: Option<(String, bool, Instant)>,
    pub(super) show_ts: bool,
    pub(super) show_src: bool,
    pub(super) wrap: bool,
    pub(super) follow: bool,
    pub(super) buf: Arc<Mutex<LogBuf>>,
    /// What the streams send, put into `buf` once a frame (`poll`).
    pub(super) inbox: Inbox,
    /// Marked on every frame the tab is drawn: the streams wake the window only while it is.
    pub(super) shown: crate::repaint::Shown,
    pub(super) epoch: u64,
    pub(super) _streams: Vec<Bg>,
    pub(super) earlier: Option<Pending<Res<Vec<(Vec<Line>, bool)>>>>,
    pub(super) exhausted: bool,
    /// The system time zone, for the timestamps (read when the tab opens).
    pub(super) tz: jiff::tz::TimeZone,
    pub(super) anchor: Option<(Option<jiff::Timestamp>, u16, String)>,
    pub find: Find,
    pub(super) only_matching: bool,
    /// Buffer indices shown (`None` = all) and per-line match counts, rebuilt when the key changes.
    pub(super) view: Option<Vec<u32>>,
    pub(super) hits: Vec<(u32, u32)>,
    pub(super) scan_key: (u64, u64, String, bool, bool, bool),
    pub(super) scanned: Instant,
    pub(super) rows: Vec<u32>,
    pub(super) rows_key: (u64, u64, usize, bool, bool, usize),
    pub(super) set_offset: Option<f32>,
    /// The selected text and what drives it (keys, pointer).
    pub(super) select: Select,
    pub(super) last_off: f32,
    pub(super) view_h: f32,
    /// Buffer length when the view left the bottom: the lines after it are new ("Ir al final").
    pub(super) away_from: Option<usize>,
    pub(super) status: String,
}

impl LogTab {
    /// Logs of pods, or of the pods behind workloads, interleaved by time.
    pub fn new(ctx: &egui::Context, client: Client, targets: Vec<(Kind, String, String)>, container: Option<String>) -> Self {
        let title = match targets.as_slice() {
            [(_, _, name)] => format!("Logs {name}"),
            t => format!("Logs ({} objects)", t.len()),
        };
        let saved = targets.iter().map(|(k, ns, n)| (k.ar.group.clone(), k.ar.kind.clone(), ns.clone(), n.clone())).collect();
        let shown = crate::repaint::Shown::default();
        LogTab {
            title,
            targets: saved,
            init: Some(Pending::spawn(ctx, pod_infos(client.clone(), targets))),
            client,
            pods: vec![],
            container: container.unwrap_or_default(),
            all_containers: false,
            sources: vec![],
            previous: false,
            range: Default::default(),
            range_form: None,
            saving: None,
            progress: None,
            note: None,
            show_ts: true,
            show_src: true,
            wrap: false,
            follow: true,
            buf: Default::default(),
            inbox: feed(ctx, &shown).1, // `start` replaces it
            shown,
            epoch: 0,
            _streams: vec![],
            earlier: None,
            exhausted: false,
            tz: jiff::tz::TimeZone::system(),
            anchor: None,
            find: Find::default(),
            only_matching: false,
            view: None,
            hits: vec![],
            scan_key: Default::default(),
            scanned: Instant::now(),
            rows: vec![],
            rows_key: Default::default(),
            set_offset: None,
            select: Select::default(),
            last_off: 0.0,
            view_h: 0.0,
            away_from: None,
            status: "resolving pods…".into(),
        }
    }

    pub(super) fn start(&mut self, ctx: &egui::Context) {
        let single = self.pods.len() == 1;
        self.sources = self
            .pods
            .iter()
            .flat_map(|p| {
                let chosen = if single && !self.container.is_empty() { self.container.clone() } else { p.default.clone() };
                let cs: Vec<String> = if self.all_containers { p.containers.clone() } else { vec![chosen] };
                let many = cs.len() > 1;
                cs.into_iter().map(move |c| Source { ns: p.ns.clone(), pod: p.name.clone(), tag: if many { format!("{}/{c}", p.name) } else { p.name.clone() }, container: c })
            })
            .collect();
        self.buf = Default::default();
        let (feed, inbox) = feed(ctx, &self.shown);
        self.inbox = inbox;
        self.epoch += 1;
        self.exhausted = false;
        self.earlier = None;
        // A fresh view starts at the top: that is not "scrolled to the top" (which loads earlier lines).
        (self.last_off, self.anchor, self.away_from, self.select.sel, self.select.hold) = (0.0, None, None, None, false);
        let (from, to) = self.bounds();
        self.exhausted = to.is_some(); // a closed range: "Earlier" would fetch from the end of the log
        self._streams = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| Bg::spawn(stream(self.client.clone(), s.clone(), i as u16, self.previous, from, to, feed.clone())))
            .collect();
    }

    pub(super) fn load_earlier(&mut self, ctx: &egui::Context) {
        if self.earlier.is_some() || self.exhausted {
            return;
        }
        let b = self.buf.locked();
        if b.lines.is_empty() {
            return; // the streams haven't delivered their last lines yet: nothing to go back from
        }
        if b.trimmed || b.lines.len() as i64 + MORE > MAX_LINES as i64 || b.bytes > MAX_BYTES / 4 * 3 {
            self.exhausted = true;
            return;
        }
        let srcs = (0..self.sources.len() as u16)
            .map(|i| {
                let mine: Vec<&Line> = b.lines.iter().filter(|l| l.src == i).collect();
                let first = mine.iter().take(20).map(|l| Line { ts: l.ts, src: l.src, text: l.text.clone(), spans: vec![], chars: l.chars }).collect();
                (i, self.sources[i as usize].clone(), mine.len(), first)
            })
            .collect();
        self.anchor = b.lines.front().map(|l| (l.ts, l.src, l.text.clone()));
        drop(b);
        self.earlier = Some(Pending::spawn(ctx, fetch_earlier(self.client.clone(), srcs, self.previous)));
    }

    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        if let Some(r) = take(&mut self.init) {
            match r {
                Ok(pods) => {
                    if pods.len() > 1 {
                        self.title = format!("Logs ({} pods)", pods.len());
                    }
                    if self.container.is_empty() {
                        self.container = pods.first().map(|p| p.default.clone()).unwrap_or_default();
                    }
                    self.pods = pods;
                    self.start(ctx);
                }
                Err(e) => self.status = e,
            }
        }
        if let Some(r) = take(&mut self.saving) {
            self.progress = None;
            self.note = match r {
                Ok(Some(path)) => Some((format!("Saved to {}", path.display()), false, Instant::now())),
                Ok(None) => None, // cancelled
                Err(e) => Some((format!("Save failed: {e}"), true, Instant::now())),
            };
        }
        if let Some(r) = take(&mut self.earlier) {
            match r {
                Ok(per_src) => {
                    let mut b = self.buf.locked();
                    self.exhausted = per_src.iter().all(|(_, ex)| *ex);
                    for (lines, _) in per_src {
                        for l in lines {
                            b.insert(l, false);
                        }
                    }
                    self.epoch += 1;
                }
                Err(e) => {
                    self.status = e;
                    self.exhausted = true;
                }
            }
        }
        let mut b = self.buf.locked();
        self.inbox.fill(&mut b);
        if self.init.is_none() && !self.pods.is_empty() {
            let live = self.sources.len() - b.ended;
            let fixed = self.previous || !self.range.1.is_empty(); // read once, not followed
            self.status = match (b.errors.last(), live, fixed) {
                (Some(e), _, _) => e.clone(),
                (None, 0, false) => "streams ended".into(),
                (None, 0, true) => "loaded".into(),
                (None, n, _) => format!("following {n} stream{}", if n == 1 { "" } else { "s" }),
            };
            if self.range != Default::default() && b.errors.is_empty() {
                let label = self.range_label();
                self.status += &format!(" · {label}");
            }
            if b.trimmed {
                self.status += &format!(" · the newest {} lines (Download keeps them all)", b.lines.len());
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.shown.mark();
        self.poll(ui.ctx());
        let mut restart = false;
        ui.horizontal_wrapped(|ui| {
            match self.pods.as_slice() {
                [p] if p.containers.len() > 1 => {
                    egui::ComboBox::from_id_salt(("log-container", &p.ns, &p.name)).selected_text(if self.all_containers { "all containers" } else { &self.container }).show_ui(ui, |ui| {
                        restart |= ui.selectable_value(&mut self.all_containers, true, "all containers").changed();
                        for c in &p.containers {
                            if ui.selectable_label(!self.all_containers && self.container == *c, c).clicked() {
                                (self.container, self.all_containers, restart) = (c.clone(), false, true);
                            }
                        }
                    });
                }
                [p] => {
                    ui.label(RichText::new(&p.default).strong());
                }
                ps => {
                    ui.label(RichText::new(format!("{} pods", ps.len())).strong());
                    restart |= ui.checkbox(&mut self.all_containers, "All containers").changed();
                }
            }
            restart |= ui.checkbox(&mut self.previous, "Previous").on_hover_text("Logs of the previous (crashed) container").changed();
            let range = egui::Button::new(format!("📅 {}", self.range_label())).selected(self.range != Default::default());
            if ui.add(range).on_hover_text("Show or download the logs of a date range, optionally only the lines with a text").clicked() {
                let (from, to) = self.range.clone();
                self.range_form = Some(RangeForm { from, to, query: self.find.query.clone(), regex: self.find.regex, only: self.only_matching });
            }
            ui.checkbox(&mut self.show_ts, "Timestamps");
            ui.checkbox(&mut self.show_src, "Pod name");
            ui.checkbox(&mut self.wrap, "Wrap");
            if ui.checkbox(&mut self.follow, "Follow").changed() && self.follow {
                self.set_offset = Some(f32::MAX);
            }
            if ui.add_enabled(!self.exhausted && self.earlier.is_none(), egui::Button::new("↑ Earlier")).on_hover_text("Load earlier lines (also when scrolling to the top)").clicked() {
                self.load_earlier(ui.ctx());
            }
            if ui.button("🔍 Find").on_hover_text("Ctrl+F").clicked() {
                self.find.open();
            }
            if ui.button("Copy").on_hover_text("Copy the shown lines").clicked() {
                ui.ctx().copy_text(self.visible_text());
            }
            if ui.add_enabled(self.saving.is_none(), egui::Button::new("Save…")).on_hover_text("Save the shown lines to a file").clicked() {
                let text = self.visible_text();
                let name = match self.pods.as_slice() {
                    [p] => format!("{}.log", p.name), // pod names are safe file names
                    _ => "logs.log".into(),
                };
                let dialog = rfd::AsyncFileDialog::new().set_title("Save logs").set_file_name(name).add_filter("Log", &["log", "txt"]);
                self.saving = Some(Pending::spawn(ui.ctx(), async move {
                    let Some(f) = dialog.save_file().await else { return Ok(None) };
                    let path = f.path().to_path_buf();
                    std::fs::write(&path, text).map(|_| Some(path)).map_err(|e| e.to_string())
                }));
            }
            match &self.progress {
                Some((_, stop)) => {
                    if ui.button("✖ Cancel download").clicked() {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                None => {
                    let tip = "Every line the pod still has, not only the loaded ones: its whole log (the previous container's with Previous), timestamps with Timestamps";
                    if ui.add_enabled(self.saving.is_none() && !self.sources.is_empty(), egui::Button::new("⬇ Download all…")).on_hover_text(tip).clicked() {
                        self.start_download(ui.ctx(), (None, None), None);
                    }
                }
            }
            restart |= ui.button("⟳ Reload").clicked();
            ui.separator();
            if ui.button("⬆ Start").on_hover_text("First line (Ctrl+Home)").clicked() {
                self.jump_to_start();
            }
            if ui.button("⬇ End").on_hover_text("Last line, and follow (Ctrl+End)").clicked() {
                self.jump_to_end();
            }
            if self.earlier.is_some() || self.init.is_some() {
                crate::ui_kit::spinner(ui);
            }
            // One line, truncated: a long error must not push the find bar around.
            let written = self.progress.as_ref().map_or(0, |(done, _)| done.load(Ordering::Relaxed));
            let downloading = format!("Downloading… {}", ops::fmt_bytes(written as f64));
            let (text, error) = match &self.note {
                _ if written > 0 => (&downloading, false),
                Some((n, e, at)) if at.elapsed() < Duration::from_secs(8) => (n, *e),
                _ => (&self.status, self.status.contains(':')),
            };
            let color = if error { RED } else { ui.visuals().weak_text_color() };
            ui.add(egui::Label::new(RichText::new(text).color(color)).truncate()).on_hover_text(text);
        });
        restart |= self.range_modal(ui.ctx());
        if restart && !self.pods.is_empty() {
            self.start(ui.ctx());
        }
        let mut only = self.only_matching;
        self.find.bar(ui, |ui| ui.toggle_value(&mut only, "Only matching").on_hover_text("Hide lines without a match").changed());
        self.only_matching = only;
        ui.separator();
        self.lines_ui(ui);
    }

    pub fn state(&self) -> LogState {
        LogState {
            targets: self.targets.clone(),
            container: self.container.clone(),
            all_containers: self.all_containers,
            previous: self.previous,
            show_ts: self.show_ts,
            show_src: self.show_src,
            wrap: self.wrap,
            follow: self.follow,
            range: self.range.clone(),
        }
    }

    /// Options of a saved tab (before its streams start; the container goes to `new`).
    pub fn restore(&mut self, s: &LogState) {
        (self.all_containers, self.previous, self.show_ts, self.show_src, self.wrap, self.follow) = (s.all_containers, s.previous, s.show_ts, s.show_src, s.wrap, s.follow);
        self.range = s.range.clone();
    }
}
