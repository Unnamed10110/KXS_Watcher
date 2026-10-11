//! Measurement tools for the performance work (docs/PERF.md): a frame HUD, section timers, an
//! allocation counter and a startup trace. Nothing here costs anything unless asked for:
//!
//! * `KXS_HUD=1` (or Ctrl+Shift+F12) shows the HUD: frame CPU time, frames, memory, why the last frame
//!   happened (`ctx.repaint_causes()`), and the time spent in the `prof!` sections;
//! * `cargo build --features profile` adds an allocation counter (allocations per frame in the HUD);
//! * `KXS_TRACE=1` appends a startup timeline to `%TEMP%\kxs-trace.log`.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, Context, Key, Modifiers, RichText};

/// Sections are only timed while the HUD is on.
static ON: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();
static TRACE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

/// Call first thing in `main`: the startup trace counts from here.
pub fn start() {
    START.get_or_init(Instant::now);
}

/// One line of the startup timeline (`KXS_TRACE=1`): milliseconds since [`start`], and the label.
pub fn trace(label: &str) {
    let path = TRACE.get_or_init(|| std::env::var_os("KXS_TRACE").map(|_| std::env::temp_dir().join("kxs-trace.log")));
    let (Some(path), Some(t0)) = (path, START.get()) else { return };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[+{:>6.1} ms] {label}", t0.elapsed().as_secs_f64() * 1000.0);
    }
}

/// [`trace`] the first time only (`flag` is a static per call site).
pub fn trace_once(flag: &AtomicBool, label: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        trace(label);
    }
}

// ---------------------------------------------------------------- section timers

#[derive(Default, Clone, Copy)]
pub struct Stat {
    pub n: u32,
    pub total_ms: f64,
    pub max_ms: f64,
}

thread_local!(static SECTIONS: RefCell<BTreeMap<&'static str, Stat>> = const { RefCell::new(BTreeMap::new()) });

/// Times the rest of the enclosing scope while the HUD is on: `crate::prof!("List::refresh");`
pub struct Scope(Option<(&'static str, Instant)>);

impl Scope {
    pub fn new(name: &'static str) -> Scope {
        Scope(ON.load(Ordering::Relaxed).then(|| (name, Instant::now())))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((name, t)) = self.0 {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            SECTIONS.with(|s| {
                let mut s = s.borrow_mut();
                let e = s.entry(name).or_default();
                (e.n, e.total_ms, e.max_ms) = (e.n + 1, e.total_ms + ms, e.max_ms.max(ms));
            });
        }
    }
}

#[macro_export]
macro_rules! prof {
    ($name:literal) => {
        let _prof = $crate::prof::Scope::new($name);
    };
}

// ---------------------------------------------------------------- allocation counter

#[cfg(feature = "profile")]
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub static ALLOCS: AtomicU64 = AtomicU64::new(0);

    pub struct Counting;

    // SAFETY: forwards to the system allocator, only counting.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            ALLOCS.fetch_add(1, Relaxed);
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { System.dealloc(p, l) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;
}

/// Allocations since the start (only in a `--features profile` build).
pub fn allocs() -> Option<u64> {
    #[cfg(feature = "profile")]
    return Some(counting::ALLOCS.load(Ordering::Relaxed));
    #[cfg(not(feature = "profile"))]
    None
}

// ---------------------------------------------------------------- memory

/// The process's private bytes (what Task Manager calls "Memory", more or less).
pub fn private_bytes() -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: `c` is a valid PROCESS_MEMORY_COUNTERS_EX (its first fields are a PROCESS_MEMORY_COUNTERS)
    // and `cb` is its size, as the API wants.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb).ok()? };
    Some(c.PrivateUsage as u64)
}

// ---------------------------------------------------------------- HUD

pub struct Hud {
    on: bool,
    begun: Option<Instant>,
    allocs_at_begin: u64,
    /// Frame CPU times (ms) of the last 120 frames.
    ms: VecDeque<f32>,
    /// When the last minute's frames ended (idle wake-ups per minute; frames in the last second).
    ends: VecDeque<Instant>,
    /// What asked for the frames of the last minute: (when, file, line, reason).
    causes: VecDeque<(Instant, &'static str, u32, String)>,
    frames: u64,
    allocs_last: u64,
    /// Refreshed once a second: private bytes and the section stats of the last second.
    slow: (Instant, Option<u64>, Vec<(&'static str, Stat)>),
}

impl Hud {
    pub fn new() -> Hud {
        Hud {
            on: std::env::var_os("KXS_HUD").is_some(),
            begun: None,
            allocs_at_begin: 0,
            ms: VecDeque::new(),
            ends: VecDeque::new(),
            causes: VecDeque::new(),
            frames: 0,
            allocs_last: 0,
            slow: (Instant::now(), None, vec![]),
        }
    }

    /// First thing in `App::ui`.
    pub fn begin(&mut self, ctx: &Context) {
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::F12)) {
            self.on = !self.on;
        }
        ON.store(self.on, Ordering::Relaxed);
        if self.on {
            (self.begun, self.allocs_at_begin) = (Some(Instant::now()), allocs().unwrap_or(0));
        }
    }

    /// Last thing in `App::ui`: records the frame and draws the overlay.
    pub fn end(&mut self, ctx: &Context) {
        let Some(t0) = self.begun.take().filter(|_| self.on) else { return };
        let now = Instant::now();
        self.frames += 1;
        self.allocs_last = allocs().unwrap_or(0).saturating_sub(self.allocs_at_begin);
        self.ms.push_back(t0.elapsed().as_secs_f32() * 1000.0);
        self.ends.push_back(now);
        while self.ms.len() > 120 {
            self.ms.pop_front();
        }
        while self.ends.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
            self.ends.pop_front();
        }
        self.causes.extend(ctx.repaint_causes().into_iter().map(|c| (now, c.file, c.line, c.reason.into_owned())));
        while self.causes.front().is_some_and(|c| now.duration_since(c.0) > Duration::from_secs(60)) {
            self.causes.pop_front();
        }
        if self.slow.0.elapsed() >= Duration::from_secs(1) {
            let mut sections: Vec<_> = SECTIONS.with(|s| std::mem::take(&mut *s.borrow_mut())).into_iter().collect();
            sections.sort_by(|a, b| b.1.total_ms.total_cmp(&a.1.total_ms));
            self.slow = (Instant::now(), private_bytes(), sections);
        }
        self.show(ctx, now);
    }

    fn show(&self, ctx: &Context, now: Instant) {
        let mut sorted: Vec<f32> = self.ms.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        let avg = sorted.iter().sum::<f32>() / sorted.len().max(1) as f32;
        let p95 = sorted.get((sorted.len() * 95 / 100).min(sorted.len().saturating_sub(1))).copied().unwrap_or(0.0);
        let fps = self.ends.iter().filter(|t| now.duration_since(**t) <= Duration::from_secs(1)).count();
        let mut text = format!(
            "frame {:.2} ms   avg {avg:.2}   p95 {p95:.2}\nframes {} total   {fps} in 1 s   {} in 60 s\n",
            self.ms.back().copied().unwrap_or(0.0),
            self.frames,
            self.ends.len(),
        );
        if let Some(b) = self.slow.1 {
            text += &format!("memory {:.1} MB private\n", b as f64 / 1_048_576.0);
        }
        // The glyph atlas lives in memory twice (here and on the GPU) and only grows.
        let [w, h] = ctx.fonts(|f| f.font_image_size());
        text += &format!("font atlas {w}x{h} ({:.1} MB)\n", (w * h * 4) as f64 / 1_048_576.0);
        if allocs().is_some() {
            text += &format!("allocs {} in the last frame\n", self.allocs_last);
        }
        // What asked for the frames of the last minute, most frequent first.
        let mut by_cause: BTreeMap<(&str, u32, &str), u32> = BTreeMap::new();
        for (_, file, line, reason) in &self.causes {
            *by_cause.entry((file, *line, reason)).or_default() += 1;
        }
        let mut by_cause: Vec<_> = by_cause.into_iter().collect();
        by_cause.sort_by_key(|c| std::cmp::Reverse(c.1));
        text += "repaint causes in 60 s:";
        for ((file, line, reason), n) in by_cause.iter().take(5) {
            text += &format!("\n  {n:>4}x {}:{line} {reason}", file.rsplit(['/', '\\']).next().unwrap_or(file));
        }
        if !self.slow.2.is_empty() {
            text += "\nsections (last second: n, total, max ms):";
            for (name, s) in self.slow.2.iter().take(8) {
                text += &format!("\n  {name:<22} {:>4} {:>7.2} {:>7.2}", s.n, s.total_ms, s.max_ms);
            }
        }
        egui::Area::new(egui::Id::new("kxs-hud")).order(egui::Order::Tooltip).anchor(Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0)).interactable(false).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).fill(Color32::from_black_alpha(215)).show(ui, |ui| {
                ui.label(RichText::new(text).monospace().size(11.5).color(Color32::from_rgb(120, 230, 160)));
            });
        });
    }
}

/// For the `#[ignore]` benchmarks: `cargo test --release -- --ignored --nocapture bench_`.
#[cfg(test)]
pub fn bench(label: &str, runs: u32, mut f: impl FnMut()) {
    f(); // warm up
    let t = Instant::now();
    for _ in 0..runs {
        f();
    }
    eprintln!("bench {label:<44} {:>9.3} ms", t.elapsed().as_secs_f64() * 1000.0 / f64::from(runs));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_timed_only_while_the_hud_is_on() {
        ON.store(false, Ordering::Relaxed);
        {
            crate::prof!("off");
        }
        ON.store(true, Ordering::Relaxed);
        {
            crate::prof!("on");
            std::thread::sleep(Duration::from_millis(2));
        }
        ON.store(false, Ordering::Relaxed);
        let s = SECTIONS.with(|s| std::mem::take(&mut *s.borrow_mut()));
        assert!(!s.contains_key("off"));
        assert!(s["on"].n == 1 && s["on"].total_ms >= 1.0);
    }

    #[test]
    fn private_bytes_are_reported() {
        assert!(private_bytes().is_some_and(|b| b > 1_000_000));
    }
}
