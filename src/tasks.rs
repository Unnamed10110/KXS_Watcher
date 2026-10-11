//! Background work handed to the UI: tasks that stop when dropped, and one-shot results polled each frame.

use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// `lock()` that goes on after a task panicked while holding the mutex: what is behind it is the
/// cluster's state or a log, still fine to show, and a dead task must not take the window with it.
pub trait Lock<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> Lock<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Task handle that aborts on drop: whoever shows the data owns the task.
pub struct Bg(tokio::task::JoinHandle<()>);

impl Bg {
    pub fn spawn(f: impl Future<Output = ()> + Send + 'static) -> Self {
        Bg(tokio::spawn(f))
    }
}

impl Drop for Bg {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What a [`Pending`] reports when its task died without an answer (it panicked): an error for a
/// result, nothing for a list. Without it the UI would wait (and spin) for ever.
pub trait TaskLost {
    fn lost() -> Self;
}

impl<T> TaskLost for Result<T, String> {
    fn lost() -> Self {
        Err("the background task stopped unexpectedly".into())
    }
}

impl<T> TaskLost for Vec<T> {
    fn lost() -> Self {
        Vec::new()
    }
}

impl<A: TaskLost, B: TaskLost> TaskLost for (A, B) {
    fn lost() -> Self {
        (A::lost(), B::lost())
    }
}

/// One-shot async result polled by the UI each frame. The `bool`: the answer was handed out.
pub struct Pending<T>(tokio::sync::oneshot::Receiver<T>, bool);

impl<T: Send + 'static> Pending<T> {
    pub fn spawn(ctx: &egui::Context, f: impl Future<Output = T> + Send + 'static) -> Self {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = tx.send(f.await);
            ctx.request_repaint();
        });
        Pending(rx, false)
    }
}

impl<T: TaskLost> Pending<T> {
    /// `Some` exactly once, when the task finished (or died: [`TaskLost`]).
    pub fn poll(&mut self) -> Option<T> {
        use tokio::sync::oneshot::error::TryRecvError;
        if self.1 {
            return None;
        }
        let r = match self.0.try_recv() {
            Ok(v) => v,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Closed) => T::lost(),
        };
        self.1 = true;
        Some(r)
    }
}

/// Poll an optional pending slot, clearing it once done.
pub fn take<T: TaskLost>(slot: &mut Option<Pending<T>>) -> Option<T> {
    let r = slot.as_mut()?.poll();
    if r.is_some() {
        *slot = None;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn answer<T: TaskLost>(p: &mut Pending<T>) -> Option<T> {
        for _ in 0..200 {
            if let Some(v) = p.poll() {
                return Some(v);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        None
    }

    #[test]
    fn a_mutex_poisoned_by_a_panic_still_locks() {
        let m = std::sync::Arc::new(Mutex::new(5));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.locked();
            panic!("a task died holding the lock");
        })
        .join();
        assert!(m.lock().is_err(), "poisoned");
        assert_eq!(*m.locked(), 5);
    }

    #[tokio::test]
    async fn a_result_arrives_once() {
        let mut p = Pending::spawn(&egui::Context::default(), async { Ok::<_, String>(7) });
        assert_eq!(answer(&mut p).await, Some(Ok(7)));
        assert_eq!(p.poll(), None, "handed out once");
    }

    #[tokio::test]
    async fn a_task_that_panics_ends_with_an_error_instead_of_spinning() {
        let mut p: Pending<Result<u8, String>> = Pending::spawn(&egui::Context::default(), async { panic!("boom") });
        let r = answer(&mut p).await.expect("the UI is told");
        assert!(r.is_err_and(|e| e.contains("stopped unexpectedly")));
        assert_eq!(p.poll(), None);
        let mut slot: Option<Pending<Vec<u8>>> = Some(Pending::spawn(&egui::Context::default(), async { panic!("boom") }));
        for _ in 0..200 {
            if take(&mut slot).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(slot.is_none(), "the slot is cleared, a spinner would stop");
    }

    #[tokio::test]
    async fn a_dropped_background_task_stops() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        let bg = Bg::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            f.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        drop(bg);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!flag.load(std::sync::atomic::Ordering::SeqCst));
    }
}
