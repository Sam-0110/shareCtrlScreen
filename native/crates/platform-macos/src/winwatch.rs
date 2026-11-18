//! Activation and minimise of a top-level app window (plans/09 W3, 09a §3.3):
//! `NSWindowDidBecomeKey` / `DidResignKey` (the window has the keyboard — the
//! video view inside it included) and `DidMiniaturize` / `DidDeminiaturize`.
//! tao forwards key-window changes but has no miniaturize handler (09 §7), so
//! both come from the notification center here, through one path.
//!
//! Everything runs on the main thread: AppKit posts these notifications on
//! it, the observers are added there, and the token removes them there.
//!
//! ## References
//! * `NSNotificationCenter addObserverForName:object:queue:usingBlock:` (a nil
//!   queue runs the block on the posting thread) and `removeObserver:`:
//!   <https://developer.apple.com/documentation/foundation/nsnotificationcenter>
//! * `NSWindowDidBecomeKeyNotification`, `NSWindowDidResignKeyNotification`,
//!   `NSWindowDidMiniaturizeNotification`, `NSWindowDidDeminiaturizeNotification`,
//!   `isKeyWindow`, `isMiniaturized`:
//!   <https://developer.apple.com/documentation/appkit/nswindow>

use std::cell::Cell;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_app_kit::{
    NSView, NSWindow, NSWindowDidBecomeKeyNotification, NSWindowDidDeminiaturizeNotification,
    NSWindowDidMiniaturizeNotification, NSWindowDidResignKeyNotification,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter, NSObjectProtocol};
use platform_api::{RawWindowHandle, SurfaceError, WindowActivity, WindowActivityFn, WindowWatch};

pub(crate) struct ActivationWatch {
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

// SAFETY: the observers (and the blocks they hold, with their `Rc`s) are only
// used on the main thread: the blocks run there, and `Drop` removes them
// there — dropped anywhere else they are leaked, never released while a
// block could be running.
unsafe impl Send for ActivationWatch {}

impl WindowWatch for ActivationWatch {}

impl Drop for ActivationWatch {
    fn drop(&mut self) {
        if MainThreadMarker::new().is_none() {
            // A contract breach: releasing here could race a delivery.
            tracing::error!("window watch dropped off the main thread; leaked");
            std::mem::forget(std::mem::take(&mut self.observers));
            return;
        }
        let center = NSNotificationCenter::defaultCenter();
        for observer in self.observers.drain(..) {
            // SAFETY: an observer this watch added to the default center.
            unsafe { center.removeObserver(&observer) };
        }
    }
}

/// Watch the window that holds the view behind `window`. Call on the main
/// thread.
pub(crate) fn watch(
    window: RawWindowHandle,
    on_change: WindowActivityFn,
) -> Result<Box<dyn WindowWatch>, SurfaceError> {
    let RawWindowHandle::AppKit(handle) = window else {
        return Err(SurfaceError::UnsupportedHandle);
    };
    if MainThreadMarker::new().is_none() {
        return Err(SurfaceError::Backend(
            "watch the window from the main (UI) thread".into(),
        ));
    }
    // SAFETY: an AppKit handle's `ns_view` is a valid NSView while its window
    // exists (raw-window-handle contract); the caller passes a live window.
    let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let win: Retained<NSWindow> = view
        .window()
        .ok_or_else(|| SurfaceError::Backend("the view is not in a window".into()))?;
    let on_change: Rc<dyn Fn(WindowActivity) + Send + Sync> = Rc::from(on_change);
    let last: Rc<Cell<Option<WindowActivity>>> = Rc::default();
    let report = Rc::new(move |w: &NSWindow| {
        let minimized = w.isMiniaturized();
        let now = WindowActivity {
            active: w.isKeyWindow() && !minimized,
            minimized: Some(minimized),
        };
        if last.get() != Some(now) {
            last.set(Some(now));
            on_change(now);
        }
    });
    let center = NSNotificationCenter::defaultCenter();
    // SAFETY: AppKit notification-name constants, valid for the process
    // lifetime.
    let names = unsafe {
        [
            NSWindowDidBecomeKeyNotification,
            NSWindowDidResignKeyNotification,
            NSWindowDidMiniaturizeNotification,
            NSWindowDidDeminiaturizeNotification,
        ]
    };
    let target: &AnyObject = &win;
    let mut observers = Vec::with_capacity(names.len());
    for name in names {
        let (report, w) = (report.clone(), win.clone());
        let block = RcBlock::new(move |_note: NonNull<NSNotification>| report(&w));
        // SAFETY: the block takes exactly the one notification argument the
        // center passes; with no queue it runs on the posting (main) thread.
        observers.push(unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                Some(target),
                None,
                &block,
            )
        });
    }
    report(&win);
    Ok(Box::new(ActivationWatch { observers }))
}
