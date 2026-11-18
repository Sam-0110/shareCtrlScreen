//! A viewer session's **current surface** (plans/09 W2, 09a §3.2 and §4.2,
//! invariant J1): the one place that says which video surface the session
//! uses now, and the one path that hands it to another.
//!
//! Everything that touches the session's main surface — the registry slot
//! (surface-in-use checks), the transport driver (cursor control, the dead
//! link), the shortcut hook, the reaper — reads it here, so a move changes it
//! for all of them at once. The view gate keeps its own copy, swapped by
//! [`SurfaceCell::complete`] in order with its other operations ("hide the
//! old" queued before "show the new").
//!
//! **Who carries a move out.** Before the session is live nothing draws on the
//! surface, so a move only swaps it ([`Carrier::Swapped`]). Once live, the
//! viewer media thread does it: it builds a presenter on the new surface on
//! its decoder's device (the decoder stays, so no keyframe is needed), then
//! calls [`complete`](SurfaceCell::complete) and drops the old presenter. If
//! the media thread is gone (its decoder failed), the caller completes the
//! move itself ([`Carrier::Caller`]); the media thread, as it exits, completes
//! a move it had not reached.
//!
//! **Locks.** `state` is a leaf lock. `handoff` orders the calls that move
//! the input sinks and cursor control between surfaces (a move, the link
//! dying, the host changing control), so no surface keeps a sink or a hidden
//! OS cursor after it was let go. It is held across those surface calls: they
//! are quick, and where a backend waits for the UI thread (macOS) that wait is
//! bounded, so a UI thread waiting here is delayed, never deadlocked.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use parking_lot::Mutex;
use platform_api::{FileDrop, VideoInput, VideoSurface};

use super::active::SessionView;
use super::{Activity, Shared};
use crate::{MoveError, WindowId};

/// Where a move is going.
#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) surface: Arc<dyn VideoSurface>,
    pub(crate) window: WindowId,
}

/// What a live session has bound to its current surface, moved with it.
#[derive(Clone)]
pub(crate) struct Bound {
    pub(crate) input: Sender<VideoInput>,
    pub(crate) drops: Sender<FileDrop>,
    /// `control` (task D10): the new surface hides the OS cursor to match.
    pub(crate) shared: Arc<Shared>,
}

/// The session's view gate and activity, which follow a move.
#[derive(Clone)]
pub(crate) struct Gate {
    pub(crate) view: Arc<SessionView>,
    pub(crate) activity: Arc<Activity>,
}

/// The media thread's life, as moves see it. Only ever goes forward, so a
/// media thread that failed before the session was bound is not mistaken
/// for one that will carry a move out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Media {
    /// Not started: nothing draws on the surface.
    NotStarted,
    /// Running: it carries out moves (it rebuilds the presenter).
    Running,
    /// Gone: whoever asks for a move carries it out.
    Gone,
}

/// Who carries out a move [`SurfaceCell::request`] accepted.
pub(crate) enum Carrier {
    /// Nothing was bound yet: the surface is swapped already; release the
    /// old one.
    Swapped(Arc<dyn VideoSurface>),
    /// The media thread: it calls `complete` (or `fail`) shortly.
    Media,
    /// The caller: call `complete` now.
    Caller,
}

struct CellState {
    current: Arc<dyn VideoSurface>,
    window: WindowId,
    /// A move accepted and not yet carried out.
    target: Option<Target>,
    /// The session ended: no more moves, nothing is bound any more.
    retired: bool,
    bound: Option<Bound>,
    gate: Option<Gate>,
    media: Media,
}

pub(crate) struct SurfaceCell {
    state: Mutex<CellState>,
    handoff: Mutex<()>,
}

impl SurfaceCell {
    pub(crate) fn new(surface: Arc<dyn VideoSurface>, window: WindowId) -> Arc<Self> {
        Arc::new(SurfaceCell {
            state: Mutex::new(CellState {
                current: surface,
                window,
                target: None,
                retired: false,
                bound: None,
                gate: None,
                media: Media::NotStarted,
            }),
            handoff: Mutex::new(()),
        })
    }

    pub(crate) fn current(&self) -> Arc<dyn VideoSurface> {
        self.state.lock().current.clone()
    }

    pub(crate) fn window(&self) -> WindowId {
        self.state.lock().window
    }

    /// Whether `surface` is this session's, now or as a move's target.
    pub(crate) fn holds(&self, surface: &Arc<dyn VideoSurface>) -> bool {
        let s = self.state.lock();
        Arc::ptr_eq(&s.current, surface)
            || s.target
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(&t.surface, surface))
    }

    /// The session's view and activity exist: from now on a move takes
    /// them along. Before its media thread starts.
    pub(crate) fn set_gate(&self, gate: Gate) {
        self.state.lock().gate = Some(gate);
    }

    /// Its media thread is about to start: it carries out moves until it
    /// says it is gone ([`media_gone`](Self::media_gone)).
    pub(crate) fn media_started(&self) {
        let mut s = self.state.lock();
        if s.media == Media::NotStarted {
            s.media = Media::Running;
        }
    }

    /// The session went live: bind its sinks and cursor control to the
    /// current surface.
    pub(crate) fn bind(&self, bound: Bound) {
        let _order = self.handoff.lock();
        let current = {
            let mut s = self.state.lock();
            if s.retired {
                return;
            }
            s.bound = Some(bound.clone());
            s.current.clone()
        };
        attach(current.as_ref(), &bound);
    }

    /// Accept a move to `target` (checked by the registry under its lock:
    /// the surface is free and the session not starting). One at a time.
    pub(crate) fn request(&self, target: Target) -> Result<Carrier, MoveError> {
        let mut s = self.state.lock();
        if s.retired {
            return Err(MoveError::Ended);
        }
        if s.target.is_some() {
            return Err(MoveError::Busy);
        }
        if Arc::ptr_eq(&s.current, &target.surface) {
            return Err(MoveError::SurfaceInUse);
        }
        let carrier = match s.media {
            Media::NotStarted if s.bound.is_none() && s.gate.is_none() => {
                // Nothing draws on it, shows it or takes its input yet: a
                // swap is the whole move (09 §7, blocker 4 — a tab detached
                // while connecting goes live in its window).
                s.window = target.window;
                return Ok(Carrier::Swapped(std::mem::replace(
                    &mut s.current,
                    target.surface,
                )));
            }
            // Its presenter is on the current surface: only it can move.
            Media::Running => Carrier::Media,
            // Nothing draws; the view and sinks move at once.
            Media::NotStarted | Media::Gone => Carrier::Caller,
        };
        s.target = Some(target);
        Ok(carrier)
    }

    /// The media thread: a move waiting for it.
    pub(crate) fn pending(&self) -> Option<Target> {
        let s = self.state.lock();
        if s.retired {
            return None;
        }
        s.target.clone()
    }

    /// Carry the accepted move out: the target becomes current, the input
    /// and drop sinks and cursor control leave the old surface (held keys
    /// and buttons are released there, by the surface contract) and go to
    /// the new one, the view follows, and the shortcut hook re-installs on
    /// the new window. Returns the old surface for release, or `None` when
    /// the session ended meanwhile (nothing moved; the session's `Released`
    /// covers both surfaces).
    pub(crate) fn complete(&self) -> Option<Arc<dyn VideoSurface>> {
        let _order = self.handoff.lock();
        let (old, new, window, bound, gate) = {
            let mut s = self.state.lock();
            if s.retired {
                s.target = None;
                return None;
            }
            let target = s.target.take()?;
            let old = std::mem::replace(&mut s.current, target.surface.clone());
            s.window = target.window;
            (
                old,
                target.surface,
                target.window,
                s.bound.clone(),
                s.gate.clone(),
            )
        };
        if bound.is_some() {
            detach(old.as_ref());
        }
        if let Some(g) = &gate {
            g.view.rebind(new.clone(), window);
        }
        if let Some(b) = &bound {
            attach(new.as_ref(), b);
        }
        if let Some(g) = &gate {
            g.activity.rebind(&g.view);
        }
        Some(old)
    }

    /// The accepted move could not be carried out: the session stays where
    /// it is. Returns the target for release — not once the session ended:
    /// its `Released` covers the target then.
    pub(crate) fn fail(&self) -> Option<Arc<dyn VideoSurface>> {
        let mut s = self.state.lock();
        let target = s.target.take()?;
        (!s.retired).then_some(target.surface)
    }

    /// The media thread exits: later moves are the caller's, and one it had
    /// not reached is carried out now. Returns the surface to release.
    pub(crate) fn media_gone(&self) -> Option<Arc<dyn VideoSurface>> {
        self.state.lock().media = Media::Gone;
        // Nothing to do without a move waiting.
        self.complete()
    }

    /// The host changed this viewer's control (task D10): the current surface
    /// hides or restores the OS cursor to match. In order with a move, so
    /// the surface left behind never keeps the cursor hidden.
    pub(crate) fn set_cursor_control(&self, control: bool) {
        let _order = self.handoff.lock();
        let (current, retired) = {
            let s = self.state.lock();
            (s.current.clone(), s.retired)
        };
        if !retired {
            current.set_cursor_control(control);
        }
    }

    /// The session ended (or its link died): no more moves, and the current
    /// surface stops taking input (held keys released first) and drops, with
    /// the OS cursor restored. Synchronous, so an ended session never keeps
    /// capturing input while its threads are joined. Hiding is the view's
    /// (`SessionView::retire`). Idempotent.
    pub(crate) fn retire(&self) {
        let _order = self.handoff.lock();
        let current = {
            let mut s = self.state.lock();
            s.retired = true;
            s.bound = None;
            s.gate = None;
            s.current.clone()
        };
        detach(current.as_ref());
    }
}

/// Give `surface` the session's sinks and cursor control.
fn attach(surface: &dyn VideoSurface, b: &Bound) {
    surface.set_drop_sink(Some(b.drops.clone()));
    surface.set_input_sink(Some(b.input.clone()));
    surface.set_cursor_control(b.shared.control.load(Ordering::SeqCst));
}

/// Take them away again: input stops (held input released first, by the
/// surface contract), drops are refused, the OS cursor comes back.
pub(crate) fn detach(surface: &dyn VideoSurface) {
    surface.set_input_sink(None);
    surface.set_drop_sink(None);
    surface.set_cursor_control(false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockSurface;

    fn surface() -> Arc<dyn VideoSurface> {
        MockSurface::new((10, 10))
    }

    fn target(surface: &Arc<dyn VideoSurface>, window: u32) -> Target {
        Target {
            surface: surface.clone(),
            window: WindowId(window),
        }
    }

    #[test]
    fn one_move_at_a_time_and_the_media_thread_carries_it() {
        let (a, b, c) = (surface(), surface(), surface());
        let cell = SurfaceCell::new(a.clone(), WindowId::MAIN);
        cell.media_started();
        assert!(matches!(cell.request(target(&b, 1)), Ok(Carrier::Media)));
        assert!(cell.holds(&b), "a pending target is taken");
        assert!(matches!(cell.request(target(&c, 2)), Err(MoveError::Busy)));
        assert!(cell.pending().is_some());
        let old = cell.complete().expect("moved");
        assert!(Arc::ptr_eq(&old, &a));
        assert!(Arc::ptr_eq(&cell.current(), &b));
        assert_eq!(cell.window(), WindowId(1));
        assert!(cell.pending().is_none());
    }

    #[test]
    fn a_media_thread_gone_before_the_bind_hands_moves_to_the_caller() {
        let (a, b) = (surface(), surface());
        let cell = SurfaceCell::new(a, WindowId::MAIN);
        cell.media_started();
        // Its presenter failed at once: gone before the session was bound.
        assert!(cell.media_gone().is_none());
        cell.media_started();
        let (input, _in_rx) = std::sync::mpsc::channel();
        let (drops, _drop_rx) = std::sync::mpsc::channel();
        cell.bind(Bound {
            input,
            drops,
            shared: Arc::default(),
        });
        assert!(
            matches!(cell.request(target(&b, 1)), Ok(Carrier::Caller)),
            "never handed to a media thread that is gone"
        );
        assert!(cell.complete().is_some());
    }

    #[test]
    fn a_move_overtaken_by_the_end_releases_nothing_itself() {
        let (a, b, c) = (surface(), surface(), surface());
        let cell = SurfaceCell::new(a.clone(), WindowId::MAIN);
        cell.media_started();
        assert!(cell.request(target(&b, 1)).is_ok());
        cell.retire();
        assert!(cell.pending().is_none());
        assert!(cell.complete().is_none(), "Released covers both");
        assert!(Arc::ptr_eq(&cell.current(), &a));
        let cell = SurfaceCell::new(a, WindowId::MAIN);
        cell.media_started();
        assert!(cell.request(target(&c, 1)).is_ok());
        cell.retire();
        assert!(cell.fail().is_none(), "Released covers the target");
    }

    #[test]
    fn before_the_session_is_live_a_move_is_a_swap() {
        let (a, b) = (surface(), surface());
        let cell = SurfaceCell::new(a.clone(), WindowId::MAIN);
        let target = Target {
            surface: b.clone(),
            window: WindowId(2),
        };
        match cell.request(target) {
            Ok(Carrier::Swapped(old)) => assert!(Arc::ptr_eq(&old, &a)),
            _ => panic!("expected a swap"),
        }
        assert!(Arc::ptr_eq(&cell.current(), &b));
        assert_eq!(cell.window(), WindowId(2));
        assert!(!cell.holds(&a));
        let back = Target {
            surface: b,
            window: WindowId::MAIN,
        };
        assert!(matches!(cell.request(back), Err(MoveError::SurfaceInUse)));
    }

    #[test]
    fn an_ended_session_does_not_move() {
        let (a, b) = (surface(), surface());
        let cell = SurfaceCell::new(a, WindowId::MAIN);
        cell.retire();
        let target = Target {
            surface: b,
            window: WindowId(1),
        };
        assert!(matches!(cell.request(target), Err(MoveError::Ended)));
        assert!(cell.pending().is_none());
    }
}
