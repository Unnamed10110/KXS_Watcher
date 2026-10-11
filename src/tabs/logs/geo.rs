//! Where the lines of a log are and how they read: the ones shown (all, or only the matching ones),
//! with Wrap several fixed-width rows per line, and what each line looks like (time, pod name).
//! Positions are (buffer line, char of the line as shown).

use super::*;

/// How a line is shown: with its time and its pod name, or without.
pub(super) struct Display<'a> {
    pub(super) show_ts: bool,
    pub(super) show_src: bool,
    pub(super) tz: &'a jiff::tz::TimeZone,
    pub(super) sources: &'a [Source],
}

impl Display<'_> {
    /// Prefix + text (colors, find highlights) of one line as a single job.
    pub(super) fn job(&self, l: &Line, font: &FontId, text_color: Color32, weak: Color32, hits: &[Range<usize>], cur: Option<usize>) -> LayoutJob {
        let fmt = |c: Color32| TextFormat::simple(font.clone(), c);
        let mut job = LayoutJob::default();
        if self.show_ts {
            let ts = l.ts.map_or_else(|| " ".repeat(TS_WIDTH - 1), |t| fmt_ts(t, self.tz));
            job.append(&format!("{ts:<w$} ", w = TS_WIDTH - 1), 0.0, fmt(weak));
        }
        if self.show_src {
            let tag = self.sources.get(l.src as usize).map_or("?", |s| s.tag.as_str());
            job.append(&format!("[{tag}] "), 0.0, fmt(SRC_COLORS[l.src as usize % SRC_COLORS.len()]));
        }
        let prefix = job.text.len();
        let base = if l.spans.is_empty() { level_color(&l.text).unwrap_or(text_color) } else { text_color };
        let (mut pos, mut cur_col) = (0usize, base);
        for &(at, c) in &l.spans {
            let at = at as usize;
            if at > pos {
                job.append(&l.text[pos..at], 0.0, fmt(cur_col));
            }
            pos = at;
            cur_col = c.unwrap_or(base);
        }
        job.append(&l.text[pos..], 0.0, fmt(cur_col));
        if !hits.is_empty() {
            let shifted: Vec<Range<usize>> = hits.iter().map(|r| r.start + prefix..r.end + prefix).collect();
            find::overlay(&mut job, &shifted, cur);
        }
        job
    }

    /// Chars before the text: the time and the pod name.
    pub(super) fn prefix_chars(&self, l: &Line) -> usize {
        (if self.show_ts { TS_WIDTH } else { 0 }) + if self.show_src { self.sources.get(l.src as usize).map_or(1, |s| s.tag.chars().count()) + 3 } else { 0 }
    }

    /// A line as shown (timestamp and pod name as they are on).
    pub(super) fn text(&self, l: &Line) -> String {
        self.job(l, &FontId::monospace(12.0), Color32::WHITE, Color32::GRAY, &[], None).text
    }

    /// Chars of a line as shown.
    pub(super) fn len(&self, l: &Line) -> usize {
        self.prefix_chars(l) + l.chars as usize
    }
}

/// The selected text, lines as shown (only the matching ones with Only matching).
pub(super) fn selected_text(disp: &Display, view: Option<&[u32]>, lines: &VecDeque<Line>, sel: Sel, marks: [usize; 2]) -> String {
    let ((l0, c0), (l1, c1)) = sel.range(marks);
    let shown = |li: usize| view.is_none_or(|v| v.binary_search(&(li as u32)).is_ok());
    (l0..=l1.min(lines.len().saturating_sub(1)))
        .filter(|&li| shown(li))
        .map(|li| {
            let t = disp.text(&lines[li]);
            let (a, e) = (if li == l0 { c0 } else { 0 }, if li == l1 { c1 } else { usize::MAX });
            t.chars().skip(a).take(e.saturating_sub(a)).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines in the buffer, which of them are shown, and the rows they take.
pub(super) struct Geo<'a> {
    pub(super) lines: &'a VecDeque<Line>,
    /// The buffer lines shown, in order (`None`: all of them).
    pub(super) view: Option<&'a [u32]>,
    /// With Wrap: the first row of each shown line, and the total at the end.
    pub(super) rows: &'a [u32],
    pub(super) wrap: bool,
    /// Chars per row with Wrap.
    pub(super) cols: usize,
    /// Lines shown, and rows.
    pub(super) n: usize,
    pub(super) total: usize,
    pub(super) disp: Display<'a>,
}

impl Geo<'_> {
    /// The buffer line of the `vi`th line shown.
    pub(super) fn line_at(&self, vi: usize) -> usize {
        self.view.map_or(vi, |v| v[vi] as usize)
    }

    pub(super) fn first_row(&self, vi: usize) -> usize {
        if self.wrap { self.rows.get(vi).copied().unwrap_or(0) as usize } else { vi }
    }

    /// The rows of the `vi`th line shown.
    pub(super) fn rows_of(&self, vi: usize) -> Range<usize> {
        if self.wrap { self.first_row(vi)..self.first_row(vi + 1).max(self.first_row(vi) + 1) } else { vi..vi + 1 }
    }

    /// Which line shown a buffer line is (or comes next).
    pub(super) fn vi_of(&self, li: usize) -> usize {
        self.view.map_or(li, |v| v.partition_point(|&x| (x as usize) < li)).min(self.n.saturating_sub(1))
    }

    /// The row a position is on.
    pub(super) fn row_of(&self, (li, ch): (usize, usize)) -> usize {
        let rs = self.rows_of(self.vi_of(li));
        (rs.start + ch / self.cols).min(rs.end - 1)
    }

    /// The position at column `col` of row `r`.
    pub(super) fn at_row(&self, r: usize, col: usize) -> (usize, usize) {
        let vi = if self.wrap { self.rows.partition_point(|&p| p as usize <= r).saturating_sub(1) } else { r }.min(self.n - 1);
        let li = self.line_at(vi);
        (li, (r.saturating_sub(self.rows_of(vi).start) * if self.wrap { self.cols } else { 0 } + col).min(self.len(li)))
    }

    /// Chars of a buffer line as shown.
    pub(super) fn len(&self, li: usize) -> usize {
        self.lines.get(li).map_or(0, |l| self.disp.len(l))
    }

    /// A buffer line as shown.
    pub(super) fn text(&self, li: usize) -> String {
        self.disp.text(&self.lines[li])
    }

    pub(super) fn selected(&self, sel: Sel, marks: [usize; 2]) -> String {
        selected_text(&self.disp, self.view, self.lines, sel, marks)
    }
}
