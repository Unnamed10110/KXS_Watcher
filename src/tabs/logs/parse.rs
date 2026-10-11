//! Reading a log line: timestamp, ANSI colors, log level, and the date typed in "Logs by date".

use super::*;

/// One log line: kubelet timestamp, source (pod/container), text without ANSI codes, color changes.
pub struct Line {
    pub(super) ts: Option<jiff::Timestamp>,
    pub(super) src: u16,
    pub(super) text: String,
    pub(super) spans: Vec<(u32, Option<Color32>)>,
    pub(super) chars: u32,
}

impl Line {
    /// About what the line takes in memory.
    pub(super) fn bytes(&self) -> usize {
        std::mem::size_of::<Line>() + self.text.len() + self.spans.len() * std::mem::size_of::<(u32, Option<Color32>)>()
    }

    pub(super) fn same(&self, o: &Line) -> bool {
        self.ts == o.ts && self.src == o.src && self.text == o.text
    }
}

/// A timestamp as shown and exported: local time, `2026-09-26 08:10:46.495`.
pub(super) fn fmt_ts(t: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> String {
    t.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// A raw log line as written to a downloaded file: with the timestamp as shown (local time), or
/// without it.
pub(super) fn export_line(raw: &str, keep_ts: bool, tz: &jiff::tz::TimeZone) -> String {
    match (split_ts(raw), keep_ts) {
        (_, false) => split_ts(raw).1.to_owned(),
        ((Some(t), text), true) => format!("{} {text}", fmt_ts(t, tz)),
        ((None, _), true) => raw.to_owned(),
    }
}

/// The RFC3339Nano prefix (always requested) and the rest of a raw line.
pub(super) fn split_ts(raw: &str) -> (Option<jiff::Timestamp>, &str) {
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
pub(super) fn parse_line(raw: &str, src: u16) -> Line {
    let (ts, rest) = split_ts(raw);
    let (text, spans) = strip_ansi(rest);
    Line { ts, src, chars: text.chars().count() as u32, text, spans }
}

pub(super) const PALETTE: [Color32; 16] = [
    Color32::from_rgb(118, 118, 118), Color32::from_rgb(205, 49, 49), Color32::from_rgb(13, 160, 100), Color32::from_rgb(190, 160, 0),
    Color32::from_rgb(36, 114, 200), Color32::from_rgb(188, 63, 188), Color32::from_rgb(17, 150, 185), Color32::from_rgb(170, 170, 170),
    Color32::from_rgb(130, 130, 130), Color32::from_rgb(241, 76, 76), Color32::from_rgb(35, 190, 120), Color32::from_rgb(210, 180, 0),
    Color32::from_rgb(59, 142, 234), Color32::from_rgb(214, 112, 214), Color32::from_rgb(41, 184, 219), Color32::from_rgb(200, 200, 200),
];

pub(super) fn xterm256(n: u8) -> Color32 {
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
pub(super) fn sgr(params: &str, mut fg: Option<Color32>) -> Option<Color32> {
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
pub(super) fn strip_ansi(s: &str) -> (String, Vec<(u32, Option<Color32>)>) {
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
pub(super) fn level_color(text: &str) -> Option<Color32> {
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

/// Where a log starts: empty for the last lines; a local date and/or time, as the lines show it
/// (`2026-09-28 14:30`, `2026-09-28 14:30:05.250`, `2026-09-28`, `14:30` today); a UTC timestamp
/// (ends in `Z`); or how long ago (`2h`, `30m`, `1d`, `1h 30m`).
pub(super) fn parse_since(s: &str, now: &jiff::Zoned) -> Result<Option<jiff::Timestamp>, String> {
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
