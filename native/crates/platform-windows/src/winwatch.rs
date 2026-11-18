//! Activation and minimise of a top-level app window (plans/09 W3, 09a §3.3).
//!
//! Tauri's `Focused` event on Windows follows WebView2's focus, so a click into
//! the native video child reports the window as *unfocused* (09 §7, blocker
//! 1). The window manager's own messages do not have that problem: the
//! top-level gets `WM_ACTIVATE` whenever it (or any child, the video included)
//! becomes or stops being the active window, and `WM_SIZE` with
//! `SIZE_MINIMIZED` when it is minimised. A comctl32 subclass sees both
//! without replacing tao's window procedure.
//!
//! ## References
//! * `SetWindowSubclass` / `RemoveWindowSubclass` / `DefSubclassProc` (same
//!   thread as the window; remove by `WM_NCDESTROY` at the latest):
//!   <https://learn.microsoft.com/windows/win32/controls/subclassing-overview>
//! * `WM_ACTIVATE` (`WA_INACTIVE` in the low word of `wParam`):
//!   <https://learn.microsoft.com/windows/win32/inputdev/wm-activate>
//! * `WM_SIZE` (`SIZE_MINIMIZED`, `SIZE_RESTORED`, `SIZE_MAXIMIZED`):
//!   <https://learn.microsoft.com/windows/win32/winmsg/wm-size>
//! * `GetAncestor(GA_ROOT)`, `PostMessageW`, `RegisterWindowMessageW`,
//!   `IsIconic`, `GetForegroundWindow`, `GetWindowThreadProcessId`:
//!   <https://learn.microsoft.com/windows/win32/api/winuser/>

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use platform_api::{SurfaceError, WindowActivity, WindowActivityFn, WindowWatch};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, PostMessageW,
    RegisterWindowMessageW, GA_ROOT, SIZE_MINIMIZED, WA_INACTIVE, WM_ACTIVATE, WM_NCDESTROY,
    WM_SIZE,
};

/// The message a watch dropped off the UI thread posts to its window, so the
/// window's own thread removes the subclass and frees it (`lParam` = the
/// data). Registered, so it collides with no other message.
fn unwatch_message() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    // SAFETY: a constant, NUL-terminated name.
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("ShareCtrlScreen.Unwatch")) })
}

/// What the subclass procedure reaches through its reference data. Lives
/// until the subclass is gone; only the window's thread touches the cells.
struct WatchData {
    on_change: WindowActivityFn,
    last: Cell<Option<WindowActivity>>,
    /// What `WM_ACTIVATE` said last (activation can change without the
    /// foreground window changing, and the reverse).
    activated: Cell<bool>,
    /// The subclass is already gone (the window was destroyed first): no
    /// procedure call can reach the data any more.
    removed: AtomicBool,
}

impl WatchData {
    /// Report the window's state if it differs from what was reported last.
    /// `on_change` must not drop the watch (the trait's contract); `self` is
    /// not touched after the call anyway.
    fn report(&self, h: HWND, minimized: Option<bool>) {
        // SAFETY: a query on a live window of this thread.
        let minimized = minimized.unwrap_or_else(|| unsafe { IsIconic(h) }.as_bool());
        let now = WindowActivity {
            active: self.activated.get() && !minimized,
            minimized: Some(minimized),
        };
        if self.last.get() != Some(now) {
            self.last.set(Some(now));
            (self.on_change)(now);
        }
    }
}

/// A subclass on one top-level window.
pub(crate) struct ActivationWatch {
    hwnd: isize,
    /// The window's (UI) thread: the only one that may free `data` while the
    /// subclass is installed.
    thread: u32,
    data: *mut WatchData,
}

// SAFETY: the token only carries the pointer. While the subclass is
// installed the data is touched only on the window's thread (the procedure,
// and `Drop` there); a drop elsewhere hands the freeing to that thread
// through a posted message, or frees directly once the subclass is gone
// (`removed`, an atomic), when nothing can reach the data any more.
unsafe impl Send for ActivationWatch {}

impl WindowWatch for ActivationWatch {}

impl ActivationWatch {
    /// Watch the top-level window of `hwnd`. Call on the thread that owns it
    /// (the UI thread).
    pub(crate) fn install(hwnd: isize, on_change: WindowActivityFn) -> Result<Self, SurfaceError> {
        // Only a top-level window is activated: a child would never hear.
        // SAFETY: plain queries on a window handle; a stale one only fails.
        let h = unsafe { GetAncestor(HWND(hwnd as *mut _), GA_ROOT) };
        if h.is_invalid() {
            return Err(SurfaceError::Backend("the window is gone".into()));
        }
        // SAFETY: as above.
        let owner = unsafe { GetWindowThreadProcessId(h, None) };
        // SAFETY: no preconditions.
        if owner == 0 || owner != unsafe { GetCurrentThreadId() } {
            return Err(SurfaceError::Backend(
                "watch the window from the thread that owns it (the UI thread)".into(),
            ));
        }
        // SAFETY: a plain query.
        let foreground = unsafe { GetForegroundWindow() } == h;
        let data = Box::into_raw(Box::new(WatchData {
            on_change,
            last: Cell::new(None),
            activated: Cell::new(foreground),
            removed: AtomicBool::new(false),
        }));
        // Registered before the subclass can see a message.
        let _ = unwatch_message();
        // SAFETY: `h` belongs to this thread (checked above); `data` stays
        // valid until the subclass is removed.
        // The data's address is the subclass id: unique per watch, so two
        // watches on one window each keep their own.
        if !unsafe { SetWindowSubclass(h, Some(subclass_proc), data as usize, data as usize) }
            .as_bool()
        {
            // SAFETY: never handed to the window: still ours alone.
            drop(unsafe { Box::from_raw(data) });
            return Err(SurfaceError::Backend("could not watch the window".into()));
        }
        // SAFETY: `data` is valid (above); this is the window's thread.
        unsafe { (*data).report(h, None) };
        Ok(ActivationWatch {
            hwnd: h.0 as isize,
            thread: owner,
            data,
        })
    }
}

impl Drop for ActivationWatch {
    fn drop(&mut self) {
        let h = HWND(self.hwnd as *mut _);
        // SAFETY: `data` is alive until freed below or by the procedure, and
        // only one of the two ever frees it.
        let removed = unsafe { (*self.data).removed.load(Ordering::SeqCst) };
        if removed {
            // SAFETY: the subclass is gone: nothing else can reach it.
            drop(unsafe { Box::from_raw(self.data) });
            return;
        }
        // SAFETY: no preconditions.
        if unsafe { GetCurrentThreadId() } != self.thread {
            // Off the window's thread: that thread removes and frees it, in
            // order with its own messages. A window destroyed before the
            // message arrives frees nothing: a small leak, never a race.
            // SAFETY: posting to a window handle; a stale one only fails.
            let posted = unsafe {
                PostMessageW(
                    Some(h),
                    unwatch_message(),
                    WPARAM(0),
                    LPARAM(self.data as isize),
                )
            };
            if let Err(e) = posted {
                tracing::debug!("window watch: unwatch not posted ({e}); leaked");
            }
            return;
        }
        // SAFETY: removes only our own subclass (procedure + id) from a
        // window of this thread.
        if unsafe { IsWindow(Some(h)) }.as_bool() {
            let _ = unsafe { RemoveWindowSubclass(h, Some(subclass_proc), self.data as usize) };
        }
        // SAFETY: allocated by `install`, freed once, here; the subclass that
        // pointed at it is gone.
        drop(unsafe { Box::from_raw(self.data) });
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    refdata: usize,
) -> LRESULT {
    if msg == unwatch_message() && lparam.0 as usize == refdata {
        // A watch dropped off this thread: remove and free it here.
        // SAFETY: our own subclass on its window's thread; `refdata` was
        // allocated by `install` and is freed exactly once, here (its token
        // is gone and did not free it).
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), id);
            drop(Box::from_raw(refdata as *mut WatchData));
        }
        return LRESULT(0);
    }
    if msg == WM_NCDESTROY {
        // SAFETY: our own subclass, on its window's thread. Marking it gone
        // is the last touch of the data here: another thread may free it
        // right after (`Drop`).
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), id);
            (*(refdata as *const WatchData))
                .removed
                .store(true, Ordering::SeqCst);
            return DefSubclassProc(hwnd, msg, wparam, lparam);
        }
    }
    // SAFETY: `refdata` is the `WatchData` installed with this subclass; it
    // outlives the subclass.
    let data = unsafe { &*(refdata as *const WatchData) };
    match msg {
        WM_ACTIVATE => {
            data.activated
                .set((wparam.0 & 0xFFFF) as u32 != WA_INACTIVE);
            data.report(hwnd, None);
        }
        WM_SIZE => data.report(hwnd, Some(wparam.0 as u32 == SIZE_MINIMIZED)),
        _ => {}
    }
    // SAFETY: the documented way to pass every message on.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, ShowWindow, MSG, PM_REMOVE,
        SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
    };

    /// A hidden top-level window of this thread.
    fn window() -> HWND {
        // SAFETY: a predefined class; the window lives on this thread.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("watch test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                300,
                200,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a test window")
    }

    fn pump() {
        let mut msg = MSG::default();
        // SAFETY: this thread's own queue.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: a message just taken from the queue.
            unsafe { DispatchMessageW(&msg) };
        }
    }

    fn recorder() -> (WindowActivityFn, Arc<Mutex<Vec<WindowActivity>>>) {
        let seen: Arc<Mutex<Vec<WindowActivity>>> = Arc::default();
        let s = seen.clone();
        (Box::new(move |a| s.lock().unwrap().push(a)), seen)
    }

    #[test]
    fn minimise_and_restore_are_reported_once_each() {
        let h = window();
        let (f, seen) = recorder();
        let watch = ActivationWatch::install(h.0 as isize, f).expect("watched");
        assert_eq!(
            seen.lock().unwrap()[0].minimized,
            Some(false),
            "the state now"
        );
        // SAFETY: a window of this thread.
        unsafe {
            let _ = ShowWindow(h, SW_SHOWMINNOACTIVE);
        }
        pump();
        assert_eq!(seen.lock().unwrap().last().unwrap().minimized, Some(true));
        assert!(!seen.lock().unwrap().last().unwrap().active);
        // SAFETY: as above.
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
        pump();
        assert_eq!(seen.lock().unwrap().last().unwrap().minimized, Some(false));
        let reports = seen.lock().unwrap().len();
        // SAFETY: as above.
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
        pump();
        assert_eq!(seen.lock().unwrap().len(), reports, "no change: no report");
        drop(watch);
        // SAFETY: as above.
        unsafe {
            let _ = ShowWindow(h, SW_SHOWMINNOACTIVE);
        }
        pump();
        assert_eq!(seen.lock().unwrap().len(), reports, "stopped when dropped");
        // SAFETY: as above.
        let _ = unsafe { DestroyWindow(h) };
    }

    #[test]
    fn a_watch_outlives_its_window_and_a_drop_elsewhere_is_handed_back() {
        // Destroyed first: the drop afterwards only frees.
        let h = window();
        let (f, _) = recorder();
        let watch = ActivationWatch::install(h.0 as isize, f).expect("watched");
        // SAFETY: a window of this thread.
        let _ = unsafe { DestroyWindow(h) };
        drop(watch);

        // Dropped on another thread: the window's thread removes it.
        let h = window();
        let (f, seen) = recorder();
        let watch = ActivationWatch::install(h.0 as isize, f).expect("watched");
        std::thread::spawn(move || drop(watch)).join().unwrap();
        pump();
        let reports = seen.lock().unwrap().len();
        // SAFETY: as above.
        unsafe {
            let _ = ShowWindow(h, SW_SHOWMINNOACTIVE);
        }
        pump();
        assert_eq!(
            seen.lock().unwrap().len(),
            reports,
            "removed by its own thread"
        );
        // SAFETY: as above.
        let _ = unsafe { DestroyWindow(h) };
    }
}
