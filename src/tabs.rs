//! Dock tool tabs: pod logs, terminals (local shell / pod exec / drain) and the YAML editor.
use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{Color32, FontId, RichText, TextStyle};
use egui_term::{BackendSettings, FontSettings, PtyEvent, TerminalBackend, TerminalFont, TerminalView};
use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ApiResource, LogParams};
use kube::Client;
use serde_json::Value;

use crate::find::{self, Find, Matcher};
use crate::kubeconfig::{self, Ctx};
use crate::ops::{self, Kind, Res};
use crate::watch::{take, Bg, Pending, GREEN, ORANGE, RED};

// ---------------------------------------------------------------- logs

const MAX_LINES: usize = 100_000;
const TAIL: i64 = 500;
const MORE: i64 = 1000;
const TS_WIDTH: usize = 24; // "2026-09-26 08:10:46.495 "

/// One log line: kubelet timestamp, source (pod/container), text without ANSI codes, color changes.
pub struct Line {
    ts: Option<jiff::Timestamp>,
    src: u16,
    text: String,
    spans: Vec<(u32, Option<Color32>)>,
    chars: u32,
}

impl Line {
    fn same(&self, o: &Line) -> bool {
        self.ts == o.ts && self.src == o.src && self.text == o.text
    }
}

/// A timestamp as shown and exported: local time, `2026-09-26 08:10:46.495`.
fn fmt_ts(t: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> String {
    t.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// A raw log line as written to a downloaded file: with the timestamp as shown (local time), or
/// without it.
fn export_line(raw: &str, keep_ts: bool, tz: &jiff::tz::TimeZone) -> String {
    match (split_ts(raw), keep_ts) {
        (_, false) => split_ts(raw).1.to_owned(),
        ((Some(t), text), true) => format!("{} {text}", fmt_ts(t, tz)),
        ((None, _), true) => raw.to_owned(),
    }
}

/// The RFC3339Nano prefix (always requested) and the rest of a raw line.
fn split_ts(raw: &str) -> (Option<jiff::Timestamp>, &str) {
    match raw.split_once(' ') {
        Some((a, b)) => match a.parse::<jiff::Timestamp>() {
            Ok(t) => (Some(t), b),
            Err(_) => (None, raw),
        },
        None => match raw.parse::<jiff::Timestamp>() {
            Ok(t) => (Some(t), ""),
            Err(_) => (None, raw),
        },
    }
}

/// Split the timestamp and strip ANSI escapes.
fn parse_line(raw: &str, src: u16) -> Line {
    let (ts, rest) = split_ts(raw);
    let (text, spans) = strip_ansi(rest);
    Line { ts, src, chars: text.chars().count() as u32, text, spans }
}

const PALETTE: [Color32; 16] = [
    Color32::from_rgb(118, 118, 118), Color32::from_rgb(205, 49, 49), Color32::from_rgb(13, 160, 100), Color32::from_rgb(190, 160, 0),
    Color32::from_rgb(36, 114, 200), Color32::from_rgb(188, 63, 188), Color32::from_rgb(17, 150, 185), Color32::from_rgb(170, 170, 170),
    Color32::from_rgb(130, 130, 130), Color32::from_rgb(241, 76, 76), Color32::from_rgb(35, 190, 120), Color32::from_rgb(210, 180, 0),
    Color32::from_rgb(59, 142, 234), Color32::from_rgb(214, 112, 214), Color32::from_rgb(41, 184, 219), Color32::from_rgb(200, 200, 200),
];

fn xterm256(n: u8) -> Color32 {
    match n {
        0..=15 => PALETTE[n as usize],
        16..=231 => {
            let n = n - 16;
            let lvl = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(lvl(n / 36), lvl((n / 6) % 6), lvl(n % 6))
        }
        _ => Color32::from_gray(8 + (n - 232) * 10),
    }
}

/// Apply one SGR parameter list to the current foreground.
fn sgr(params: &str, mut fg: Option<Color32>) -> Option<Color32> {
    let p: Vec<u16> = params.split(';').map(|x| x.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            0 | 39 => fg = None,
            n @ 30..=37 => fg = Some(PALETTE[(n - 30) as usize]),
            n @ 90..=97 => fg = Some(PALETTE[(n - 90 + 8) as usize]),
            38 if p.get(i + 1) == Some(&5) => {
                fg = p.get(i + 2).map(|&n| xterm256(n as u8));
                i += 2;
            }
            38 if p.get(i + 1) == Some(&2) => {
                let c = |k: usize| p.get(i + k).copied().unwrap_or(0) as u8;
                fg = Some(Color32::from_rgb(c(2), c(3), c(4)));
                i += 4;
            }
            _ => {}
        }
        i += 1;
    }
    fg
}

/// Text without escape sequences (tabs expanded) plus the byte offsets where the color changes.
fn strip_ansi(s: &str) -> (String, Vec<(u32, Option<Color32>)>) {
    if !s.bytes().any(|b| b == 0x1b || b == b'\t' || b == b'\r') {
        return (s.to_string(), vec![]);
    }
    let (mut out, mut spans, mut fg) = (String::with_capacity(s.len()), vec![], None);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\t' => out.push_str("    "),
            '\x1b' => match it.next() {
                Some('[') => {
                    let (mut params, mut fin) = (String::new(), None);
                    for n in it.by_ref() {
                        if ('@'..='~').contains(&n) {
                            fin = Some(n);
                            break;
                        }
                        params.push(n);
                    }
                    if fin == Some('m') {
                        let new = sgr(&params, fg);
                        if new != fg {
                            fg = new;
                            let at = out.len() as u32;
                            match spans.last_mut() {
                                Some((pos, col)) if *pos == at => *col = fg,
                                _ => spans.push((at, fg)),
                            }
                        }
                    }
                }
                Some(']') => while let Some(n) = it.next() {
                    if n == '\x07' || (n == '\x1b' && it.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                },
                _ => {}
            },
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    (out, spans)
}

/// Color for lines without ANSI codes, from their log level.
fn level_color(text: &str) -> Option<Color32> {
    let end = text.char_indices().nth(160).map_or(text.len(), |(i, _)| i);
    let head = &text[..end];
    let b = head.as_bytes();
    // klog: "E0926 11:10:46.123 ..."
    if b.len() > 5 && b[1..5].iter().all(u8::is_ascii_digit) {
        match b[0] {
            b'E' | b'F' => return Some(RED),
            b'W' => return Some(ORANGE),
            _ => {}
        }
    }
    let has = |ws: &[&str]| ws.iter().any(|w| head.contains(w));
    if has(&["ERROR", "Error", "error:", "FATAL", "PANIC", "CRITICAL", "level=error", "level=fatal", "\"level\":\"error\"", "\"level\":\"fatal\"", "Exception", "Traceback"]) {
        Some(RED)
    } else if has(&["WARN", "level=warn", "\"level\":\"warn"]) {
        Some(ORANGE)
    } else {
        None
    }
}

/// Lines of `fetched` older than what's loaded: aligned on the first loaded lines (full match,
/// so identical timestamps are safe), falling back to timestamps when there's no overlap.
fn older(fetched: &[Line], first: &[&Line]) -> usize {
    if first.is_empty() {
        return fetched.len();
    }
    if let Some(p) = (0..fetched.len()).find(|&p| fetched.len() - p >= first.len() && fetched[p..].iter().zip(first).all(|(a, b)| a.same(b))) {
        return p;
    }
    let t0 = first[0].ts;
    fetched.iter().take_while(|l| matches!((l.ts, t0), (Some(a), Some(b)) if a < b)).count()
}

/// Byte offset of the `n`th char (or the end).
fn char_byte(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

fn slice_job(job: &LayoutJob, a: usize, b: usize) -> LayoutJob {
    let mut out = LayoutJob { text: job.text[a..b].to_string(), ..Default::default() };
    for s in &job.sections {
        let (sa, sb) = (s.byte_range.start.0.max(a), s.byte_range.end.0.min(b));
        if sa < sb {
            out.sections.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(sa - a)..ByteIndex(sb - a), format: s.format.clone() });
        }
    }
    out
}

/// Selected log text: `anchor` (the word or line a double/triple press picked, else a point) on the
/// buffer line `LogBuf::marks[0]`, the moving end `head` on `marks[1]`. Chars of the shown text
/// (timestamp and pod name included), so a copy is what is on screen.
#[derive(Clone, Copy, Debug)]
struct Sel {
    anchor: (usize, usize),
    head: usize,
    /// What a drag extends by: 1 chars, 2 words, 3 lines.
    unit: u32,
}

impl Sel {
    /// Start and end as (line, char), in order.
    fn range(&self, [a, h]: [usize; 2]) -> ((usize, usize), (usize, usize)) {
        let head = (h, self.head);
        if head >= (a, self.anchor.0) { ((a, self.anchor.0), head.max((a, self.anchor.1))) } else { (head, (a, self.anchor.1)) }
    }
}

/// The token around char `i` (a double click): letters, digits and `._-:/=@%+~`, as in names, IPs,
/// images and paths, without a `.` or `:` at its end; else the one char there.
fn word_at(text: &str, i: usize) -> (usize, usize) {
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

/// A log row on screen this frame: its line, chars `c0..c1` of it, where, and how it was laid out.
struct Drawn {
    li: usize,
    c0: usize,
    c1: usize,
    rect: egui::Rect,
    galley: Arc<egui::Galley>,
}

#[derive(Default)]
struct LogBuf {
    lines: VecDeque<Line>,
    rev: u64,
    max_chars: u32,
    ended: usize,
    errors: Vec<String>,
    /// Lines that came in after a stream's first batch (its last lines or range): "N new lines".
    appended: usize,
    /// The lines of the selection's ends (`Sel`), kept on their lines as lines come in or go.
    marks: [usize; 2],
}

impl LogBuf {
    /// `fresh`: a line that just happened (not the first batch of a stream, not an earlier one).
    fn insert(&mut self, l: Line, fresh: bool) {
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
        self.lines.insert(pos, l);
        self.marks.iter_mut().filter(|m| **m >= pos).for_each(|m| *m += 1);
        if self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.marks.iter_mut().for_each(|m| *m = m.saturating_sub(1));
        }
        self.rev += 1;
    }
}

#[derive(Clone)]
struct PodInfo {
    ns: String,
    name: String,
    containers: Vec<String>,
    default: String,
}

#[derive(Clone)]
struct Source {
    ns: String,
    pod: String,
    container: String,
    tag: String,
}

async fn pod_infos(client: Client, targets: Vec<(Kind, String, String)>) -> Res<Vec<PodInfo>> {
    let pods = ops::pods_of(client.clone(), targets).await?;
    let mut out = vec![];
    for (ns, name) in pods {
        let pod = ops::get(client.clone(), ApiResource::erase::<Pod>(&()), ns.clone(), name.clone()).await?;
        let names = |k: &str| pod["spec"][k].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().map(String::from)).collect::<Vec<_>>();
        let (main, init) = (names("containers"), names("initContainers"));
        let default = pod["metadata"]["annotations"]["kubectl.kubernetes.io/default-container"].as_str().map(String::from).or_else(|| main.first().cloned()).unwrap_or_default();
        out.push(PodInfo { ns, name, containers: main.into_iter().chain(init).collect(), default });
    }
    Ok(out)
}

/// Where a log starts: empty for the last lines; a local date and/or time, as the lines show it
/// (`2026-09-28 14:30`, `2026-09-28 14:30:05.250`, `2026-09-28`, `14:30` today); a UTC timestamp
/// (ends in `Z`); or how long ago (`2h`, `30m`, `1d`, `1h 30m`).
fn parse_since(s: &str, now: &jiff::Zoned) -> Result<Option<jiff::Timestamp>, String> {
    use jiff::civil;
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let local = |dt: civil::DateTime| dt.to_zoned(now.time_zone().clone()).map(|z| z.timestamp());
    let t = if let Ok(t) = s.parse::<jiff::Timestamp>() {
        Ok(t)
    } else if let Ok(dt) = s.parse::<civil::DateTime>() {
        local(dt)
    } else if let Ok(t) = s.parse::<civil::Time>() {
        local(now.date().to_datetime(t))
    } else if let Ok(span) = s.parse::<jiff::Span>() {
        now.checked_sub(span).map(|z| z.timestamp())
    } else {
        return Err(format!("Since: can't read \"{s}\" (try 2h, 14:30 or 2026-09-28 14:30)"));
    };
    t.map(Some).map_err(|e| format!("Since: {e}"))
}

/// Follow one container's log. Dropped connections (load balancers cut idle streams) are resumed
/// from the last line's timestamp; the stream only ends when the API says so or the log stays shut.
/// It starts at `from` (every line since then), else with the last `TAIL` lines; a range with an
/// end (`to`) is read once, like a previous container's log.
#[allow(clippy::too_many_arguments)]
async fn stream(client: Client, src: Source, idx: u16, previous: bool, from: Option<jiff::Timestamp>, to: Option<jiff::Timestamp>, buf: Arc<Mutex<LogBuf>>, ctx: egui::Context) {
    let api: Api<Pod> = Api::namespaced(client, &src.ns);
    let fixed = previous || to.is_some();
    let (mut last, mut quiet, mut backoff) = (None::<jiff::Timestamp>, 0, 1);
    let result = loop {
        // sinceTime has whole-second precision: go back a second and drop what we already have.
        // ponytail: a not-yet-seen line with exactly the last timestamp would be skipped on resume.
        let since = last.and_then(|t| t.checked_sub(jiff::SignedDuration::from_secs(1)).ok()).or(from);
        let tail = if last.is_none() && from.is_none() && to.is_none() { Some(TAIL) } else { None };
        // What the first request brings from before it was made isn't new, what it follows is (the
        // node's clock may be off a little: 2 s of slack). A resumed request only brings new lines.
        // ponytail: a node clock off by more than that counts some old lines as new, or the reverse.
        let (resumed, asked) = (last.is_some(), jiff::Timestamp::now() - jiff::SignedDuration::from_secs(2));
        let lp = LogParams { container: Some(src.container.clone()), follow: !fixed, previous, timestamps: true, tail_lines: tail, since_time: since, ..Default::default() };
        let mut got = 0;
        let r: anyhow::Result<()> = async {
            let mut lines = api.log_stream(&src.pod, &lp).await?.lines();
            while let Some(line) = lines.next().await {
                let line = parse_line(&line?, idx);
                if line.ts.zip(to).is_some_and(|(t, end)| t > end) {
                    break; // past the end of the range
                }
                if let (Some(l), Some(t)) = (last, line.ts) {
                    if t <= l {
                        continue;
                    }
                }
                last = line.ts.or(last);
                got += 1;
                let fresh = resumed || line.ts.is_some_and(|t| t >= asked);
                buf.lock().unwrap().insert(line, fresh);
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Ok(())
        }
        .await;
        let api_error = matches!(&r, Err(e) if matches!(e.downcast_ref::<kube::Error>(), Some(kube::Error::Api(_))));
        if fixed || api_error {
            break r; // fixed log, or the pod/container is gone
        }
        match (&r, got) {
            (Ok(()), 0) => quiet += 1,
            _ => quiet = 0,
        }
        if quiet >= 2 {
            break Ok(()); // container stopped: the log closes right away with nothing new
        }
        if got > 0 {
            backoff = 1;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    };
    let mut b = buf.lock().unwrap();
    b.ended += 1;
    if let Err(e) = result {
        b.errors.push(format!("{}: {}", src.tag, ops::err_text(&e)));
    }
    b.rev += 1;
    ctx.request_repaint();
}

/// Writes logs to `path` as they arrive (the whole log can be far bigger than the view): each source
/// in turn, headed `==> pod/container <==` when there are several, from `from` (else the start) to
/// `to` (else now), optionally only the lines `only` matches. `done` counts bytes; `stop` cancels,
/// removing the partial file.
#[allow(clippy::too_many_arguments)]
async fn download(
    client: Client,
    srcs: Vec<Source>,
    previous: bool,
    (from, to): (Option<jiff::Timestamp>, Option<jiff::Timestamp>),
    keep_ts: bool,
    tz: jiff::tz::TimeZone,
    only: Option<Matcher>,
    path: PathBuf,
    (done, stop): (Arc<AtomicU64>, Arc<AtomicBool>),
    ctx: egui::Context,
) -> Res<PathBuf> {
    use std::io::Write;
    let run = async {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
        for (i, s) in srcs.iter().enumerate() {
            if srcs.len() > 1 {
                writeln!(out, "{}==> {} <==", if i > 0 { "\n" } else { "" }, s.tag)?;
            }
            let lp = LogParams { container: Some(s.container.clone()), previous, timestamps: true, since_time: from, ..Default::default() };
            let mut lines = Api::<Pod>::namespaced(client.clone(), &s.ns).log_stream(&s.pod, &lp).await?.lines();
            let mut n = 0u64;
            while let Some(raw) = lines.next().await {
                anyhow::ensure!(!stop.load(Ordering::Relaxed), "cancelled");
                let raw = raw?;
                let (ts, text) = split_ts(&raw);
                if ts.zip(to).is_some_and(|(t, end)| t > end) {
                    break;
                }
                if only.as_ref().is_some_and(|m| !m.hit(&strip_ansi(text).0)) {
                    continue;
                }
                let line = export_line(&raw, keep_ts, &tz);
                writeln!(out, "{line}")?;
                done.fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
                n += 1;
                if n % 2000 == 0 {
                    ctx.request_repaint(); // the byte count
                }
            }
        }
        out.flush()?;
        anyhow::Ok(())
    };
    let r = run.await;
    ctx.request_repaint();
    match r {
        Ok(()) => Ok(path),
        Err(e) => {
            std::fs::remove_file(&path).ok();
            Err(ops::err_text(&e))
        }
    }
}

/// The date-range dialog, filled from the tab's current range and find text.
struct RangeForm {
    from: String,
    to: String,
    query: String,
    regex: bool,
    only: bool,
}

/// Earlier lines for each source: (lines older than what's loaded, source exhausted).
async fn fetch_earlier(client: Client, srcs: Vec<(u16, Source, usize, Vec<Line>)>, previous: bool) -> Res<Vec<(Vec<Line>, bool)>> {
    let mut out = vec![];
    for (idx, src, loaded, first) in srcs {
        let tail = loaded as i64 + MORE;
        let lp = LogParams { container: Some(src.container.clone()), previous, timestamps: true, tail_lines: Some(tail), ..Default::default() };
        let text = Api::<Pod>::namespaced(client.clone(), &src.ns).logs(&src.pod, &lp).await.map_err(|e| e.to_string())?;
        let fetched: Vec<Line> = text.lines().map(|l| parse_line(l, idx)).collect();
        let n = older(&fetched, &first.iter().collect::<Vec<_>>());
        let exhausted = (fetched.len() as i64) < tail;
        out.push((fetched.into_iter().take(n).collect(), exhausted));
    }
    Ok(out)
}

const SRC_COLORS: [Color32; 8] = [
    Color32::from_rgb(86, 156, 214), Color32::from_rgb(197, 134, 192), Color32::from_rgb(78, 201, 176), Color32::from_rgb(220, 160, 90),
    Color32::from_rgb(156, 220, 254), Color32::from_rgb(214, 157, 133), Color32::from_rgb(181, 206, 168), Color32::from_rgb(255, 128, 128),
];

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
    client: Client,
    /// What the tab was opened for, for `state`.
    targets: Vec<(String, String, String, String)>,
    init: Option<Pending<Res<Vec<PodInfo>>>>,
    pods: Vec<PodInfo>,
    /// Chosen container for a single pod.
    container: String,
    all_containers: bool,
    sources: Vec<Source>,
    previous: bool,
    /// Date range shown: (from, to) as typed, re-read on each start so "2h" stays relative to the
    /// reload; empty = the last lines, following.
    range: (String, String),
    range_form: Option<RangeForm>,
    /// Save dialog + write (Save…, downloads): the file written, `None` when cancelled.
    saving: Option<Pending<Res<Option<PathBuf>>>>,
    /// A running download: bytes written, cancel.
    progress: Option<(Arc<AtomicU64>, Arc<AtomicBool>)>,
    /// A message shown in place of the stream status for a while: (text, is an error, when).
    note: Option<(String, bool, Instant)>,
    show_ts: bool,
    show_src: bool,
    wrap: bool,
    follow: bool,
    buf: Arc<Mutex<LogBuf>>,
    epoch: u64,
    _streams: Vec<Bg>,
    earlier: Option<Pending<Res<Vec<(Vec<Line>, bool)>>>>,
    exhausted: bool,
    /// The system time zone, for the timestamps (read when the tab opens).
    tz: jiff::tz::TimeZone,
    anchor: Option<(Option<jiff::Timestamp>, u16, String)>,
    pub find: Find,
    only_matching: bool,
    /// Buffer indices shown (`None` = all) and per-line match counts, rebuilt when the key changes.
    view: Option<Vec<u32>>,
    hits: Vec<(u32, u32)>,
    scan_key: (u64, u64, String, bool, bool, bool),
    scanned: Instant,
    rows: Vec<u32>,
    rows_key: (u64, u64, usize, bool, bool, usize),
    set_offset: Option<f32>,
    sel: Option<Sel>,
    /// A selection was made: a followed log holds still (until End) so it doesn't scroll away.
    hold: bool,
    /// The last press was on this log: the keys go here even with the pointer elsewhere.
    focused: bool,
    /// Where a press landed inside the selection, and whether the drag out of the window was asked for
    /// already: moving on drags the selected text out, letting go without moving collapses the selection
    /// to that point.
    drag_src: Option<(egui::Pos2, bool)>,
    /// Scrolling asked for last frame (a selection dragged past an edge), applied in the next.
    scroll_by: egui::Vec2,
    last_off: f32,
    view_h: f32,
    /// Buffer length when the view left the bottom: the lines after it are new ("Ir al final").
    away_from: Option<usize>,
    status: String,
}

impl LogTab {
    /// Logs of pods, or of the pods behind workloads, interleaved by time.
    pub fn new(ctx: &egui::Context, client: Client, targets: Vec<(Kind, String, String)>, container: Option<String>) -> Self {
        let title = match targets.as_slice() {
            [(_, _, name)] => format!("Logs {name}"),
            t => format!("Logs ({} objects)", t.len()),
        };
        let saved = targets.iter().map(|(k, ns, n)| (k.ar.group.clone(), k.ar.kind.clone(), ns.clone(), n.clone())).collect();
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
            sel: None,
            hold: false,
            focused: false,
            drag_src: None,
            scroll_by: egui::Vec2::ZERO,
            last_off: 0.0,
            view_h: 0.0,
            away_from: None,
            status: "resolving pods…".into(),
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
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
        let buf: Arc<Mutex<LogBuf>> = Default::default();
        self.buf = buf.clone();
        self.epoch += 1;
        self.exhausted = false;
        self.earlier = None;
        // A fresh view starts at the top: that is not "scrolled to the top" (which loads earlier lines).
        (self.last_off, self.anchor, self.away_from, self.sel, self.hold) = (0.0, None, None, None, false);
        let (from, to) = self.bounds();
        self.exhausted = to.is_some(); // a closed range: "Earlier" would fetch from the end of the log
        self._streams = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| Bg::spawn(stream(self.client.clone(), s.clone(), i as u16, self.previous, from, to, buf.clone(), ctx.clone())))
            .collect();
    }

    /// The range as timestamps, relative ones counted from now.
    fn bounds(&self) -> (Option<jiff::Timestamp>, Option<jiff::Timestamp>) {
        let now = jiff::Zoned::now();
        let at = |s: &str| parse_since(s, &now).ok().flatten();
        (at(&self.range.0), at(&self.range.1))
    }

    fn range_label(&self) -> String {
        match (self.range.0.as_str(), self.range.1.as_str()) {
            ("", "") => format!("Last {TAIL} lines"),
            (f, "") => format!("Since {f}"),
            ("", t) => format!("Until {t}"),
            (f, t) => format!("{f} → {t}"),
        }
    }

    /// Save dialog, then every line of the shown pods/containers in the range (all of it when
    /// open-ended), written as it streams in.
    fn start_download(&mut self, ctx: &egui::Context, bounds: (Option<jiff::Timestamp>, Option<jiff::Timestamp>), only: Option<Matcher>) {
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
    fn range_modal(&mut self, ctx: &egui::Context) -> bool {
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
        let f = self.range_form.take().expect("open");
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

    fn load_earlier(&mut self, ctx: &egui::Context) {
        if self.earlier.is_some() || self.exhausted {
            return;
        }
        let b = self.buf.lock().unwrap();
        if b.lines.is_empty() {
            return; // the streams haven't delivered their last lines yet: nothing to go back from
        }
        if b.lines.len() as i64 + MORE > MAX_LINES as i64 {
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

    fn poll(&mut self, ctx: &egui::Context) {
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
                    let mut b = self.buf.lock().unwrap();
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
        let b = self.buf.lock().unwrap();
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
            if b.lines.len() >= MAX_LINES {
                self.status += &format!(" · the newest {MAX_LINES} lines (Download keeps them all)");
            }
        }
    }

    /// Prefix + text (colors, find highlights) of one line as a single job.
    fn line_job(&self, l: &Line, font: &FontId, text_color: Color32, weak: Color32, hits: &[Range<usize>], cur: Option<usize>) -> LayoutJob {
        let fmt = |c: Color32| TextFormat::simple(font.clone(), c);
        let mut job = LayoutJob::default();
        if self.show_ts {
            let ts = l.ts.map_or_else(|| " ".repeat(TS_WIDTH - 1), |t| fmt_ts(t, &self.tz));
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

    fn prefix_chars(&self, l: &Line) -> usize {
        (if self.show_ts { TS_WIDTH } else { 0 }) + if self.show_src { self.sources.get(l.src as usize).map_or(1, |s| s.tag.chars().count()) + 3 } else { 0 }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
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
                self.to_start();
            }
            if ui.button("⬇ End").on_hover_text("Last line, and follow (Ctrl+End)").clicked() {
                self.to_end();
            }
            if self.earlier.is_some() || self.init.is_some() {
                ui.spinner();
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

    fn to_start(&mut self) {
        (self.follow, self.set_offset) = (false, Some(0.0));
    }

    fn to_end(&mut self) {
        (self.follow, self.set_offset, self.away_from, self.hold) = (true, Some(f32::MAX), None, false);
    }

    fn visible_text(&self) -> String {
        let b = self.buf.lock().unwrap();
        let idx: Box<dyn Iterator<Item = usize>> = match &self.view {
            Some(v) => Box::new(v.iter().map(|&i| i as usize)),
            None => Box::new(0..b.lines.len()),
        };
        idx.filter_map(|i| b.lines.get(i)).map(|l| self.shown(l)).collect::<Vec<_>>().join("\n")
    }

    /// A line as shown (timestamp and pod name as they are on).
    fn shown(&self, l: &Line) -> String {
        self.line_job(l, &FontId::monospace(12.0), Color32::WHITE, Color32::GRAY, &[], None).text
    }

    /// The selected text, lines as shown (only the matching ones with Only matching).
    fn selected_text(&self, b: &LogBuf) -> String {
        let Some(((l0, c0), (l1, c1))) = self.sel.map(|s| s.range(b.marks)) else { return String::new() };
        let shown = |li: usize| self.view.as_ref().is_none_or(|v| v.binary_search(&(li as u32)).is_ok());
        (l0..=l1.min(b.lines.len().saturating_sub(1)))
            .filter(|&li| shown(li))
            .map(|li| {
                let t = self.shown(&b.lines[li]);
                let (a, e) = (if li == l0 { c0 } else { 0 }, if li == l1 { c1 } else { usize::MAX });
                t.chars().skip(a).take(e.saturating_sub(a)).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn lines_ui(&mut self, ui: &mut egui::Ui) {
        let buf = self.buf.clone();
        let mut b = buf.lock().unwrap();
        let matcher = self.find.matcher();

        // Match scan: on changes, throttled while lines stream in (regex over 100k lines isn't free).
        let key = (self.epoch, b.rev, self.find.query.clone(), self.find.regex, self.find.case, self.only_matching && matcher.is_some());
        let opts_changed = (&key.2, key.3, key.4, key.5) != (&self.scan_key.2, self.scan_key.3, self.scan_key.4, self.scan_key.5) || key.0 != self.scan_key.0;
        if key != self.scan_key && (opts_changed || self.scanned.elapsed() > Duration::from_millis(250)) {
            let mut total = 0u32;
            self.hits.clear();
            if let Some(m) = &matcher {
                for (i, l) in b.lines.iter().enumerate() {
                    let n = m.ranges(&l.text).len() as u32;
                    if n > 0 {
                        self.hits.push((i as u32, total));
                        total += n;
                    }
                }
            }
            self.view = key.5.then(|| self.hits.iter().map(|h| h.0).collect());
            self.find.set_total(total as usize);
            (self.scan_key, self.scanned) = (key, Instant::now());
        } else if key != self.scan_key {
            ui.ctx().request_repaint_after(Duration::from_millis(260));
        }

        let n = self.view.as_ref().map_or(b.lines.len(), Vec::len);
        let line_at = |vi: usize| -> usize { self.view.as_ref().map_or(vi, |v| v[vi] as usize) };
        let font = TextStyle::Monospace.resolve(ui.style());
        let (row_h, gw) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
        let row_sp = row_h + ui.spacing().item_spacing.y;
        let cols = ((ui.available_width() - 16.0) / gw).max(20.0) as usize;

        // Wrap: long lines become several fixed-width rows, so every row has the same height.
        let rkey = (self.epoch, b.rev, n, self.wrap, self.show_ts, if self.wrap { cols + usize::from(self.show_src) * 100_000 } else { 0 });
        if self.wrap && self.rows_key != rkey {
            self.rows.clear();
            let mut acc = 0u32;
            for vi in 0..n {
                self.rows.push(acc);
                let l = &b.lines[line_at(vi)];
                acc += ((self.prefix_chars(l) + l.chars as usize).div_ceil(cols)).max(1) as u32;
            }
            self.rows.push(acc);
            self.rows_key = rkey;
        }
        let first_row = |vi: usize| if self.wrap { self.rows.get(vi).copied().unwrap_or(0) as usize } else { vi };
        let total = if self.wrap { self.rows.last().copied().unwrap_or(0) as usize } else { n };

        // Keep the first line in place after loading earlier lines.
        if let Some((ts, src, text)) = self.anchor.clone().filter(|_| self.earlier.is_none()) {
            if let Some(vi) = (0..n).find(|&vi| b.lines.get(line_at(vi)).is_some_and(|l| l.ts == ts && l.src == src && l.text == text)) {
                self.set_offset = Some(first_row(vi) as f32 * row_sp);
            }
            self.anchor = None;
        }
        // Jump to the current match.
        if self.find.scroll && self.find.total > 0 {
            let k = self.find.current as u32;
            if let Some(&(line, _)) = self.hits.get(self.hits.partition_point(|h| h.1 <= k).wrapping_sub(1)) {
                let vi = self.view.as_ref().map_or(Some(line as usize), |v| v.iter().position(|&x| x == line));
                if let Some(vi) = vi {
                    self.follow = false;
                    self.set_offset = Some((first_row(vi) as f32 * row_sp - self.view_h / 2.0).max(0.0));
                }
            }
            self.find.scroll = false;
        }

        let (text_color, weak) = (ui.visuals().text_color(), ui.visuals().weak_text_color());
        let area = ui.available_rect_before_wrap();
        // Keys go to the log under the pointer, else to the one clicked last; not from a text field or
        // a terminal on screen that has them.
        let term = TERM_KEYS.with(|t| t.get()).is_some_and(|p| p + 1 >= ui.ctx().cumulative_pass_nr());
        let keys = (ui.rect_contains_pointer(area) || self.focused) && !ui.ctx().text_edit_focused() && !term;
        let (mut sel, mut marks) = (self.sel, b.marks);
        let has_sel = sel.is_some_and(|s| s.range(marks).0 != s.range(marks).1);
        // Positions are (buffer line, char of the shown text); rows are several per line with Wrap.
        let len = |li: usize| {
            b.lines.get(li).map_or(0, |l| {
                (if self.show_ts { TS_WIDTH } else { 0 }) + if self.show_src { self.sources.get(l.src as usize).map_or(1, |s| s.tag.chars().count()) + 3 } else { 0 } + l.chars as usize
            })
        };
        let vi_of = |li: usize| self.view.as_ref().map_or(li, |v| v.partition_point(|&x| (x as usize) < li)).min(n.saturating_sub(1));
        let rows_of = |vi: usize| if self.wrap { first_row(vi)..first_row(vi + 1).max(first_row(vi) + 1) } else { vi..vi + 1 };
        let row_of = |(li, ch): (usize, usize)| {
            let rs = rows_of(vi_of(li));
            (rs.start + ch / cols).min(rs.end - 1)
        };
        let at_row = |r: usize, col: usize| {
            let vi = if self.wrap { self.rows.partition_point(|&p| p as usize <= r).saturating_sub(1) } else { r }.min(n - 1);
            let li = line_at(vi);
            (li, (r.saturating_sub(rows_of(vi).start) * if self.wrap { cols } else { 0 } + col).min(len(li)))
        };

        // Shift with the arrows, Page Up/Down and Home/End (Ctrl: of the whole log) selects, as in an
        // editor; Ctrl+A all, Ctrl+C copies, Escape drops it.
        if keys && n > 0 {
            use egui::{Key, Modifiers as M};
            #[derive(Clone, Copy)]
            enum Mv {
                Rows(isize),
                Char(isize),
                Line(bool),
                Log(bool),
            }
            let page = ((self.view_h / row_sp) as isize - 1).max(1);
            let table = [
                (M::COMMAND | M::SHIFT, Key::Home, Mv::Log(false)),
                (M::COMMAND | M::SHIFT, Key::End, Mv::Log(true)),
                (M::SHIFT, Key::ArrowUp, Mv::Rows(-1)),
                (M::SHIFT, Key::ArrowDown, Mv::Rows(1)),
                (M::SHIFT, Key::PageUp, Mv::Rows(-page)),
                (M::SHIFT, Key::PageDown, Mv::Rows(page)),
                (M::SHIFT, Key::ArrowLeft, Mv::Char(-1)),
                (M::SHIFT, Key::ArrowRight, Mv::Char(1)),
                (M::SHIFT, Key::Home, Mv::Line(false)),
                (M::SHIFT, Key::End, Mv::Line(true)),
            ];
            let (mvs, all, copy, esc) = ui.input_mut(|i| {
                let mvs: Vec<Mv> = table.iter().flat_map(|&(m, k, mv)| std::iter::repeat_n(mv, i.count_and_consume_key(m, k))).collect();
                let copy = has_sel && i.events.iter().any(|e| matches!(e, egui::Event::Copy));
                if copy {
                    i.events.retain(|e| !matches!(e, egui::Event::Copy));
                }
                (mvs, i.consume_key(M::COMMAND, Key::A), copy, has_sel && i.consume_key(M::NONE, Key::Escape))
            });
            if copy {
                ui.ctx().copy_text(self.selected_text(&b));
            }
            if esc {
                (sel, self.hold) = (None, false);
            }
            if all {
                let li = line_at(n - 1);
                (sel, marks, self.hold) = (Some(Sel { anchor: (0, 0), head: len(li), unit: 1 }), [line_at(0), li], true);
            }
            if !mvs.is_empty() {
                // Without a selection it starts at the first line on screen.
                let mut h = match sel {
                    Some(s) => (marks[1], s.head),
                    None => at_row(((self.last_off / row_sp).ceil() as usize).min(total - 1), 0),
                };
                if sel.is_none() {
                    (sel, marks[0]) = (Some(Sel { anchor: (h.1, h.1), head: h.1, unit: 1 }), h.0);
                }
                for mv in mvs {
                    h = match mv {
                        Mv::Rows(d) => {
                            let r = row_of(h);
                            let col = h.1.saturating_sub((r - rows_of(vi_of(h.0)).start) * if self.wrap { cols } else { 0 });
                            at_row(r.saturating_add_signed(d).min(total - 1), col)
                        }
                        Mv::Char(-1) if h.1 == 0 => match vi_of(h.0) {
                            0 => h,
                            vi => (line_at(vi - 1), len(line_at(vi - 1))),
                        },
                        Mv::Char(1) if h.1 >= len(h.0) => match vi_of(h.0) + 1 {
                            vi if vi < n => (line_at(vi), 0),
                            _ => h,
                        },
                        Mv::Char(d) => (h.0, h.1.saturating_add_signed(d).min(len(h.0))),
                        Mv::Line(end) => (h.0, if end { len(h.0) } else { 0 }),
                        Mv::Log(false) => (line_at(0), 0),
                        Mv::Log(true) => (line_at(n - 1), len(line_at(n - 1))),
                    };
                }
                if let Some(s) = &mut sel {
                    (s.head, s.unit, marks[1]) = (h.1, 1, h.0);
                }
                self.hold = true;
                // Keep the moving end on screen.
                let y = row_of(h) as f32 * row_sp;
                if y < self.last_off {
                    self.set_offset = Some(y);
                } else if y + row_sp > self.last_off + self.view_h {
                    self.set_offset = Some(y + row_sp - self.view_h);
                }
            }
        }

        // Under the scroll bars (added later, so they stay on top): presses and drags that select.
        let resp = ui.interact(area, ui.id().with("log-select"), egui::Sense::click_and_drag());
        let mut sa = egui::ScrollArea::new([!self.wrap, true]).id_salt(("logs", &self.title, self.epoch)).auto_shrink(false).stick_to_bottom(self.follow && !self.hold);
        if let Some(y) = self.set_offset.take() {
            // Past the end show_rows draws no rows and the offset turns infinite: a black log for good.
            sa = sa.vertical_scroll_offset(y.min((total as f32 * row_sp - self.view_h).max(0.0)));
        }
        let max_w = (b.max_chars as usize + 60) as f32 * gw;
        let scroll_by = std::mem::take(&mut self.scroll_by);
        let sel_range = sel.map(|s| s.range(marks)).filter(|(a, e)| a != e);
        let sel_color = ui.visuals().selection.bg_fill;
        let mut drawn: Vec<Drawn> = vec![];
        let mut sel_rects: Vec<egui::Rect> = vec![];
        let out = sa.show_rows(ui, row_h, total, |ui, range| {
            // The arrows and Page Up/Down scroll.
            let mut by = scroll_by;
            if keys {
                let page = (self.view_h - row_sp).max(row_sp);
                by += ui.input_mut(|i| {
                    let mut k = |key, by: egui::Vec2| by * i.count_and_consume_key(egui::Modifiers::NONE, key) as f32;
                    k(egui::Key::ArrowUp, egui::vec2(0.0, row_sp)) + k(egui::Key::ArrowDown, egui::vec2(0.0, -row_sp))
                        + k(egui::Key::PageUp, egui::vec2(0.0, page)) + k(egui::Key::PageDown, egui::vec2(0.0, -page))
                        + k(egui::Key::ArrowLeft, egui::vec2(4.0 * gw, 0.0)) + k(egui::Key::ArrowRight, egui::vec2(-4.0 * gw, 0.0))
                });
            }
            if by != egui::Vec2::ZERO {
                ui.scroll_with_delta_animation(by, egui::style::ScrollAnimation::none());
            }
            if !self.wrap {
                ui.set_min_width(max_w);
            }
            let (left, top, first) = (ui.max_rect().left(), ui.max_rect().top(), range.start);
            for r in range {
                let (vi, part) = if self.wrap { (self.rows.partition_point(|&p| p as usize <= r).saturating_sub(1), 0) } else { (r, 0) };
                let part = if self.wrap { r - first_row(vi) } else { part };
                let li = line_at(vi);
                let Some(l) = b.lines.get(li) else { continue };
                let (hits, cur) = match &matcher {
                    Some(m) => {
                        let hits = m.ranges(&l.text);
                        // Global index of this line's first match (prefix sums from the scan).
                        let before = self.hits.get(self.hits.partition_point(|h| (h.0 as usize) < li)).map_or(self.find.total, |h| h.1 as usize);
                        let cur = self.find.current.checked_sub(before).filter(|&c| c < hits.len());
                        (hits, cur)
                    }
                    None => (vec![], None),
                };
                let job = self.line_job(l, &font, text_color, weak, &hits, cur);
                let job = if self.wrap { slice_job(&job, char_byte(&job.text, part * cols), char_byte(&job.text, (part + 1) * cols)) } else { job };
                let chars = len(li);
                let (c0, c1) = if self.wrap { (part * cols, ((part + 1) * cols).min(chars)) } else { (0, chars) };
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                let rect = egui::Rect::from_min_size(egui::pos2(left, top + (r - first) as f32 * row_sp), egui::vec2(galley.size().x, row_h));
                // The selection behind the text; a line selected up to its end shows its line break.
                if let Some(((l0, s0), (l1, e1))) = sel_range
                    && (l0..=l1).contains(&li)
                {
                    let s = if li == l0 { s0 } else { 0 }.max(c0);
                    let (e, eol) = if li == l1 { (e1.min(c1), false) } else { (c1, c1 >= chars) };
                    if s < e || (eol && s <= e) {
                        let x = |c: usize| rect.left() + galley.pos_from_cursor(egui::text::CCursor::new(c - c0)).min.x;
                        let x1 = if eol { x(e) + gw * 0.6 } else { x(e) };
                        let hl = egui::Rect::from_x_y_ranges(x(s)..=x1, rect.y_range());
                        ui.painter().rect_filled(hl, 0.0, sel_color);
                        sel_rects.push(hl);
                    }
                }
                ui.painter().galley(rect.min, galley.clone(), text_color);
                drawn.push(Drawn { li, c0, c1, rect, galley });
            }
        });

        // The pointer selects: a press starts it (a double press takes the word, a triple the line;
        // with Shift it extends), dragging moves the end, and past an edge the log scrolls along.
        if resp.hovered() {
            ui.set_cursor_icon(egui::CursorIcon::Text);
        }
        let (pressed, down, shift, presses, pos, dt) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.modifiers.shift, i.pointer.press_count(), i.pointer.interact_pos(), i.stable_dt.min(0.1)));
        if pressed {
            self.focused = resp.is_pointer_button_down_on();
        }
        // The line and char at a point; rows past the ends count as the first or last one on screen.
        let at_pos = |p: egui::Pos2| {
            let (first, last) = (drawn.first()?, drawn.last()?);
            let y = p.y.clamp(first.rect.top(), last.rect.bottom() - 1.0);
            let d = drawn.iter().find(|d| y < d.rect.bottom()).unwrap_or(last);
            Some((d.li, (d.c0 + d.galley.cursor_from_pos(egui::vec2(p.x - d.rect.left(), row_h / 2.0)).index.0).min(d.c1)))
        };
        // A single press inside the selection keeps it: moving on drags the text out of the window to
        // another program, letting go without moving collapses the selection to that point.
        if let (true, Some(p)) = (pressed && presses == 1 && !shift && resp.is_pointer_button_down_on(), pos) {
            self.drag_src = sel_rects.iter().any(|r| r.contains(p)).then_some((p, false));
        }
        let armed = self.drag_src;
        if let Some((p0, sent)) = armed {
            if !down {
                self.drag_src = None;
                if let Some(at) = at_pos(p0).filter(|_| !sent) {
                    (sel, marks) = (Some(Sel { anchor: (at.1, at.1), head: at.1, unit: 1 }), [at.0; 2]);
                    self.hold = false;
                }
            } else if !sent && ui.input(|i| i.pointer.is_decidedly_dragging()) {
                self.drag_src = Some((p0, true));
                ui.ctx().kxs_request_drag_out(self.selected_text(&b));
            }
        }
        if let (true, Some(p), Some(at)) = (down && resp.is_pointer_button_down_on() && armed.is_none(), pos, pos.and_then(at_pos)) {
            let text = || self.shown(&b.lines[at.0]);
            if pressed && !(shift && sel.is_some()) {
                let anchor = match presses {
                    1 => (at.1, at.1),
                    2 => word_at(&text(), at.1),
                    _ => (0, len(at.0)),
                };
                (sel, marks) = (Some(Sel { anchor, head: anchor.1, unit: presses.min(3) }), [at.0; 2]);
            } else if let Some(s) = &mut sel {
                let forward = at >= (marks[0], s.anchor.0);
                s.head = match s.unit {
                    1 => at.1,
                    2 => {
                        let w = word_at(&text(), at.1);
                        if forward { w.1 } else { w.0 }
                    }
                    _ if forward => len(at.0),
                    _ => 0,
                };
                marks[1] = at.0;
            }
            self.hold = sel.is_some_and(|s| s.range(marks).0 != s.range(marks).1);
            // Past an edge: scroll that way, the faster the farther out.
            let r = out.inner_rect;
            let past = |v: f32, lo: f32, hi: f32| if v < lo { v - lo } else if v > hi { v - hi } else { 0.0 };
            let by = egui::vec2(if self.wrap { 0.0 } else { past(p.x, r.left(), r.right()) }, past(p.y, r.top(), r.bottom()));
            if by != egui::Vec2::ZERO {
                self.scroll_by = -by * 15.0 * dt;
                ui.ctx().request_repaint();
            }
        }
        (self.sel, b.marks) = (sel, marks);
        let (lines, appended) = (b.lines.len(), b.appended);
        // Matches on the scroll track, the current one white (at most one per pixel row).
        if !self.hits.is_empty() && lines > 0 {
            let track = out.inner_rect;
            let mut last_y = f32::NEG_INFINITY;
            for (k, &(li, first)) in self.hits.iter().enumerate() {
                let y = track.top() + track.height() * li as f32 / lines as f32;
                let next = self.hits.get(k + 1).map_or(self.find.total as u32, |h| h.1);
                let current = (first as usize..next as usize).contains(&self.find.current);
                if y - last_y >= 1.0 || current {
                    let color = if current { Color32::WHITE } else { find::HIT };
                    ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(track.right() - 5.0, y - 1.0), egui::vec2(5.0, 3.0)), 1.0, color);
                    last_y = y;
                }
            }
        }
        drop(b);
        let off = out.state.offset.y;
        if off <= 0.0 && self.last_off > 0.0 {
            self.load_earlier(ui.ctx()); // reached the top: fetch older lines
        }
        (self.last_off, self.view_h) = (off, out.inner_rect.height());
        // Away from the bottom with new lines coming in: offer to jump there.
        let at_bottom = off + out.inner_rect.height() >= out.content_size.y - row_h;
        if at_bottom {
            self.away_from = None;
        } else {
            let from = *self.away_from.get_or_insert(appended);
            let new = appended.saturating_sub(from);
            if new > 0 {
                let text = format!("⬇ {new} new line{} · Go to end", if new == 1 { "" } else { "s" });
                let size = egui::vec2(ui.fonts_mut(|f| f.layout_no_wrap(text.clone(), FontId::proportional(13.0), Color32::WHITE).size().x) + 36.0, 34.0);
                let at = egui::Rect::from_min_size(out.inner_rect.right_bottom() - size - egui::vec2(24.0, 14.0), size);
                let pill = egui::Button::new(RichText::new(text).strong()).corner_radius(17.0).stroke(egui::Stroke::new(1.0, ui.visuals().selection.stroke.color));
                if ui.put(at, pill).clicked() {
                    self.to_end();
                }
            }
        }
        if keys {
            let (home, end) = ui.input_mut(|i| (i.consume_key(egui::Modifiers::COMMAND, egui::Key::Home), i.consume_key(egui::Modifiers::COMMAND, egui::Key::End)));
            if home {
                self.to_start();
            }
            if end {
                self.to_end();
            }
        }
    }
}

// ---------------------------------------------------------------- terminals

/// egui_term panics if its event receiver goes away; the App keeps the receiver for its lifetime.
pub static TERM_TX: OnceLock<Sender<(u64, PtyEvent)>> = OnceLock::new();

pub struct TermTab {
    pub title: String,
    /// A local terminal for this context (id): reopened on the next start.
    pub local: Option<String>,
    backend: TerminalBackend,
    /// Pod this terminal owns (node shell): deleted when the terminal goes away.
    pub cleanup: Option<Cleanup>,
    /// Pass it was last drawn in: coming back on screen (opened, its tab picked) takes the keyboard.
    drawn: u64,
}

thread_local! {
    /// The terminal that takes the keyboard (backend id): the one opened, clicked or brought up
    /// last, until a click somewhere else. Only one at a time, even with several on screen.
    static KEYBOARD: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    /// The pass a terminal holding the keyboard was last drawn in: the logs leave the keys to it.
    static TERM_KEYS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

pub type Command = (String, Vec<String>);

/// (client, namespace, pod) to delete.
pub type Cleanup = (Client, String, String);

impl TermTab {
    pub fn new(ctx: &egui::Context, id: u64, title: String, (shell, args): Command) -> std::io::Result<Self> {
        let tx = TERM_TX.get().expect("terminal channel").clone();
        let settings = BackendSettings { shell, args, working_directory: kubeconfig::home() };
        Ok(TermTab { title, local: None, backend: TerminalBackend::new(id, ctx.clone(), tx, settings)?, cleanup: None, drawn: 0 })
    }

    /// Whether typing goes here (the app leaves its shortcuts to the terminal then).
    pub fn has_keyboard(&self) -> bool {
        KEYBOARD.with(|k| k.get()) == Some(self.backend.id())
    }

    /// `size`: the terminal's font size factor (Settings), on egui_term's 14 px.
    pub fn ui(&mut self, ui: &mut egui::Ui, size: f32) {
        let id = self.backend.id();
        let area = ui.max_rect();
        // Where a button went down this frame (a tap can press and release in one frame, so not
        // `press_origin`, which is gone by then).
        let press = ui.input(|i| i.events.iter().rev().find_map(|e| match e {
            egui::Event::PointerButton { pressed: true, pos, .. } => Some(*pos),
            _ => None,
        }));
        if let Some(p) = press {
            if area.contains(p) {
                KEYBOARD.with(|k| k.set(Some(id)));
            } else if self.has_keyboard() {
                KEYBOARD.with(|k| k.set(None)); // clicked elsewhere
            }
        }
        let pass = ui.ctx().cumulative_pass_nr();
        if self.drawn + 1 < pass {
            KEYBOARD.with(|k| k.set(Some(id))); // just opened, or its tab was picked
        }
        self.drawn = pass;
        let keys = self.has_keyboard();
        if keys {
            TERM_KEYS.with(|t| t.set(Some(pass)));
        }
        let font = TerminalFont::new(FontSettings { font_type: FontId::monospace(14.0 * size) });
        let view = TerminalView::new(ui, &mut self.backend).set_focus(keys).set_size(ui.available_size()).set_font(font);
        let r = ui.add(view);
        if keys {
            let accent = crate::ui_kit::tokens(ui).accent;
            ui.painter().rect_stroke(r.rect, 0.0, egui::Stroke::new(1.0, accent), egui::StrokeKind::Inside);
        }
    }
}

impl Drop for TermTab {
    fn drop(&mut self) {
        if self.has_keyboard() {
            KEYBOARD.with(|k| k.set(None));
        }
        // ponytail: if the app is killed mid-session the pod stays until its 4h sleep ends.
        if let Some((client, ns, pod)) = self.cleanup.take() {
            tokio::spawn(ops::delete(client, ApiResource::erase::<Pod>(&()), ns, pod));
        }
    }
}

fn kubectl_line(k: &Ctx, rest: &str) -> String {
    format!("kubectl --kubeconfig \"{}\" --context \"{}\" {rest}", k.file.display(), k.name)
}

/// alacritty passes args raw on Windows, so hand cmd.exe one pre-quoted line.
fn cmd_line(line: String) -> Command {
    ("cmd.exe".into(), vec!["/s".into(), "/c".into(), format!("\"{line}\"")])
}

pub fn local_shell(k: &Ctx) -> Command {
    let kc = kubeconfig::terminal_kubeconfig(k).unwrap_or_default().replace('\'', "''");
    let name = k.name.replace('\'', "''");
    let script = format!("$env:KUBECONFIG='{kc}'; Write-Host 'kubectl context: {name}' -ForegroundColor Cyan");
    ("powershell.exe".into(), vec!["-NoLogo".into(), "-NoExit".into(), "-Command".into(), script])
}

pub fn pod_shell(k: &Ctx, ns: &str, pod: &str, container: Option<&str>) -> Command {
    let c = container.map(|c| format!(" -c {c}")).unwrap_or_default();
    cmd_line(kubectl_line(k, &format!("-n {ns} exec -it {pod}{c} -- sh -c \"clear; (bash || ash || sh)\" || pause")))
}

/// Shell in the node's own namespaces, through a node-shell pod.
pub fn node_exec(k: &Ctx, ns: &str, pod: &str) -> Command {
    cmd_line(kubectl_line(k, &format!("-n {ns} exec -it {pod} -- nsenter -t 1 -m -u -i -n -p -- sh -c \"clear; (bash || ash || sh)\" || pause")))
}

pub fn drain(k: &Ctx, node: &str) -> Command {
    cmd_line(kubectl_line(k, &format!("drain {node} --ignore-daemonsets --delete-emptydir-data & pause")))
}

// ---------------------------------------------------------------- yaml

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
}

pub const CREATE_TEMPLATE: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: example\ndata:\n  key: value\n";

impl YamlTab {
    fn with(title: String, client: Option<Client>, mode: YamlMode, text: String, load: Option<Pending<Res<Value>>>) -> Self {
        YamlTab { title, client, mode, text, load, busy: None, status: None, find: Find::default() }
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
                ui.spinner();
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
        let theme = egui_extras::syntax_highlighting::CodeTheme::from_memory(ui.ctx(), ui.style());
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            let mut job = if buf.as_str().len() < 200_000 {
                egui_extras::syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, buf.as_str(), "yaml")
            } else {
                // ponytail: no highlighting for huge objects; syntect gets slow past ~200KB.
                LayoutJob::simple(buf.as_str().to_owned(), TextStyle::Monospace.resolve(ui.style()), ui.visuals().text_color(), f32::INFINITY)
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_kept_once() {
        let line = |ns: u32, text: &str| Line { ts: Some(jiff::Timestamp::new(1_790_000_000, ns as i32).unwrap()), src: 0, text: text.into(), spans: vec![], chars: text.len() as u32 };
        let mut b = LogBuf::default();
        for (ns, t) in [(1, "a"), (2, "b"), (3, "c")] {
            b.insert(line(ns, t), true);
        }
        // The same lines again (a reload racing the stream, an earlier-lines fetch): no copies.
        for (ns, t) in [(1, "a"), (2, "b"), (3, "c"), (4, "d")] {
            b.insert(line(ns, t), true);
        }
        assert_eq!(b.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        b.insert(line(4, "d2"), true); // same instant, other text: a real line
        assert_eq!(b.lines.len(), 5);
        assert_eq!(b.appended, 5);
        b.insert(line(0, "earlier"), false); // loaded earlier: goes in front, isn't "new"
        assert_eq!((b.lines[0].text.as_str(), b.appended), ("earlier", 5));
    }

    #[test]
    fn log_start_points() {
        let now: jiff::Zoned = "2026-09-28T15:00:00-03:00[-03:00]".parse().unwrap();
        let at = |s: &str| parse_since(s, &now).unwrap().map(|t| t.to_string());
        assert_eq!(at(""), None);
        assert_eq!(at("2026-09-26T11:10:46.495Z").as_deref(), Some("2026-09-26T11:10:46.495Z")); // copied from a line
        assert_eq!(at("2026-09-28 14:30").as_deref(), Some("2026-09-28T17:30:00Z")); // local time
        assert_eq!(at("2026-09-28 14:30:05.250").as_deref(), Some("2026-09-28T17:30:05.25Z")); // as a line shows it
        assert_eq!(at("2026-09-28").as_deref(), Some("2026-09-28T03:00:00Z"));
        assert_eq!(at("14:30").as_deref(), Some("2026-09-28T17:30:00Z")); // today
        assert_eq!(at("2h").as_deref(), Some("2026-09-28T16:00:00Z"));
        assert_eq!(at("1h 30m").as_deref(), Some("2026-09-28T16:30:00Z"));
        assert_eq!(at("1d").as_deref(), Some("2026-09-27T18:00:00Z"));
        assert!(parse_since("yesterday-ish", &now).is_err());
    }

    #[test]
    fn timestamps_show_in_local_time() {
        let tz = jiff::tz::TimeZone::fixed(jiff::tz::offset(-3));
        let at = |s: &str| fmt_ts(s.parse().unwrap(), &tz);
        assert_eq!(at("2026-10-06T10:45:29.265Z"), "2026-10-06 07:45:29.265");
        assert_eq!(at("2026-10-06T02:00:00Z"), "2026-10-05 23:00:00.000"); // the day before
        assert_eq!(at("2026-10-06T10:45:29.1234567Z"), "2026-10-06 07:45:29.123");
        // A line as it is written to a file: the shown time, or just the text.
        let raw = "2026-10-06T10:45:29.265Z hello world";
        assert_eq!(export_line(raw, true, &tz), "2026-10-06 07:45:29.265 hello world");
        assert_eq!(export_line(raw, false, &tz), "hello world");
        assert_eq!(export_line("no timestamp", true, &tz), "no timestamp");
        assert_eq!(export_line("no timestamp", false, &tz), "no timestamp");
        // What a line shows can be typed back in the date range.
        let now: jiff::Zoned = "2026-10-06T12:00:00-03:00[-03:00]".parse().unwrap();
        let shown = at("2026-10-06T10:45:29.265Z");
        assert_eq!(parse_since(&shown, &now).unwrap(), Some("2026-10-06T10:45:29.265Z".parse().unwrap()));
    }

    #[tokio::test]
    async fn lines_show_the_local_time_aligned() {
        let ctx = egui::Context::default();
        let client = kube::Client::try_from(kube::Config::new("http://127.0.0.1:9".parse().unwrap())).unwrap();
        let mut t = LogTab::new(&ctx, client, vec![], None);
        t.tz = jiff::tz::TimeZone::fixed(jiff::tz::offset(-3));
        t.show_src = false;
        let line = |ts: Option<&str>| Line { ts: ts.map(|s| s.parse().unwrap()), src: 0, text: "hello".into(), spans: vec![], chars: 5 };
        let shown = t.shown(&line(Some("2026-10-06T10:45:29.265Z")));
        assert_eq!(shown, "2026-10-06 07:45:29.265 hello");
        assert_eq!(shown.find("hello"), Some(TS_WIDTH), "the text starts after the prefix the layout counts");
        assert_eq!(t.shown(&line(None)), format!("{}hello", " ".repeat(TS_WIDTH)));
        t.show_ts = false;
        assert_eq!(t.shown(&line(Some("2026-10-06T10:45:29.265Z"))), "hello");
    }

    #[test]
    fn parses_timestamp_and_ansi() {
        let l = parse_line("2026-09-26T11:10:46.495123456Z \x1b[31mERR\x1b[0m done\tok", 2);
        assert!(l.ts.is_some());
        assert_eq!(l.text, "ERR done    ok");
        assert_eq!(l.spans, vec![(0, Some(PALETTE[1])), (3, None)]);
        assert_eq!(l.src, 2);
        let plain = parse_line("no timestamp here", 0);
        assert!(plain.ts.is_none() && plain.text == "no timestamp here" && plain.spans.is_empty());
        assert_eq!(strip_ansi("\x1b]0;title\x07x\x1b[2Ky").0, "xy");
        assert_eq!(strip_ansi("\x1b[38;5;196mred").1, vec![(0, Some(xterm256(196)))]);
    }

    #[test]
    fn level_colors() {
        assert_eq!(level_color("2026 ERROR failed"), Some(RED));
        assert_eq!(level_color("E0926 11:10:46.1 x"), Some(RED));
        assert_eq!(level_color(r#"{"level":"warn","msg":"x"}"#), Some(ORANGE));
        assert_eq!(level_color("all good"), None);
    }

    #[test]
    fn merges_earlier_lines_by_overlap() {
        let l = |t: &str, s: &str| parse_line(&format!("2026-01-01T00:00:0{t}Z {s}"), 0);
        let fetched = [l("1", "a"), l("2", "b"), l("3", "c"), l("3", "c2"), l("4", "d")];
        let loaded = [l("3", "c"), l("3", "c2"), l("4", "d")];
        assert_eq!(older(&fetched, &loaded.iter().collect::<Vec<_>>()), 2); // same-timestamp lines kept exactly once
        let rotated = [l("5", "e")];
        assert_eq!(older(&fetched, &rotated.iter().collect::<Vec<_>>()), 5); // no overlap: all older by time
    }

    #[test]
    fn buffer_interleaves_sources_by_time() {
        let mut b = LogBuf::default();
        for (t, s) in [("2", 0), ("1", 1), ("3", 1), ("2", 1)] {
            b.insert(parse_line(&format!("2026-01-01T00:00:0{t}Z x"), s), true);
        }
        let order: Vec<_> = b.lines.iter().map(|l| (l.ts.unwrap().as_second() % 10, l.src)).collect();
        assert_eq!(order, vec![(1, 1), (2, 0), (2, 1), (3, 1)]);
    }

    #[test]
    fn slices_jobs_for_wrapping() {
        let mut job = LayoutJob::default();
        job.append("abc", 0.0, TextFormat::default());
        job.append("def", 0.0, TextFormat { color: Color32::RED, ..Default::default() });
        let s = slice_job(&job, 2, 5);
        assert_eq!(s.text, "cde");
        assert_eq!(s.sections.len(), 2);
        assert_eq!(s.sections[1].byte_range.start.0, 1);
    }

    /// Selecting in a log as in an editor, while lines stream in; the keys scroll.
    #[tokio::test]
    async fn log_selection_and_keys() {
        use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, vec2};
        let ctx = egui::Context::default();
        let client = kube::Client::try_from(kube::Config::new("http://127.0.0.1:9".parse().unwrap())).unwrap();
        let mut t = LogTab::new(&ctx, client, vec![], None);
        t.show_ts = false; // lines read "[?] line N ..."
        let mut n = 0;
        let mut add = |t: &LogTab, k: usize| {
            for _ in 0..k {
                n += 1;
                let text = format!("line {n} some-text to select");
                t.buf.lock().unwrap().insert(Line { ts: Some(jiff::Timestamp::new(1_790_000_000 + n, 0).unwrap()), src: 0, chars: text.len() as u32, text, spans: vec![] }, true);
            }
        };
        add(&t, 300);
        let mut k = 0.0;
        let mut frame = |t: &mut LogTab, events: Vec<Event>| {
            k += 1.0;
            let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 500.0))), events, time: Some(k * 0.05), ..Default::default() };
            let _ = ctx.run_ui(input, |ui| t.lines_ui(ui));
        };
        let btn = |p: Pos2, pressed| Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers: Default::default() };
        let key = |key, modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let drag = |t: &mut LogTab, frame: &mut dyn FnMut(&mut LogTab, Vec<Event>), a: Pos2, b: Pos2, hold: usize, stream: &mut dyn FnMut(&LogTab)| {
            for events in [vec![Event::PointerMoved(a)], vec![Event::PointerMoved(a)], vec![btn(a, true)]] {
                frame(t, events);
            }
            for i in 1..=10 {
                stream(t); // a busy log: a line every frame
                frame(t, vec![Event::PointerMoved(a + (b - a) * (i as f32 / 10.0))]);
            }
            for _ in 0..hold {
                frame(t, vec![]);
            }
            frame(t, vec![btn(b, false)]);
        };
        frame(&mut t, vec![]);
        frame(&mut t, vec![]);
        let rows = |t: &LogTab| t.selected_text(&t.buf.lock().unwrap()).lines().map(String::from).collect::<Vec<_>>();

        // Drag across lines of a followed log: the selection stays on its lines.
        drag(&mut t, &mut frame, Pos2::new(60.0, 200.0), Pos2::new(400.0, 300.0), 0, &mut |t| add(t, 1));
        let sel = rows(&t);
        assert!(t.hold && sel.len() > 3, "{sel:?}");
        assert!(sel[1].starts_with("[?] line ") && sel.windows(2).all(|w| w[0] != w[1]), "{sel:?}");
        // Shift+Down: one line more; Shift+End: to its end.
        frame(&mut t, vec![key(Key::ArrowDown, Modifiers::SHIFT), key(Key::End, Modifiers::SHIFT)]);
        let more = rows(&t);
        assert_eq!(more.len(), sel.len() + 1);
        assert!(more.last().unwrap().ends_with("to select"), "{more:?}");
        // Ctrl+A and copy: every line.
        frame(&mut t, vec![key(Key::A, Modifiers::COMMAND)]);
        assert_eq!(rows(&t).len(), t.buf.lock().unwrap().lines.len());
        frame(&mut t, vec![Event::Copy]);
        // Escape drops it.
        frame(&mut t, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(rows(&t).is_empty() && !t.hold);

        // A double press takes the word.
        let w = Pos2::new(60.0, 250.0);
        frame(&mut t, vec![Event::PointerMoved(w)]);
        frame(&mut t, vec![btn(w, true)]);
        frame(&mut t, vec![btn(w, false)]);
        frame(&mut t, vec![btn(w, true)]);
        frame(&mut t, vec![btn(w, false)]);
        assert_eq!(rows(&t).len(), 1);
        let word = rows(&t)[0].clone();
        assert!(!word.contains(' ') && !word.is_empty(), "{word:?}");

        // Dragging past the bottom edge scrolls on and takes the lines that come into view.
        t.to_start();
        frame(&mut t, vec![]);
        frame(&mut t, vec![]);
        let top = t.last_off;
        drag(&mut t, &mut frame, Pos2::new(60.0, 100.0), Pos2::new(400.0, 560.0), 30, &mut |_| {});
        assert!(t.last_off > top + 100.0, "scrolled: {top} -> {}", t.last_off);
        let far = rows(&t).len();
        assert!(far as f32 > t.view_h / 17.0 + 5.0, "{far} lines selected in a {} px view", t.view_h);
        frame(&mut t, vec![key(Key::Escape, Modifiers::NONE)]);

        // Page Up, then the down arrow, scroll.
        t.to_end();
        frame(&mut t, vec![]);
        frame(&mut t, vec![]);
        let bottom = t.last_off;
        frame(&mut t, vec![key(Key::PageUp, Modifiers::NONE)]);
        frame(&mut t, vec![]);
        let paged = t.last_off;
        assert!(paged < bottom - t.view_h / 2.0, "page up: {bottom} -> {paged}");
        frame(&mut t, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        frame(&mut t, vec![]);
        assert!(t.last_off > paged && t.last_off < paged + t.view_h / 2.0, "arrow down: {paged} -> {}", t.last_off);

        // A press inside a selection keeps it, and moving on asks to drag its text out of the window.
        let free = |t: &mut LogTab, frame: &mut dyn FnMut(&mut LogTab, Vec<Event>)| {
            frame(t, vec![Event::PointerMoved(Pos2::new(600.0, 450.0))]);
            frame(t, vec![btn(Pos2::new(600.0, 450.0), true)]);
            frame(t, vec![btn(Pos2::new(600.0, 450.0), false)]);
        };
        free(&mut t, &mut frame);
        drag(&mut t, &mut frame, Pos2::new(60.0, 200.0), Pos2::new(400.0, 300.0), 0, &mut |_| {});
        let picked = rows(&t);
        assert!(picked.len() > 3, "{picked:?}");
        assert!(ctx.kxs_take_drag_out().is_none());
        let inside = Pos2::new(100.0, 250.0);
        frame(&mut t, vec![Event::PointerMoved(inside)]);
        frame(&mut t, vec![btn(inside, true)]);
        assert_eq!(rows(&t), picked, "the press keeps the selection");
        assert!(ctx.kxs_take_drag_out().is_none(), "not yet moved");
        for i in 1..=4 {
            frame(&mut t, vec![Event::PointerMoved(inside + vec2(10.0 * i as f32, 3.0 * i as f32))]);
        }
        assert_eq!(ctx.kxs_take_drag_out().map(|d| d.lines().map(String::from).collect::<Vec<_>>()), Some(picked.clone()));
        assert_eq!(rows(&t), picked, "the drag does not change the selection");
        frame(&mut t, vec![btn(inside, false)]);
        assert_eq!(rows(&t), picked);
        // A click without moving collapses it, as in an editor.
        frame(&mut t, vec![Event::PointerMoved(inside)]);
        frame(&mut t, vec![btn(inside, true)]);
        frame(&mut t, vec![btn(inside, false)]);
        frame(&mut t, vec![]);
        assert!(rows(&t).is_empty() && !t.hold, "{:?}", rows(&t));
        assert!(ctx.kxs_take_drag_out().is_none());
        // Outside the selection a press starts a new one, as before.
        drag(&mut t, &mut frame, Pos2::new(60.0, 200.0), Pos2::new(400.0, 300.0), 0, &mut |_| {});
        let first = rows(&t);
        let outside = Pos2::new(60.0, 100.0);
        drag(&mut t, &mut frame, outside, outside + vec2(200.0, 0.0), 0, &mut |_| {});
        assert_ne!(rows(&t), first);
        assert!(ctx.kxs_take_drag_out().is_none());
    }

    #[test]
    fn words_to_select() {
        let w = |t: &str, i| {
            let (a, b) = word_at(t, i);
            t.chars().skip(a).take(b - a).collect::<String>()
        };
        assert_eq!(w("pod api-gateway-7d9f8c failed: OOM", 6), "api-gateway-7d9f8c");
        assert_eq!(w("pod api-gateway-7d9f8c failed: OOM", 25), "failed");
        assert_eq!(w("{\"ip\":\"10.42.1.37\"}", 10), "10.42.1.37");
        assert_eq!(w("a  b", 2), " ");
    }
}
