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
//! * `IsIconic`, `GetForegroundWindow`, `GetWindowThreadProcessId`:
//!   <https://learn.microsoft.com/windows/win32/api/winuser/>

use std::cell::Cell;

use platform_api::{SurfaceError, WindowActivity, WindowActivityFn, WindowWatch};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, SIZE_MINIMIZED, WA_INACTIVE,
    WM_ACTIVATE, WM_NCDESTROY, WM_SIZE,
};

/// This subclass's id on the window (any value unique among its subclasses
/// with the same procedure).
const SUBCLASS_ID: usize = 0x5C7_0900;

/// What the subclass procedure reaches through its reference data. Lives
/// until the watch is dropped; only the window's thread touches it.
struct WatchData {
    on_change: WindowActivityFn,
    last: Cell<Option<WindowActivity>>,
    /// The subclass is already gone (the window was destroyed first).
    removed: Cell<bool>,
}

impl WatchData {
    /// Report `now` if it differs from what was reported last.
    fn report(&self, now: WindowActivity) {
        if self.last.get() != Some(now) {
            self.last.set(Some(now));
            (self.on_change)(now);
        }
    }
}

/// A subclass on one top-level window.
pub(crate) struct ActivationWatch {
    hwnd: isize,
    /// The window's (UI) thread: the only one that may free `data`.
    thread: u32,
    data: *mut WatchData,
}

// SAFETY: the token only carries the pointer; `data` is touched by the
// subclass procedure (UI thread) and freed by `Drop` only on that same
// thread — dropped anywhere else it is leaked instead, never freed while the
// procedure could run.
unsafe impl Send for ActivationWatch {}

impl WindowWatch for ActivationWatch {}

impl ActivationWatch {
    /// Watch top-level `hwnd`. Call on the thread that owns it (the UI
    /// thread).
    pub(crate) fn install(hwnd: isize, on_change: WindowActivityFn) -> Result<Self, SurfaceError> {
        let h = HWND(hwnd as *mut _);
        // SAFETY: plain queries on a window handle; a stale one only fails.
        let owner = unsafe { GetWindowThreadProcessId(h, None) };
        // SAFETY: no preconditions.
        if owner == 0 || owner != unsafe { GetCurrentThreadId() } {
            return Err(SurfaceError::Backend(
                "watch the window from the thread that owns it (the UI thread)".into(),
            ));
        }
        let data = Box::into_raw(Box::new(WatchData {
            on_change,
            last: Cell::new(None),
            removed: Cell::new(false),
        }));
        // SAFETY: `h` belongs to this thread (checked above); `data` stays
        // valid until `Drop`, which removes the subclass first.
        if !unsafe { SetWindowSubclass(h, Some(subclass_proc), SUBCLASS_ID, data as usize) }
            .as_bool()
        {
            // SAFETY: never handed to the window: still ours alone.
            drop(unsafe { Box::from_raw(data) });
            return Err(SurfaceError::Backend("could not watch the window".into()));
        }
        // SAFETY: `data` is valid (above); this is the window's thread.
        unsafe { (*data).report(current(h, None)) };
        Ok(ActivationWatch {
            hwnd,
            thread: owner,
            data,
        })
    }
}

impl Drop for ActivationWatch {
    /// On the UI thread (the trait's contract): the subclass procedure runs
    /// only there, so it cannot be inside the data while it is freed.
    fn drop(&mut self) {
        // SAFETY: no preconditions.
        if unsafe { GetCurrentThreadId() } != self.thread {
            // A contract breach: freeing here could race the procedure.
            tracing::error!("window watch dropped off the UI thread; leaked");
            return;
        }
        let h = HWND(self.hwnd as *mut _);
        // SAFETY: `data` lives until the end of this function.
        let removed = unsafe { (*self.data).removed.get() };
        // SAFETY: removes only our own subclass (procedure + id) from a
        // window of this thread; on a window already destroyed it was
        // removed at `WM_NCDESTROY` and is skipped.
        if !removed && unsafe { IsWindow(Some(h)) }.as_bool() {
            let _ = unsafe { RemoveWindowSubclass(h, Some(subclass_proc), SUBCLASS_ID) };
        }
        // SAFETY: allocated by `install`, freed once, here; the subclass that
        // pointed at it is gone.
        drop(unsafe { Box::from_raw(self.data) });
    }
}

/// The window's state now. `activated`: what a `WM_ACTIVATE` being handled
/// says (the foreground window may not have changed yet while it is sent).
fn current(h: HWND, activated: Option<bool>) -> WindowActivity {
    // SAFETY: queries on a live window of this thread.
    let (minimized, foreground) = unsafe { (IsIconic(h).as_bool(), GetForegroundWindow()) };
    WindowActivity {
        active: activated.unwrap_or(foreground == h) && !minimized,
        minimized: Some(minimized),
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
    // SAFETY: `refdata` is the `WatchData` installed with this subclass;
    // it outlives the subclass (see `Drop`).
    let data = unsafe { &*(refdata as *const WatchData) };
    match msg {
        WM_ACTIVATE => {
            let active = (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE;
            data.report(current(hwnd, Some(active)));
        }
        WM_SIZE => {
            let minimized = wparam.0 as u32 == SIZE_MINIMIZED;
            let mut now = current(hwnd, None);
            now.minimized = Some(minimized);
            now.active &= !minimized;
            data.report(now);
        }
        WM_NCDESTROY => {
            data.removed.set(true);
            // SAFETY: our own subclass, on its window's thread.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), id) };
        }
        _ => {}
    }
    // SAFETY: the documented way to pass every message on.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}
