//! The ActiveGate (plans/08 P3, 08a §2.2, §6.4, §6.18, invariants I2/I7;
//! plans/09 W1, 09a §3.2): the one place that decides whether a viewer
//! session's main video is shown.
//!
//! A session's video is shown exactly when it is **on screen** (the strip's
//! active tab, or a detached window that is not minimised), **ready** (its
//! decoder and presenter exist), its link is **not in trouble** and **no web
//! overlay** (Settings, a confirm) is open **in its window**. Nothing else in
//! the engine shows or hides a main surface. Several sessions may be on screen
//! at once; one of them is **focused** (it has the keyboard, the clipboard and
//! the shortcut hook — see `TabGate`).
//!
//! **Ordering without locks held across the OS.** Backends' `show`/`hide` may
//! run synchronously on the UI thread (macOS dispatches to the main thread), so
//! the gate never calls them while holding a lock: each change is decided under
//! the view's own small lock and the resulting operation is queued, in order,
//! to one applier thread that performs it. Because one thread performs every
//! operation in the order decided, a switch's "hide the old tab" always
//! happens before its "show the new one" (X11's `XdndProxy` and XI2 grab need
//! that, 08a §6.4 step 3).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use platform_api::VideoSurface;

use super::Shared;

/// The windows with a web dialog open over their video (by `WindowId` value).
/// Shared by every view; a leaf lock (nothing is taken while it is held).
pub(crate) type Overlays = Arc<Mutex<BTreeSet<u32>>>;

/// One show/hide, performed by the applier.
pub(crate) enum ViewOp {
    /// Show, then ask the present loop to repaint the held frame (D12):
    /// re-showing does not repaint, and a static screen would stay black.
    /// `focus`: the first show since the session became focused also gives it
    /// the keyboard; a re-show (link restored, a dialog closed) does not, so it
    /// never takes the keys from a web field the user is typing in.
    Show {
        surface: Arc<dyn VideoSurface>,
        shared: Arc<Shared>,
        focus: bool,
    },
    Hide(Arc<dyn VideoSurface>),
    /// A shown video's session became focused: it takes the keyboard.
    Focus(Arc<dyn VideoSurface>),
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
        ViewOp::Show {
            surface,
            shared,
            focus,
        } => {
            surface.show();
            if focus {
                surface.focus();
            }
            shared.repaint_requested.store(true, Ordering::SeqCst);
        }
        ViewOp::Hide(surface) => surface.hide(),
        ViewOp::Focus(surface) => surface.focus(),
        ViewOp::Barrier(done) => {
            let _ = done.send(());
        }
    }
}

#[derive(Default)]
struct ViewState {
    on_screen: bool,
    focused: bool,
    ready: bool,
    trouble: bool,
    /// Ended: never shown again.
    gone: bool,
    /// What was last queued.
    shown: bool,
    /// Became focused and not shown since: the next show takes the keyboard.
    focus_owed: bool,
}

/// One viewer session's main video, as the gate sees it.
pub(crate) struct SessionView {
    surface: Arc<dyn VideoSurface>,
    shared: Arc<Shared>,
    /// Which windows have a web dialog open; this view is covered when its
    /// own window has one.
    overlays: Overlays,
    /// The window its video is in (`WindowId` value; 0 = the main window).
    window: AtomicU32,
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
        overlays: Overlays,
        ops: ViewOps,
    ) -> Arc<Self> {
        Arc::new(SessionView {
            surface,
            shared,
            overlays,
            window: AtomicU32::new(0),
            ops,
            state: Mutex::new(ViewState::default()),
            shown: AtomicBool::new(false),
        })
    }

    /// Whether its video is (to be) shown: the present loop presents only then.
    pub(crate) fn is_shown(&self) -> bool {
        self.shown.load(Ordering::SeqCst)
    }

    /// The window its video is in.
    pub(crate) fn window(&self) -> u32 {
        self.window.load(Ordering::SeqCst)
    }

    /// A web dialog is open over its window.
    pub(crate) fn covered(&self) -> bool {
        self.overlays.lock().contains(&self.window())
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

    pub(crate) fn set_on_screen(&self, on: bool) {
        self.update(|s| s.on_screen = on);
    }

    /// Became (or stopped being) the focused session. Becoming focused gives
    /// its video the keyboard: at once when it is shown, else on its next
    /// show.
    pub(crate) fn set_focused(&self, focused: bool) {
        let mut s = self.state.lock();
        let became = focused && !s.focused;
        s.focused = focused;
        if !became {
            s.focus_owed &= focused;
            return;
        }
        if s.shown {
            // Under the view's lock, like every op of this view: ordered.
            self.queue(ViewOp::Focus(self.surface.clone()));
        } else {
            s.focus_owed = true;
        }
    }

    /// A web dialog opened or closed (the overlay set changed already).
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
        let want = !s.gone && s.on_screen && s.ready && !s.trouble && !self.covered();
        if want == s.shown {
            return;
        }
        s.shown = want;
        self.shown.store(want, Ordering::SeqCst);
        let op = if want {
            ViewOp::Show {
                surface: self.surface.clone(),
                shared: self.shared.clone(),
                focus: std::mem::take(&mut s.focus_owed),
            }
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
    fn shown_only_when_on_screen_ready_untroubled_and_uncovered() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ops = applier(rt.handle());
        let overlays = Overlays::default();
        let surface = MockSurface::new((640, 360));
        let view = SessionView::new(
            surface.clone(),
            Arc::new(Shared::default()),
            overlays.clone(),
            ops,
        );
        view.set_ready();
        assert!(!view.is_shown(), "not on screen");
        view.set_on_screen(true);
        assert!(view.is_shown());
        settle(&surface, true);

        view.set_trouble(true);
        assert!(!view.is_shown());
        settle(&surface, false);
        view.set_trouble(false);
        settle(&surface, true);

        // A dialog in another window does not cover it; one in its own does.
        overlays.lock().insert(1);
        view.refresh();
        assert!(view.is_shown(), "another window's dialog");
        overlays.lock().insert(0);
        view.refresh();
        assert!(!view.is_shown(), "a web dialog covers it");
        settle(&surface, false);
        overlays.lock().clear();
        view.refresh();
        settle(&surface, true);

        view.retire();
        settle(&surface, false);
        view.set_on_screen(true);
        view.refresh();
        assert!(!view.is_shown(), "an ended session never shows again");
    }

    #[test]
    fn switching_hides_the_old_video_before_showing_the_new_one() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ops = applier(rt.handle());
        let overlays = Overlays::default();
        let (a, b) = (MockSurface::new((10, 10)), MockSurface::new((10, 10)));
        let va = SessionView::new(a.clone(), Arc::default(), overlays.clone(), ops.clone());
        let vb = SessionView::new(b.clone(), Arc::default(), overlays, ops);
        va.set_on_screen(true);
        va.set_ready();
        vb.set_ready();
        settle(&a, true);
        for _ in 0..20 {
            va.set_on_screen(false);
            vb.set_on_screen(true);
            settle(&b, true);
            assert!(!a.state().visible);
            vb.set_on_screen(false);
            va.set_on_screen(true);
            settle(&a, true);
            assert!(!b.state().visible);
        }
    }

    #[test]
    fn several_on_screen_and_focus_goes_to_the_focused_one() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ops = applier(rt.handle());
        let overlays = Overlays::default();
        let (a, b) = (MockSurface::new((10, 10)), MockSurface::new((10, 10)));
        let va = SessionView::new(a.clone(), Arc::default(), overlays.clone(), ops.clone());
        let vb = SessionView::new(b.clone(), Arc::default(), overlays, ops);
        for v in [&va, &vb] {
            v.set_ready();
            v.set_on_screen(true);
        }
        settle(&a, true);
        settle(&b, true);
        vb.set_focused(true);
        let deadline = Instant::now() + Duration::from_secs(2);
        while b.state().focus_requests == 0 {
            assert!(
                Instant::now() < deadline,
                "the focused video never took the keys"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(a.state().focus_requests, 0);
        assert!(a.state().visible, "the unfocused one stays on screen");
    }
}
