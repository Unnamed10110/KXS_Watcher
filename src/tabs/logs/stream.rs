//! Reading pod logs from the API: following (with resume), earlier lines, and downloads to a file.

use super::*;

pub(super) const TAIL: i64 = 500;

/// What a stream task hands to its tab.
pub(super) enum Msg {
    /// A line, and whether it just happened (not the first batch of the stream, not an earlier one).
    Line(Line, bool),
    /// The stream is over, with its error if it had one.
    Ended(Option<String>),
}

/// The sending end of a tab's feed. Lines wait in the channel until the tab drains them (once a
/// frame, all under one lock of the buffer); the window is woken when the first one after a drain
/// arrives, and only while the tab is on screen, not once per line.
#[derive(Clone)]
pub(super) struct Feed {
    tx: tokio::sync::mpsc::UnboundedSender<Msg>,
    pub(super) queued: Arc<AtomicBool>,
    shown: crate::repaint::Shown,
    ctx: egui::Context,
}

pub(super) struct Inbox {
    rx: tokio::sync::mpsc::UnboundedReceiver<Msg>,
    queued: Arc<AtomicBool>,
}

pub(super) fn feed(ctx: &egui::Context, shown: &crate::repaint::Shown) -> (Feed, Inbox) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let queued = Arc::new(AtomicBool::new(false));
    (Feed { tx, queued: queued.clone(), shown: shown.clone(), ctx: ctx.clone() }, Inbox { rx, queued })
}

impl Feed {
    pub(super) fn send(&self, m: Msg) {
        let end = matches!(m, Msg::Ended(_));
        if self.tx.send(m).is_err() {
            return; // the tab went away
        }
        if end {
            crate::repaint::now(&self.ctx);
        } else if !self.queued.swap(true, Ordering::AcqRel) && self.shown.is() {
            crate::repaint::after(&self.ctx, Duration::from_millis(100));
        }
    }
}

impl Inbox {
    /// Everything that arrived since the last call.
    pub(super) fn drain(&mut self, mut f: impl FnMut(Msg)) {
        self.queued.store(false, Ordering::Release);
        while let Ok(m) = self.rx.try_recv() {
            f(m);
        }
    }

    /// The buffer after `drain` has put what arrived into it.
    pub(super) fn fill(&mut self, b: &mut LogBuf) {
        self.drain(|m| match m {
            Msg::Line(l, fresh) => b.insert(l, fresh),
            Msg::Ended(err) => {
                b.ended += 1;
                b.errors.extend(err);
                b.rev += 1;
            }
        });
    }
}

pub(super) const MORE: i64 = 1000;

#[derive(Clone)]
pub(super) struct PodInfo {
    pub(super) ns: String,
    pub(super) name: String,
    pub(super) containers: Vec<String>,
    pub(super) default: String,
}

#[derive(Clone)]
pub(super) struct Source {
    pub(super) ns: String,
    pub(super) pod: String,
    pub(super) container: String,
    pub(super) tag: String,
}

pub(super) async fn pod_infos(client: Client, targets: Vec<(Kind, String, String)>) -> Res<Vec<PodInfo>> {
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

/// Follow one container's log. Dropped connections (load balancers cut idle streams) are resumed
/// from the last line's timestamp; the stream only ends when the API says so or the log stays shut.
/// It starts at `from` (every line since then), else with the last `TAIL` lines; a range with an
/// end (`to`) is read once, like a previous container's log.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream(client: Client, src: Source, idx: u16, previous: bool, from: Option<jiff::Timestamp>, to: Option<jiff::Timestamp>, feed: Feed) {
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
                if let (Some(l), Some(t)) = (last, line.ts)
                    && t <= l {
                        continue;
                    }
                last = line.ts.or(last);
                got += 1;
                let fresh = resumed || line.ts.is_some_and(|t| t >= asked);
                feed.send(Msg::Line(line, fresh));
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
    feed.send(Msg::Ended(result.err().map(|e| format!("{}: {}", src.tag, ops::err_text(&e)))));
}

/// Writes logs to `path` as they arrive (the whole log can be far bigger than the view): each source
/// in turn, headed `==> pod/container <==` when there are several, from `from` (else the start) to
/// `to` (else now), optionally only the lines `only` matches. `done` counts bytes; `stop` cancels,
/// removing the partial file.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download(
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
                if n.is_multiple_of(2000) {
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

/// Earlier lines for each source: (lines older than what's loaded, source exhausted).
pub(super) async fn fetch_earlier(client: Client, srcs: Vec<(u16, Source, usize, Vec<Line>)>, previous: bool) -> Res<Vec<(Vec<Line>, bool)>> {
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
