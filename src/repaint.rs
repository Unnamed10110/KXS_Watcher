//! When background work may wake the window: not while it is minimized, and what is not on screen (the
//! list of a hidden page) only now and then. Every frame asks the OS for nothing; the tasks do.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

static ACTIVE: AtomicBool = AtomicBool::new(true);
static FRAME: AtomicU64 = AtomicU64::new(0);

/// What is not on screen still wakes the window this often (counts in the sidebar stay current).
const HIDDEN_EVERY: Duration = Duration::from_secs(2);

/// Called at the start of every frame.
pub fn begin_frame(ctx: &egui::Context) {
    FRAME.fetch_add(1, Ordering::Relaxed);
    let hidden = ctx.input(|i| i.viewport().minimized == Some(true) || i.viewport().occluded == Some(true));
    ACTIVE.store(!hidden, Ordering::Relaxed);
}

/// A frame after `delay`, unless the window is minimized: restoring it paints anyway.
pub fn after(ctx: &egui::Context, delay: Duration) {
    if ACTIVE.load(Ordering::Relaxed) {
        ctx.request_repaint_after(delay);
    }
}

pub fn now(ctx: &egui::Context) {
    after(ctx, Duration::ZERO);
}

/// Marks something the UI draws and a background task feeds: the code that draws it calls `mark` on
/// every frame, the task asks `wake` for frames and gets them at its own pace only while the mark is
/// fresh (a few frames old at most). A window that draws nothing keeps its marks, so what was on screen
/// stays on screen.
#[derive(Clone, Default)]
pub struct Shown(Arc<AtomicU64>);

impl Shown {
    pub fn mark(&self) {
        self.0.store(FRAME.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    pub fn is(&self) -> bool {
        FRAME.load(Ordering::Relaxed) <= self.0.load(Ordering::Relaxed) + 3
    }

    /// A frame after `delay` if this is on screen, else after `HIDDEN_EVERY` at the soonest.
    pub fn wake(&self, ctx: &egui::Context, delay: Duration) {
        after(ctx, if self.is() { delay } else { delay.max(HIDDEN_EVERY) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_not_drawn_wakes_the_window_slowly() {
        let shown = Shown::default();
        shown.mark();
        assert!(shown.is());
        let mut h = crate::testkit::Harness::new(egui::vec2(100.0, 100.0));
        let ctx = h.ctx.clone();
        // The first frames of a context ask for more frames: the last of a few is the one that counts.
        let delay = |h: &mut crate::testkit::Harness| (0..4).map(|_| h.frame_delay(vec![], |_| shown.wake(&ctx, Duration::from_millis(250)))).last().unwrap();
        // Frames go by without it being drawn.
        for _ in 0..5 {
            FRAME.fetch_add(1, Ordering::Relaxed);
        }
        assert!(!shown.is());
        let d = delay(&mut h);
        assert!(d > Duration::from_millis(1500), "hidden: every 2 s, not 4 times a second: {d:?}");
        shown.mark();
        let d = delay(&mut h);
        assert!(d <= Duration::from_millis(250), "drawn: at its own pace: {d:?}");
    }
}
