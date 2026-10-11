

use super::*;
use crate::testkit::*;

/// ~140-char JSON log line number `i` (the usual shape of what the cluster's apps write).
fn bench_line(i: u32) -> Line {
    let text = format!(r#"{{"level":"info","ts":"2026-10-06T10:{:02}:{:02}Z","app":"argocd/service-{}","msg":"Update successful","duration_ms":{},"error":"{}"}}"#, i / 60 % 60, i % 60, i % 500, i % 977, if i.is_multiple_of(97) { "error 500 upstream" } else { "none" });
    Line { ts: Some(jiff::Timestamp::new(1_790_000_000 + i64::from(i) / 50, (i % 50 * 1000) as i32).unwrap()), src: 0, chars: text.chars().count() as u32, text, spans: vec![] }
}

/// 100k lines in a buffer: filling it, and the match scan `lines_ui` repeats while a find is open.
#[test]
#[ignore]
fn bench_log_buffer_100k() {
    crate::prof::bench("log buffer: insert 100k lines in order", 3, || {
        let mut b = LogBuf::default();
        for i in 0..100_000 {
            b.insert(bench_line(i), true);
        }
    });
    let mut b = LogBuf::default();
    for i in 0..100_000 {
        b.insert(bench_line(i), true);
    }
    let scan = |m: &Matcher| b.lines.iter().map(|l| m.count(&l.text)).sum::<usize>();
    let (plain, rare, regex) = (Matcher::plain("error", false), Matcher::plain("zzzz-not-there", false), Matcher::regex(r"err(or)? \d+", false).unwrap());
    crate::prof::bench("log scan 100k lines, plain (1% match)", 5, || assert!(scan(&plain) > 0));
    crate::prof::bench("log scan 100k lines, plain (no match)", 5, || assert_eq!(scan(&rare), 0));
    crate::prof::bench("log scan 100k lines, regex", 5, || assert!(scan(&regex) > 0));
    crate::prof::bench("log scan 100k lines, hit() first (plain)", 5, || assert!(b.lines.iter().filter(|l| plain.hit(&l.text)).map(|l| plain.count(&l.text)).sum::<usize>() > 0));
}

/// One `lines_ui` frame over 100k lines, as it runs while lines stream in.
#[tokio::test]
#[ignore]
async fn bench_log_frame_100k() {
    let mut h = Harness::new(egui::vec2(1400.0, 800.0));
    let mut t = LogTab::new(&h.ctx, dummy_client(), vec![], None);
    for i in 0..100_000 {
        t.buf.locked().insert(bench_line(i), false);
    }
    let mut frame = |t: &mut LogTab, streaming: bool| {
        if streaming {
            // A line arrived: the buffer changed and the last scan is old.
            t.buf.locked().rev += 1;
            t.scanned = Instant::now() - Duration::from_secs(1);
        }
        h.frame(vec![], |ui| t.lines_ui(ui));
    };
    frame(&mut t, false);
    frame(&mut t, false);
    crate::prof::bench("lines_ui frame, 100k lines, idle", 20, || frame(&mut t, false));
    crate::prof::bench("lines_ui frame, 100k lines, streaming", 20, || frame(&mut t, true));
    t.wrap = true;
    crate::prof::bench("lines_ui frame, wrap on, streaming", 20, || frame(&mut t, true));
    t.wrap = false;
    (t.find.open, t.find.query) = (true, "error".into());
    crate::prof::bench("lines_ui frame, find open, streaming", 10, || frame(&mut t, true));
}

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
    let mut t = LogTab::new(&egui::Context::default(), dummy_client(), vec![], None);
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

/// With no search open a new line needs no scan, so the log neither waits 250 ms for one nor asks for
/// a frame to run it; with a search the scan is throttled.
#[tokio::test]
async fn lines_that_come_in_are_scanned_at_once_only_without_a_search() {
    let mut h = Harness::themed(egui::vec2(900.0, 400.0));
    let mut t = LogTab::new(&h.ctx, dummy_client(), vec![], None);
    let mut frame = |t: &mut LogTab| h.frame(vec![], |ui| t.lines_ui(ui));
    t.buf.lock().unwrap().insert(bench_line(1), true);
    frame(&mut t);
    t.buf.lock().unwrap().insert(bench_line(2), true);
    frame(&mut t);
    let rev = t.buf.lock().unwrap().rev;
    assert_eq!(t.scan_key.1, rev, "no search: scanned (nothing) at once");
    (t.find.open, t.find.query) = (true, "service".into());
    frame(&mut t);
    t.buf.lock().unwrap().insert(bench_line(3), true);
    frame(&mut t);
    assert_ne!(t.scan_key.1, t.buf.lock().unwrap().rev, "a search is rescanned at most every 250 ms");
}

/// Selecting in a log as in an editor, while lines stream in; the keys scroll.
#[tokio::test]
async fn log_selection_and_keys() {
    use egui::{Event, Key, Modifiers, Pos2, vec2};
    let mut h = Harness::new(vec2(900.0, 500.0));
    let ctx = h.ctx.clone();
    let mut t = LogTab::new(&ctx, dummy_client(), vec![], None);
    t.show_ts = false; // lines read "[?] line N ..."
    let mut n = 0;
    let mut add = |t: &LogTab, k: usize| {
        for _ in 0..k {
            n += 1;
            let text = format!("line {n} some-text to select");
            t.buf.locked().insert(Line { ts: Some(jiff::Timestamp::new(1_790_000_000 + n, 0).unwrap()), src: 0, chars: text.len() as u32, text, spans: vec![] }, true);
        }
    };
    add(&t, 300);
    let mut frame = |t: &mut LogTab, events: Vec<Event>| h.frame(events, |ui| t.lines_ui(ui));
    let btn = button;
    let drag = |t: &mut LogTab, frame: &mut dyn FnMut(&mut LogTab, Vec<Event>), a: Pos2, b: Pos2, hold: usize, stream: &mut dyn FnMut(&LogTab)| {
        for events in [vec![moved(a)], vec![moved(a)], vec![btn(a, true)]] {
            frame(t, events);
        }
        for i in 1..=10 {
            stream(t); // a busy log: a line every frame
            frame(t, vec![moved(a + (b - a) * (i as f32 / 10.0))]);
        }
        for _ in 0..hold {
            frame(t, vec![]);
        }
        frame(t, vec![btn(b, false)]);
    };
    frame(&mut t, vec![]);
    frame(&mut t, vec![]);
    let rows = |t: &LogTab| t.selected_text(&t.buf.locked()).lines().map(String::from).collect::<Vec<_>>();

    // Drag across lines of a followed log: the selection stays on its lines.
    drag(&mut t, &mut frame, Pos2::new(60.0, 200.0), Pos2::new(400.0, 300.0), 0, &mut |t| add(t, 1));
    let sel = rows(&t);
    assert!(t.select.hold && sel.len() > 3, "{sel:?}");
    assert!(sel[1].starts_with("[?] line ") && sel.windows(2).all(|w| w[0] != w[1]), "{sel:?}");
    // Shift+Down: one line more; Shift+End: to its end.
    frame(&mut t, vec![key(Key::ArrowDown, Modifiers::SHIFT), key(Key::End, Modifiers::SHIFT)]);
    let more = rows(&t);
    assert_eq!(more.len(), sel.len() + 1);
    assert!(more.last().unwrap().ends_with("to select"), "{more:?}");
    // Ctrl+A and copy: every line.
    frame(&mut t, vec![key(Key::A, Modifiers::COMMAND)]);
    assert_eq!(rows(&t).len(), t.buf.locked().lines.len());
    frame(&mut t, vec![Event::Copy]);
    // Escape drops it.
    frame(&mut t, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(rows(&t).is_empty() && !t.select.hold);

    // A double press takes the word.
    let w = Pos2::new(60.0, 250.0);
    frame(&mut t, vec![moved(w)]);
    frame(&mut t, vec![btn(w, true)]);
    frame(&mut t, vec![btn(w, false)]);
    frame(&mut t, vec![btn(w, true)]);
    frame(&mut t, vec![btn(w, false)]);
    assert_eq!(rows(&t).len(), 1);
    let word = rows(&t)[0].clone();
    assert!(!word.contains(' ') && !word.is_empty(), "{word:?}");

    // Dragging past the bottom edge scrolls on and takes the lines that come into view.
    t.jump_to_start();
    frame(&mut t, vec![]);
    frame(&mut t, vec![]);
    let top = t.last_off;
    drag(&mut t, &mut frame, Pos2::new(60.0, 100.0), Pos2::new(400.0, 560.0), 30, &mut |_| {});
    assert!(t.last_off > top + 100.0, "scrolled: {top} -> {}", t.last_off);
    let far = rows(&t).len();
    assert!(far as f32 > t.view_h / 17.0 + 5.0, "{far} lines selected in a {} px view", t.view_h);
    frame(&mut t, vec![key(Key::Escape, Modifiers::NONE)]);

    // Page Up, then the down arrow, scroll.
    t.jump_to_end();
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
        frame(t, vec![moved(Pos2::new(600.0, 450.0))]);
        frame(t, vec![btn(Pos2::new(600.0, 450.0), true)]);
        frame(t, vec![btn(Pos2::new(600.0, 450.0), false)]);
    };
    free(&mut t, &mut frame);
    drag(&mut t, &mut frame, Pos2::new(60.0, 200.0), Pos2::new(400.0, 300.0), 0, &mut |_| {});
    let picked = rows(&t);
    assert!(picked.len() > 3, "{picked:?}");
    assert!(ctx.kxs_take_drag_out().is_none());
    let inside = Pos2::new(100.0, 250.0);
    frame(&mut t, vec![moved(inside)]);
    frame(&mut t, vec![btn(inside, true)]);
    assert_eq!(rows(&t), picked, "the press keeps the selection");
    assert!(ctx.kxs_take_drag_out().is_none(), "not yet moved");
    for i in 1..=4 {
        frame(&mut t, vec![moved(inside + vec2(10.0 * i as f32, 3.0 * i as f32))]);
    }
    assert_eq!(ctx.kxs_take_drag_out().map(|d| d.lines().map(String::from).collect::<Vec<_>>()), Some(picked.clone()));
    assert_eq!(rows(&t), picked, "the drag does not change the selection");
    frame(&mut t, vec![btn(inside, false)]);
    assert_eq!(rows(&t), picked);
    // A click without moving collapses it, as in an editor.
    frame(&mut t, vec![moved(inside)]);
    frame(&mut t, vec![btn(inside, true)]);
    frame(&mut t, vec![btn(inside, false)]);
    frame(&mut t, vec![]);
    assert!(rows(&t).is_empty() && !t.select.hold, "{:?}", rows(&t));
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

// ---------------------------------------------------------------- streams against a fake API server

fn log_body(lines: &[(&str, &str)]) -> String {
    lines.iter().map(|(ts, text)| format!("{ts} {text}
")).collect()
}

fn a_source() -> Source {
    Source { ns: "ns".into(), pod: "pod".into(), container: "app".into(), tag: "pod".into() }
}

fn texts(b: &LogBuf) -> Vec<String> {
    b.lines.iter().map(|l| l.text.clone()).collect()
}

/// Runs a stream to its end and returns what it delivered.
async fn run_stream(client: Client, previous: bool, from: Option<jiff::Timestamp>, to: Option<jiff::Timestamp>) -> LogBuf {
    let (feed, mut inbox) = feed(&egui::Context::default(), &crate::repaint::Shown::default());
    stream(client, a_source(), 0, previous, from, to, feed).await;
    let mut b = LogBuf::default();
    inbox.fill(&mut b);
    b
}

/// Lines wait in the channel until the tab drains them, in order, all of them at once; the window is
/// woken for the first one only.
#[tokio::test]
async fn lines_wait_in_the_channel_until_the_tab_drains_them() {
    let shown = crate::repaint::Shown::default();
    shown.mark();
    let (feed, mut inbox) = feed(&egui::Context::default(), &shown);
    let line = |n: u32| Line { ts: Some(jiff::Timestamp::new(1_790_000_000 + i64::from(n), 0).unwrap()), src: 0, text: format!("line {n}"), spans: vec![], chars: 6 };
    for n in 1..=3 {
        feed.send(Msg::Line(line(n), n > 1));
    }
    feed.send(Msg::Ended(Some("pod: gone".into())));
    let mut b = LogBuf::default();
    inbox.fill(&mut b);
    assert_eq!(texts(&b), ["line 1", "line 2", "line 3"]);
    assert_eq!((b.appended, b.ended, b.errors.clone()), (2, 1, vec!["pod: gone".to_string()]));
    assert!(!feed.queued.load(Ordering::Relaxed), "drained: the next line wakes the window again");
    feed.send(Msg::Line(line(4), true));
    assert!(feed.queued.load(Ordering::Relaxed));
    drop(inbox);
    feed.send(Msg::Line(line(5), true)); // the tab is gone: nothing to do, nothing breaks
}

/// A log of very long lines is kept to a memory budget, not to a number of lines.
#[test]
fn a_log_is_kept_within_its_memory_budget() {
    let mut b = LogBuf::default();
    let big = |n: i64| Line { ts: Some(jiff::Timestamp::new(1_790_000_000 + n, 0).unwrap()), src: 0, text: "x".repeat(1 << 20), spans: vec![], chars: 1 << 20 };
    for n in 0..60 {
        b.insert(big(n), true);
    }
    assert!(!b.trimmed, "60 MB fit");
    b.marks = [58, 59];
    for n in 60..70 {
        b.insert(big(n), true);
    }
    assert!(b.trimmed && b.bytes <= MAX_BYTES, "{} lines, {} bytes", b.lines.len(), b.bytes);
    assert!(b.lines.len() < 70 && b.lines.len() > 50, "{}", b.lines.len());
    assert_eq!(b.lines.back().unwrap().ts, big(69).ts, "the newest are kept");
    assert_eq!(b.lines[b.marks[1]].ts, big(59).ts, "the marks stay on their lines");
    assert_eq!(b.lines[b.marks[0]].ts, big(58).ts);
    let mut small = LogBuf::default();
    for n in 0..1000 {
        small.insert(bench_line(n), true);
    }
    assert!(!small.trimmed && small.lines.len() == 1000);
}

/// A followed log whose connection drops is resumed from just before its last line, and what the
/// server sends again is not shown twice; it ends once the container has stopped producing lines.
#[tokio::test(start_paused = true)]
async fn a_followed_log_resumes_without_repeating_lines() {
    let first = log_body(&[("2026-10-06T10:00:01.100000000Z", "one"), ("2026-10-06T10:00:02.200000000Z", "two")]);
    let second = log_body(&[("2026-10-06T10:00:02.200000000Z", "two"), ("2026-10-06T10:00:03.300000000Z", "three")]);
    let api = crate::testkit::MockApi::default().route("/log", &[&first, &second]); // then nothing more
    let b = run_stream(api.client(), false, None, None).await;
    assert_eq!(texts(&b), ["one", "two", "three"]);
    assert_eq!((b.ended, b.errors.len()), (1, 0));
    let reqs = api.requests();
    assert_eq!(reqs.len(), 4, "two with lines, then two quiet ones end it: {reqs:?}");
    assert!(reqs[0].contains("tailLines=500") && !reqs[0].contains("sinceTime"), "{}", reqs[0]);
    assert!(reqs[1].contains("sinceTime=") && !reqs[1].contains("tailLines"), "{}", reqs[1]);
    assert!(reqs.iter().all(|r| r.contains("follow=true") && r.contains("timestamps=true")), "{reqs:?}");
}

/// A previous container's log, or a closed range, is read once and stops at the end of the range.
#[tokio::test(start_paused = true)]
async fn a_closed_range_is_read_once_and_stops_at_its_end() {
    let all = log_body(&[("2026-10-06T10:00:01.000000000Z", "one"), ("2026-10-06T10:00:02.000000000Z", "two"), ("2026-10-06T10:00:03.000000000Z", "three")]);
    let api = crate::testkit::MockApi::default().route("/log", &[&all]);
    let (from, to) = ("2026-10-06T10:00:00Z".parse().unwrap(), "2026-10-06T10:00:02.500Z".parse().unwrap());
    let b = run_stream(api.client(), false, Some(from), Some(to)).await;
    assert_eq!(texts(&b), ["one", "two"], "past the end of the range is dropped");
    assert_eq!(api.requests().len(), 1);
    assert!(api.requests()[0].contains("sinceTime=") && !api.requests()[0].contains("follow=true"), "{}", api.requests()[0]);
    assert_eq!(b.ended, 1);

    let api = crate::testkit::MockApi::default().route("/log", &[&all]);
    let b = run_stream(api.client(), true, None, None).await;
    assert_eq!(texts(&b).len(), 3);
    assert!(api.requests()[0].contains("previous=true"), "{}", api.requests()[0]);
    assert_eq!(api.requests().len(), 1, "a previous container's log is read once");
}
