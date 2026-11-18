//! The ActiveGate (plans/08 P3, 08a §2.2, §6.4, §6.18, invariants I2/I7): the
//! one place that decides whether a viewer session's main video is shown.
//!
//! A session's video is shown exactly when it is **active** (the tab on
//! screen), **ready** (its decoder and presenter exist), its link is **not in
//! trouble** and **no web overlay** (Settings, a confirm) is open over the
//! video. Nothing else in the engine shows or hides a main surface.
//!
//! **Ordering without locks held across the OS.** Backends' `show`/`hide` may
//! run synchronously on the UI thread (macOS dispatches to the main thread), so
//! the gate never calls them while holding a lock: each change is decided under
//! the view's own small lock and the resulting operation is queued, in order,
//! to one applier thread that performs it. Because one thread performs every
//! operation in the order decided, a switch's "hide the old tab" always
//! happens before its "show the new one" (X11's `XdndProxy` and XI2 grab need
//! that, 08a §6.4 step 3).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use platform_api::VideoSurface;

use super::Shared;

/// One show/hide, performed by the applier.
pub(crate) enum ViewOp {
    /// Show, then ask the present loop to repaint the held frame (D12):
    /// re-showing does not repaint, and a static screen would stay black.
    Show(Arc<dyn VideoSurface>, Arc<Shared>),
    Hide(Arc<dyn VideoSurface>),
    /// Answered once every op queued before it has been performed.
    Barrier(tokio::sync::oneshot::Sender<()>),
}

/// The applier queue: ops sent to it run one at a time, in order.
pub(crate) type ViewOps = tokio::sync::mpsc::UnboundedSender<ViewOp>;

/// Starts the applier on `rt`. Each op runs on a blocking-pool thread (the
/// backend may wait on the UI thread) and the next one starts only after it
/// returns, so the order decided is the order performed. Spawning a task
/// cannot fail, so the gate never has to fall back to calling the OS on a
/// caller that may hold a lock.
pub(crate) fn applier(rt: &tokio::runtime::Handle) -> ViewOps {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ViewOp>();
    rt.spawn(async move {
        while let Some(op) = rx.recv().await {
            if let Err(e) = tokio::task::spawn_blocking(move || perform(op)).await {
                tracing::error!("a video show/hide failed: {e}");
            }
        }
    });
    tx
}

fn perform(op: ViewOp) {
    match op {
        ViewOp::Show(surface, shared) => {
            surface.show();
            shared.repaint_requested.store(true, Ordering::SeqCst);
        }
        ViewOp::Hide(surface) => surface.hide(),
        ViewOp::Barrier(done) => {
            let _ = done.send(());
        }
    }
}

#[derive(Default)]
struct ViewState {
    active: bool,
    ready: bool,
    trouble: bool,
    /// Ended: never shown again.
    gone: bool,
    /// What was last queued.
    shown: bool,
}

/// One viewer session's main video, as the gate sees it.
pub(crate) struct SessionView {
    surface: Arc<dyn VideoSurface>,
    shared: Arc<Shared>,
    /// The overlay flag is engine-wide: one web dialog covers every tab.
    overlay: Arc<AtomicBool>,
    /// The applier queue (shared by every view: one order for all of them).
    ops: ViewOps,
    state: Mutex<ViewState>,
    /// Lock-free copy of `state.shown` for the present loop (B5): a hidden
    /// video is decoded (its reference frames stay current) but not presented.
    shown: AtomicBool,
}

impl SessionView {
    pub(crate) fn new(
        surface: Arc<dyn VideoSurface>,
        shared: Arc<Shared>,
        overlay: Arc<AtomicBool>,
        ops: ViewOps,
        active: bool,
    ) -> Arc<Self> {
        Arc::new(SessionView {
            surface,
            shared,
            overlay,
            ops,
            state: Mutex::new(ViewState {
                active,
                ..ViewState::default()
            }),
            shown: AtomicBool::new(false),
        })
    }

    /// Whether its video is (to be) shown: the present loop presents only then.
    pub(crate) fn is_shown(&self) -> bool {
        self.shown.load(Ordering::SeqCst)
    }

    /// Decoder and presenter exist: the video may be shown.
    pub(crate) fn set_ready(&self) {
        self.update(|s| s.ready = true);
    }

    /// The link went quiet (hide: a frozen frame looks live) or answered
    /// again.
    pub(crate) fn set_trouble(&self, trouble: bool) {
        self.update(|s| s.trouble = trouble);
    }

    pub(crate) fn set_active(&self, active: bool) {
        self.update(|s| s.active = active);
    }

    /// An overlay opened or closed (the engine-wide flag changed already).
    pub(crate) fn refresh(&self) {
        self.update(|_| {});
    }

    /// The session ended: hide now (after anything queued before) and never
    /// show again.
    pub(crate) fn retire(&self) {
        self.update(|s| s.gone = true);
        // Queued even when it was never shown, after everything queued
        // before: the surface is hidden for certain once the session is over.
        self.queue(ViewOp::Hide(self.surface.clone()));
    }

    fn update(&self, change: impl FnOnce(&mut ViewState)) {
        let mut s = self.state.lock();
        change(&mut s);
        let want =
            !s.gone && s.active && s.ready && !s.trouble && !self.overlay.load(Ordering::SeqCst);
        if want == s.shown {
            return;
        }
        s.shown = want;
        self.shown.store(want, Ordering::SeqCst);
        let op = if want {
            ViewOp::Show(self.surface.clone(), self.shared.clone())
        } else {
            ViewOp::Hide(self.surface.clone())
        };
        // Queued under the view's lock so this view's operations keep the
        // order they were decided in; sending never blocks.
        self.queue(op);
    }

    /// Hand `op` to the applier. Never blocks. A send fails only once the
    /// runtime is gone (app exit), when no window is left to show or hide.
    fn queue(&self, op: ViewOp) {
        let _ = self.ops.send(op);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockSurface;
    use std::time::{Duration, Instant};

    fn settle(surface: &MockSurface, visible: bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while surface.state().visible != visible {
            assert!(
                Instant::now() < deadline,
                "surface never became visible={visible}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn shown_only_when_active_ready_untroubled_and_uncovered() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ops = applier(rt.handle());
        let overlay = Arc::new(AtomicBool::new(false));
        let surface = MockSurface::new((640, 360));
        let view = SessionView::new(
            surface.clone(),
            Arc::new(Shared::default()),
            overlay.clone(),
            ops,
            false,
        );
        view.set_ready();
        assert!(!view.is_shown(), "not active");
        view.set_active(true);
        assert!(view.is_shown());
        settle(&surface, true);

        view.set_trouble(true);
        assert!(!view.is_shown());
        settle(&surface, false);
        view.set_trouble(false);
        settle(&surface, true);

        overlay.store(true, Ordering::SeqCst);
        view.refresh();
        assert!(!view.is_shown(), "a web dialog covers it");
        settle(&surface, false);
        overlay.store(false, Ordering::SeqCst);
        view.refresh();
        settle(&surface, true);

        view.retire();
        settle(&surface, false);
        view.set_active(true);
        view.refresh();
        assert!(!view.is_shown(), "an ended session never shows again");
    }

    #[test]
    fn switching_hides_the_old_video_before_showing_the_new_one() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ops = applier(rt.handle());
        let overlay = Arc::new(AtomicBool::new(false));
        let (a, b) = (MockSurface::new((10, 10)), MockSurface::new((10, 10)));
        let va = SessionView::new(
            a.clone(),
            Arc::default(),
            overlay.clone(),
            ops.clone(),
            true,
        );
        let vb = SessionView::new(b.clone(), Arc::default(), overlay, ops, false);
        va.set_ready();
        vb.set_ready();
        settle(&a, true);
        for _ in 0..20 {
            va.set_active(false);
            vb.set_active(true);
            settle(&b, true);
            assert!(!a.state().visible);
            vb.set_active(false);
            va.set_active(true);
            settle(&a, true);
            assert!(!b.state().visible);
        }
    }
}
