//! The lines of a log tab (sorted by time, deduplicated) and the text selection over them.

use super::*;

pub(super) const MAX_LINES: usize = 100_000;

/// Lines this long add up too: a log of 10 KB lines would take a gigabyte at `MAX_LINES`.
pub(super) const MAX_BYTES: usize = 64 << 20;

/// Selected log text: `anchor` (the word or line a double/triple press picked, else a point) on the
/// buffer line `LogBuf::marks[0]`, the moving end `head` on `marks[1]`. Chars of the shown text
/// (timestamp and pod name included), so a copy is what is on screen.
#[derive(Clone, Copy, Debug)]
pub(super) struct Sel {
    pub(super) anchor: (usize, usize),
    pub(super) head: usize,
    /// What a drag extends by: 1 chars, 2 words, 3 lines.
    pub(super) unit: u32,
}

impl Sel {
    /// Start and end as (line, char), in order.
    pub(super) fn range(&self, [a, h]: [usize; 2]) -> ((usize, usize), (usize, usize)) {
        let head = (h, self.head);
        if head >= (a, self.anchor.0) { ((a, self.anchor.0), head.max((a, self.anchor.1))) } else { (head, (a, self.anchor.1)) }
    }
}

/// The token around char `i` (a double click): letters, digits and `._-:/=@%+~`, as in names, IPs,
/// images and paths, without a `.` or `:` at its end; else the one char there.
pub(super) fn word_at(text: &str, i: usize) -> (usize, usize) {
    let c: Vec<char> = text.chars().collect();
    let on = |k: usize| c.get(k).is_some_and(|&ch| ch.is_alphanumeric() || "._-:/=@%+~".contains(ch));
    let at = match i {
        _ if on(i) => i,
        _ if i > 0 && on(i - 1) => i - 1,
        _ => return (i.min(c.len()), (i + 1).min(c.len())),
    };
    let (mut a, mut b) = (at, at + 1);
    while a > 0 && on(a - 1) {
        a -= 1;
    }
    while on(b) {
        b += 1;
    }
    while b > at + 1 && ".:".contains(c[b - 1]) {
        b -= 1;
    }
    (a, b)
}

#[derive(Default)]
pub(super) struct LogBuf {
    pub(super) lines: VecDeque<Line>,
    /// What `lines` take (`Line::bytes`).
    pub(super) bytes: usize,
    /// The oldest lines were dropped to stay within `MAX_LINES` and `MAX_BYTES`.
    pub(super) trimmed: bool,
    pub(super) rev: u64,
    pub(super) max_chars: u32,
    pub(super) ended: usize,
    pub(super) errors: Vec<String>,
    /// Lines that came in after a stream's first batch (its last lines or range): "N new lines".
    pub(super) appended: usize,
    /// The lines of the selection's ends (`Sel`), kept on their lines as lines come in or go.
    pub(super) marks: [usize; 2],
}

impl LogBuf {
    /// `fresh`: a line that just happened (not the first batch of a stream, not an earlier one).
    pub(super) fn insert(&mut self, l: Line, fresh: bool) {
        self.max_chars = self.max_chars.max(l.chars);
        // Sorted by time so several pods interleave; lines without a timestamp stay where they arrive.
        let pos = match l.ts {
            Some(t) if self.lines.back().is_some_and(|b| b.ts.is_some_and(|bt| bt > t)) => self.lines.partition_point(|x| x.ts.is_none_or(|xt| xt <= t)),
            _ => self.lines.len(),
        };
        // A line already here (same nanosecond timestamp, source and text) comes once: a reload or
        // an earlier-lines fetch racing the stream must not show it twice.
        if l.ts.is_some() && self.lines.range(..pos).rev().take_while(|x| x.ts == l.ts).any(|x| x.same(&l)) {
            return;
        }
        if fresh {
            self.appended += 1;
        }
        self.bytes += l.bytes();
        self.lines.insert(pos, l);
        self.marks.iter_mut().filter(|m| **m >= pos).for_each(|m| *m += 1);
        while self.lines.len() > 1 && (self.lines.len() > MAX_LINES || self.bytes > MAX_BYTES) {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.bytes();
            }
            self.trimmed = true;
            self.marks.iter_mut().for_each(|m| *m = m.saturating_sub(1));
        }
        self.rev += 1;
    }
}

/// Lines of `fetched` older than what's loaded: aligned on the first loaded lines (full match,
/// so identical timestamps are safe), falling back to timestamps when there's no overlap.
pub(super) fn older(fetched: &[Line], first: &[&Line]) -> usize {
    if first.is_empty() {
        return fetched.len();
    }
    if let Some(p) = (0..fetched.len()).find(|&p| fetched.len() - p >= first.len() && fetched[p..].iter().zip(first).all(|(a, b)| a.same(b))) {
        return p;
    }
    let t0 = first[0].ts;
    fetched.iter().take_while(|l| matches!((l.ts, t0), (Some(a), Some(b)) if a < b)).count()
}
