//! Find in view (Ctrl+K, Ctrl+F on logs): query state, the find bar, and highlighted text.
use std::cell::RefCell;
use std::ops::Range;

use egui::text::{ByteIndex, LayoutJob, LayoutSection};
use egui::{Align, Color32, FontSelection, Key, RichText, Ui, WidgetText};

use crate::watch::{contains_ci, RED};

#[derive(Clone)]
pub struct Matcher {
    re: Option<regex::Regex>,
    needle: String,
    case: bool,
}

impl Matcher {
    pub fn plain(needle: &str, case: bool) -> Self {
        Matcher { re: None, needle: needle.to_string(), case }
    }

    pub fn regex(pattern: &str, case: bool) -> Result<Self, String> {
        let re = regex::RegexBuilder::new(pattern).case_insensitive(!case).build().map_err(|e| e.to_string())?;
        Ok(Matcher { re: Some(re), needle: String::new(), case })
    }

    /// Non-overlapping byte ranges of every match (always on char boundaries).
    pub fn ranges(&self, hay: &str) -> Vec<Range<usize>> {
        match &self.re {
            Some(re) => re.find_iter(hay).filter(|m| !m.is_empty()).map(|m| m.range()).collect(),
            None if self.case => hay.match_indices(&self.needle).map(|(i, m)| i..i + m.len()).collect(),
            None => {
                let (h, n) = (hay.as_bytes(), self.needle.len());
                let (mut i, mut out) = (0, vec![]);
                while n > 0 && i + n <= h.len() {
                    if h[i..i + n].eq_ignore_ascii_case(self.needle.as_bytes()) {
                        out.push(i..i + n);
                        i += n;
                    } else {
                        i += 1;
                    }
                }
                out
            }
        }
    }

    pub fn hit(&self, hay: &str) -> bool {
        match &self.re {
            Some(re) => re.is_match(hay),
            None if self.case => hay.contains(&self.needle),
            None => contains_ci(hay, &self.needle),
        }
    }
}

#[derive(Default)]
pub struct Find {
    pub open: bool,
    focus: bool,
    pub query: String,
    pub regex: bool,
    pub case: bool,
    /// Current match (0-based) and the total the view counted.
    pub current: usize,
    pub total: usize,
    /// Scroll to the current match this frame.
    pub scroll: bool,
    cache: Option<((String, bool, bool), Result<Matcher, String>)>,
}

impl Find {
    pub fn open(&mut self) {
        self.open = true;
        self.focus = true;
    }

    /// Matcher for the query; `None` when closed, empty or an invalid regex.
    pub fn matcher(&mut self) -> Option<Matcher> {
        if !self.open || self.query.is_empty() {
            return None;
        }
        let key = (self.query.clone(), self.regex, self.case);
        if self.cache.as_ref().is_none_or(|(k, _)| *k != key) {
            let m = if self.regex { Matcher::regex(&self.query, self.case) } else { Ok(Matcher::plain(&self.query, self.case)) };
            self.cache = Some((key, m));
        }
        self.cache.as_ref().and_then(|(_, m)| m.as_ref().ok()).cloned()
    }

    pub fn step(&mut self, forward: bool) {
        if self.total > 0 {
            self.current = if forward { (self.current + 1) % self.total } else { (self.current + self.total - 1) % self.total };
            self.scroll = true;
        }
    }

    pub fn set_total(&mut self, n: usize) {
        self.total = n;
        if self.current >= n {
            self.current = 0;
        }
    }

    /// The find bar. `extra` adds view-specific toggles. Returns true when query/options changed.
    pub fn bar(&mut self, ui: &mut Ui, extra: impl FnOnce(&mut Ui) -> bool) -> bool {
        if !self.open {
            return false;
        }
        let mut changed = false;
        // Wraps instead of widening a narrow side panel.
        ui.horizontal_wrapped(|ui| {
            let w = (ui.available_width() - 250.0).clamp(100.0, 260.0);
            let r = ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("🔍 Find…").desired_width(w));
            if std::mem::take(&mut self.focus) {
                r.request_focus();
            }
            changed |= r.changed();
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    let back = ui.input(|i| i.modifiers.shift);
                    self.step(!back);
                    r.request_focus();
                } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.open = false;
                }
            }
            changed |= ui.toggle_value(&mut self.regex, ".*").on_hover_text("Regular expression").changed();
            changed |= ui.toggle_value(&mut self.case, "Aa").on_hover_text("Match case").changed();
            changed |= extra(ui);
            if let Some((_, Err(e))) = &self.cache {
                ui.colored_label(RED, "invalid regex").on_hover_text(e);
            } else if !self.query.is_empty() {
                ui.label(if self.total == 0 { "No results".to_string() } else { format!("{} / {}", self.current + 1, self.total) });
            }
            if ui.small_button("⏶").on_hover_text("Previous (Shift+Enter)").clicked() {
                self.step(false);
            }
            if ui.small_button("⏷").on_hover_text("Next (Enter)").clicked() {
                self.step(true);
            }
            if ui.small_button("×").on_hover_text("Close (Esc)").clicked() {
                self.open = false;
            }
        });
        if changed {
            self.current = 0;
            self.scroll = true;
        }
        changed
    }
}

pub fn hit_bg(current: bool) -> Color32 {
    if current { Color32::from_rgb(255, 140, 0) } else { Color32::from_rgba_unmultiplied(255, 210, 0, 80) }
}

/// Give `hits` (byte ranges into `job.text`) a highlight background; `cur` is the current hit.
pub fn overlay(job: &mut LayoutJob, hits: &[Range<usize>], cur: Option<usize>) {
    if hits.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(job.sections.len() + 2 * hits.len());
    let mut h = 0;
    for s in std::mem::take(&mut job.sections) {
        let (mut a, b, mut lead) = (s.byte_range.start.0, s.byte_range.end.0, s.leading_space);
        while a < b {
            while h < hits.len() && hits[h].end <= a {
                h += 1;
            }
            let (end, on) = match hits.get(h) {
                Some(r) if r.start <= a => (r.end.min(b), true),
                Some(r) => (r.start.min(b), false),
                None => (b, false),
            };
            let mut format = s.format.clone();
            if on {
                format.background = hit_bg(cur == Some(h));
                if cur == Some(h) {
                    format.color = Color32::BLACK; // readable on the solid orange
                }
            }
            out.push(LayoutSection { leading_space: std::mem::take(&mut lead), byte_range: ByteIndex(a)..ByteIndex(end), format });
            a = end;
        }
    }
    job.sections = out;
}

/// Find state for views that draw everything (details): helpers count matches as they draw.
struct Hl {
    m: Matcher,
    current: usize,
    count: usize,
    scroll: bool,
}

// ponytail: per-thread "currently finding" slot instead of threading a context through every
// details helper; egui draws on one thread and begin/end bracket a single view.
thread_local!(static HL: RefCell<Option<Hl>> = const { RefCell::new(None) });

pub fn begin(find: &mut Find) {
    let m = find.matcher();
    let (current, scroll) = (find.current, find.scroll);
    HL.with(|h| *h.borrow_mut() = m.map(|m| Hl { m, current, count: 0, scroll }));
}

pub fn end(find: &mut Find) {
    if let Some(n) = HL.with(|h| h.borrow_mut().take().map(|h| h.count)) {
        find.set_total(n);
    }
    find.scroll = false;
}

/// Hits in `text` for the active find, plus which of them is current; bumps the match count.
fn claim(text: &str) -> Option<(Vec<Range<usize>>, Option<usize>, bool)> {
    HL.with(|h| {
        let mut h = h.borrow_mut();
        let h = h.as_mut()?;
        let hits = h.m.ranges(text);
        if hits.is_empty() {
            return None;
        }
        let first = h.count;
        h.count += hits.len();
        let cur = (h.current >= first && h.current < h.count).then(|| h.current - first);
        Some((hits, cur, h.scroll && cur.is_some()))
    })
}

fn show(ui: &mut Ui, rt: RichText, link: bool, wrap: bool) -> egui::Response {
    let label = |w: WidgetText| if wrap { egui::Label::new(w).wrap() } else { egui::Label::new(w) };
    let text = rt.text().to_owned();
    let Some((hits, cur, scroll)) = claim(&text) else {
        return if link { ui.link(rt) } else { ui.add(label(rt.into())) };
    };
    let mut job = (*WidgetText::from(rt).into_layout_job(ui.style(), FontSelection::Default, Align::Center)).clone();
    if link {
        job.sections.iter_mut().for_each(|s| s.format.color = ui.visuals().hyperlink_color);
    }
    overlay(&mut job, &hits, cur);
    let r = if link { ui.add(egui::Link::new(job)) } else { ui.add(label(job.into())) };
    if scroll {
        ui.scroll_to_rect(r.rect, Some(Align::Center));
    }
    r
}

/// A label that takes part in find-in-view.
pub fn label(ui: &mut Ui, text: impl Into<RichText>) -> egui::Response {
    show(ui, text.into(), false, false)
}

/// Same, wrapping long text (values, messages).
pub fn wrapped(ui: &mut Ui, text: impl Into<RichText>) -> egui::Response {
    show(ui, text.into(), false, true)
}

/// A link that takes part in find-in-view.
pub fn link(ui: &mut Ui, text: impl Into<RichText>) -> egui::Response {
    show(ui, text.into(), true, false)
}

/// True when the active find matches `text` (built only while finding); counts nothing.
/// Used to open collapsed sections that hold matches.
pub fn matches(text: impl FnOnce() -> String) -> bool {
    HL.with(|h| h.borrow().as_ref().is_some_and(|h| h.m.hit(&text())))
}

/// A hidden value (secret): its matches count and are flagged, the value stays masked.
pub fn masked(ui: &mut Ui, text: &str) -> egui::Response {
    let dots = RichText::new("•".repeat(12)).monospace();
    let Some((hits, cur, scroll)) = claim(text) else { return ui.label(dots) };
    ui.horizontal(|ui| {
        ui.label(dots);
        let n = hits.len();
        let tag = RichText::new(format!(" {n} hidden match{}, Show to see ", if n == 1 { "" } else { "es" })).background_color(hit_bg(cur.is_some()));
        let r = ui.label(if cur.is_some() { tag.color(Color32::BLACK) } else { tag });
        if scroll {
            ui.scroll_to_rect(r.rect, Some(Align::Center));
        }
        r
    })
    .inner
}

/// Highlight a prepared job (e.g. syntax-highlighted YAML) and scroll to its current hit.
pub fn job(ui: &mut Ui, mut job: LayoutJob) -> egui::Response {
    let claimed = claim(&job.text);
    if let Some((hits, cur, _)) = &claimed {
        overlay(&mut job, hits, *cur);
    }
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let r = ui.add(egui::Label::new(galley.clone()).selectable(true));
    if let Some((hits, Some(c), true)) = claimed {
        let ci = galley.job.text[..hits[c].start].chars().count();
        let rect = galley.pos_from_cursor(egui::text::CCursor::new(ci)).translate(r.rect.min.to_vec2()).expand(24.0);
        ui.scroll_to_rect(rect, Some(Align::Center));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_plain_ci_case_and_regex() {
        let m = Matcher::plain("err", false);
        assert_eq!(m.ranges("Error: err ERR"), vec![0..3, 7..10, 11..14]);
        assert_eq!(Matcher::plain("err", true).ranges("Error: err"), vec![7..10]);
        let mut f = Find { open: true, query: r"\d+ms".into(), regex: true, ..Default::default() };
        assert_eq!(f.matcher().unwrap().ranges("took 15ms and 7ms"), vec![5..9, 14..17]);
        f.query = "(".into();
        assert!(f.matcher().is_none());
        assert!(Matcher::plain("é", false).ranges("café é").iter().all(|r| "café é".is_char_boundary(r.start)));
    }

    #[test]
    fn overlay_splits_sections() {
        let mut job = LayoutJob::simple("hello world".into(), egui::FontId::monospace(12.0), Color32::WHITE, f32::INFINITY);
        overlay(&mut job, &[6..11], Some(0));
        assert_eq!(job.sections.len(), 2);
        assert_eq!(job.sections[1].byte_range.start.0, 6);
        assert_eq!(job.sections[1].format.background, hit_bg(true));
    }
}
