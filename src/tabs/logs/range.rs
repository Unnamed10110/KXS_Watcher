//! The "Logs by date" dialog and the download of a range to a file.

use super::*;

/// The date-range dialog, filled from the tab's current range and find text.
pub(super) struct RangeForm {
    pub(super) from: String,
    pub(super) to: String,
    pub(super) query: String,
    pub(super) regex: bool,
    pub(super) only: bool,
}

impl LogTab {
    /// The range as timestamps, relative ones counted from now.
    pub(super) fn bounds(&self) -> (Option<jiff::Timestamp>, Option<jiff::Timestamp>) {
        let now = jiff::Zoned::now();
        let at = |s: &str| parse_since(s, &now).ok().flatten();
        (at(&self.range.0), at(&self.range.1))
    }

    pub(super) fn range_label(&self) -> String {
        match (self.range.0.as_str(), self.range.1.as_str()) {
            ("", "") => format!("Last {TAIL} lines"),
            (f, "") => format!("Since {f}"),
            ("", t) => format!("Until {t}"),
            (f, t) => format!("{f} → {t}"),
        }
    }

    /// Save dialog, then every line of the shown pods/containers in the range (all of it when
    /// open-ended), written as it streams in.
    pub(super) fn start_download(&mut self, ctx: &egui::Context, bounds: (Option<jiff::Timestamp>, Option<jiff::Timestamp>), only: Option<Matcher>) {
        let name = match self.pods.as_slice() {
            [p] => format!("{}.log", p.name), // pod names are safe file names
            _ => "logs.log".into(),
        };
        let dialog = rfd::AsyncFileDialog::new().set_title("Download logs").set_file_name(name).add_filter("Log", &["log", "txt"]);
        let progress = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        self.progress = Some(progress.clone());
        let (client, srcs, previous, keep_ts, tz, c) = (self.client.clone(), self.sources.clone(), self.previous, self.show_ts, self.tz.clone(), ctx.clone());
        self.saving = Some(Pending::spawn(ctx, async move {
            let Some(f) = dialog.save_file().await else { return Ok(None) };
            download(client, srcs, previous, bounds, keep_ts, tz, only, f.path().to_path_buf(), progress, c).await.map(Some)
        }));
    }

    /// The date-range dialog; true when the log has to restart.
    pub(super) fn range_modal(&mut self, ctx: &egui::Context) -> bool {
        let Some(f) = &mut self.range_form else { return false };
        let now = jiff::Zoned::now();
        let (from, to) = (parse_since(&f.from, &now), parse_since(&f.to, &now));
        let backwards = matches!((&from, &to), (Ok(Some(a)), Ok(Some(b))) if a >= b);
        let bad_regex = f.regex && !f.query.is_empty() && Matcher::regex(&f.query, false).is_err();
        let ok = from.is_ok() && to.is_ok() && !backwards && !bad_regex;
        #[derive(PartialEq)]
        enum Do {
            Show,
            Download,
            Clear,
            Cancel,
        }
        let mut act = None;
        let busy = self.saving.is_some();
        let r = egui::Modal::new(egui::Id::new("log-range")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Logs by date");
            ui.label(RichText::new("Local time, as shown with Timestamps (UTC if it ends in Z). 2h, 30m, 1d: that long ago.").weak());
            ui.add_space(10.0);
            let from_empty = if f.to.trim().is_empty() { "the last 500 lines" } else { "the start of the log" };
            for (label, text, parsed, empty) in [("From", &mut f.from, &from, from_empty), ("To", &mut f.to, &to, "now, and keep following")] {
                ui.horizontal(|ui| {
                    ui.add_sized([40.0, 20.0], egui::Label::new(label));
                    ui.add(egui::TextEdit::singleline(text).hint_text("2026-09-28 14:30 · 14:30 · 2h").desired_width(220.0));
                    match parsed {
                        Ok(None) => ui.label(RichText::new(empty).weak()),
                        Ok(Some(t)) => ui.label(t.to_zoned(jiff::tz::TimeZone::system()).strftime("%a %d %b %Y, %H:%M:%S").to_string()),
                        Err(_) => ui.colored_label(RED, "not a date or time"),
                    };
                });
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_sized([40.0, 20.0], egui::Label::new(RichText::new("Last").weak()));
                for (label, ago) in [("15 min", "15m"), ("1 hour", "1h"), ("6 hours", "6h"), ("24 hours", "24h")] {
                    if ui.small_button(label).clicked() {
                        (f.from, f.to) = (ago.into(), String::new());
                    }
                }
                if ui.small_button("Today").clicked() {
                    (f.from, f.to) = (now.date().to_string(), String::new());
                }
                if ui.small_button("Yesterday").clicked() {
                    let yesterday = now.date().yesterday().unwrap_or(now.date());
                    (f.from, f.to) = (yesterday.to_string(), now.date().to_string());
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_sized([40.0, 20.0], egui::Label::new("Text"));
                ui.add(egui::TextEdit::singleline(&mut f.query).hint_text("optional: find in these lines").desired_width(220.0));
                ui.toggle_value(&mut f.regex, ".*").on_hover_text("Regular expression");
                ui.add_enabled(!f.query.is_empty(), egui::Checkbox::new(&mut f.only, "Only matching lines"));
            });
            if backwards {
                ui.colored_label(RED, "“To” has to be after “From”.");
            }
            if bad_regex {
                ui.colored_label(RED, "Not a valid regular expression.");
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Clear").on_hover_text("Back to the last 500 lines, following").clicked() {
                    act = Some(Do::Clear);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(ok, egui::Button::new(RichText::new("Show").strong())).on_hover_text("Enter").clicked() || (ok && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        act = Some(Do::Show);
                    }
                    let tip = "Every line of this range to a file (only the matching ones with \"Only matching lines\"), however many";
                    if ui.add_enabled(ok && !busy, egui::Button::new("⬇ Download…")).on_hover_text(tip).clicked() {
                        act = Some(Do::Download);
                    }
                    if ui.button("Cancel").clicked() {
                        act = Some(Do::Cancel);
                    }
                });
            });
        });
        if r.should_close() && act.is_none() {
            act = Some(Do::Cancel);
        }
        let Some(act) = act else { return false };
        let Some(f) = self.range_form.take() else { return false };
        match act {
            Do::Cancel => false,
            Do::Clear => {
                self.range = Default::default();
                true
            }
            Do::Show => {
                self.range = (f.from.trim().to_string(), f.to.trim().to_string());
                if !f.query.is_empty() {
                    (self.find.query, self.find.regex, self.find.open) = (f.query, f.regex, true);
                }
                self.only_matching = f.only && !self.find.query.is_empty();
                true
            }
            Do::Download => {
                let only = (f.only && !f.query.is_empty()).then(|| if f.regex { Matcher::regex(&f.query, false).ok() } else { Some(Matcher::plain(&f.query, false)) }).flatten();
                let (from, to) = (from.ok().flatten(), to.ok().flatten());
                self.start_download(ctx, (from, to), only);
                false
            }
        }
    }
}
