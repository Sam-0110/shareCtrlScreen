//! The WindowManager (plans/09 W3, 09a §1, §5.1–§5.8): detached tab windows —
//! made, placed, watched and destroyed — and the OS's word on which of the
//! app's windows is active or minimised.
//!
//! It owns windows, not tabs: which tab is in which window, and what follows
//! from a window being active or minimised, is the TabManager's (`tabs.rs`).
//! What it reports goes there through one channel ([`WindowReport`]).
//!
//! * **Activation comes from the OS** (`Platform::watch_window`), not from
//!   Tauri's `Focused`, which on Windows follows WebView2's focus and misses
//!   the native video (09 §7, blocker 1). The main window, every detached
//!   window and every pop-out window is watched; a pop-out counts as its
//!   session's.
//! * **Every window call that needs the UI thread runs there**, through
//!   `run_on_main_thread`, and nothing here holds a lock while it waits for
//!   it. Watches are dropped there too (their contract).
//! * **Placement** (09a §5.4, §5.5) is read and written as the OS reports it
//!   and converted by `protocol::sync::placement`: shares of a monitor's work
//!   area. It is saved only in the normal state, 500 ms after the last move
//!   or resize; a window on Wayland never saves its position (09a J9).
//!
//! ## References
//! * Tauri `WebviewWindowBuilder`, `Window::{outer_position, outer_size,
//!   inner_size, set_position, set_size, is_minimized, is_maximized,
//!   available_monitors, primary_monitor, current_monitor}`, `Monitor::
//!   {position, size, work_area, scale_factor}`:
//!   <https://docs.rs/tauri/2/tauri/webview/struct.WebviewWindowBuilder.html>,
//!   <https://docs.rs/tauri/2/tauri/window/struct.Monitor.html>
//! * Set the position before the size, so a window moving to a monitor with
//!   another DPI is sized in that monitor's scale (09 §7):
//!   <https://learn.microsoft.com/windows/win32/hidpi/wm-dpichanged>

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine::{Engine, SessionId, WindowId};
use platform_api::{WindowActivity, WindowWatch};
use protocol::sync::placement::{self, Monitor, Rect};
use protocol::sync::{Pos, Size, WinState};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};

/// Detached windows' label prefix; then the window's number.
pub(crate) const LABEL_PREFIX: &str = "detached-";
/// How long after the last move or resize the placement is saved.
const SAVE_AFTER: Duration = Duration::from_millis(500);
/// A new window with no saved size: this share of the work area.
const DEFAULT_SIZE: Size = Size { w: 6000, h: 6500 };
/// Smallest window restored, in the OS's units (09a §5.5).
const MIN_SIZE: (u32, u32) = (480, 320);
/// The smallest a detached window can be made, in logical pixels.
const MIN_INNER: (f64, f64) = (400.0, 280.0);

/// Window `id`'s label.
pub(crate) fn label_of(id: WindowId) -> String {
    if id == WindowId::MAIN {
        "main".to_string()
    } else {
        format!("{LABEL_PREFIX}{}", id.0)
    }
}

/// The window behind `label`, if it is the main or a detached one.
pub(crate) fn window_of(label: &str) -> Option<WindowId> {
    if label == "main" {
        return Some(WindowId::MAIN);
    }
    label
        .strip_prefix(LABEL_PREFIX)?
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .map(WindowId)
}

/// Whose window the OS reported on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Who {
    Window(WindowId),
    /// A pop-out window counts as its session's (09 §7, blocker 5).
    Popout(SessionId),
}

/// What the WindowManager tells the TabManager.
#[derive(Debug)]
pub(crate) enum WindowReport {
    Activity(Who, WindowActivity),
    /// The window went (its watch first, so its last word may still say
    /// "active"): forget what it said.
    Gone(Who),
    /// A detached window settled after a move, resize or state change: its
    /// placement to save.
    Placement {
        window: WindowId,
        state: WinState,
        /// Size and position, in the normal state only.
        normal: Option<(Size, Option<Pos>)>,
    },
}

/// Where a new detached window goes: the tab's saved placement.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Place {
    pub(crate) size: Option<Size>,
    pub(crate) pos: Option<Pos>,
    pub(crate) state: WinState,
}

pub(crate) struct WindowManager {
    app: AppHandle,
    engine: Arc<Engine>,
    reports: tokio::sync::mpsc::UnboundedSender<WindowReport>,
    /// Live watches by window label (dropped on the UI thread).
    watches: Mutex<HashMap<String, Box<dyn WindowWatch>>>,
    /// Per window, the latest move or resize and whether a save is waiting
    /// for it to settle: one waiting task per window, however long the drag.
    moves: Mutex<HashMap<String, (u64, bool)>>,
    serial: AtomicU64,
    /// Windows being placed by `open`: the moves and resizes that placing
    /// causes are the saved placement played back (or a minimise at login),
    /// not the user's, so they are not saved again.
    placing: Mutex<std::collections::HashSet<String>>,
}

impl WindowManager {
    pub(crate) fn new(
        app: AppHandle,
        engine: Arc<Engine>,
        reports: tokio::sync::mpsc::UnboundedSender<WindowReport>,
    ) -> Arc<Self> {
        Arc::new(WindowManager {
            app,
            engine,
            reports,
            watches: Mutex::default(),
            moves: Mutex::default(),
            serial: AtomicU64::new(0),
            placing: Mutex::default(),
        })
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        // Plain maps: consistent across a panic elsewhere.
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Watch the main window. Call on the UI thread (setup).
    pub(crate) fn watch_main(&self) {
        if let Some(win) = self.app.get_webview_window("main") {
            self.watch_on_ui_thread(&win, Who::Window(WindowId::MAIN));
        }
    }

    /// Watch pop-out window `win` of `session`. Call on the UI thread.
    pub(crate) fn watch_popout(&self, win: &tauri::WebviewWindow, session: SessionId) {
        self.watch_on_ui_thread(win, Who::Popout(session));
    }

    /// Stop watching window `label` of `who` (a pop-out closing). Any
    /// thread.
    pub(crate) fn unwatch(&self, label: &str, who: Who) {
        let watch = Self::lock(&self.watches).remove(label);
        if let Some(watch) = watch {
            self.drop_on_ui_thread(watch);
        }
        let _ = self.reports.send(WindowReport::Gone(who));
    }

    fn watch_on_ui_thread(&self, win: &tauri::WebviewWindow, who: Who) {
        let handles = win
            .window_handle()
            .map(|h| h.as_raw())
            .and_then(|w| win.display_handle().map(|d| (w, d.as_raw())));
        let (window, display) = match handles {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("window {}: no handle to watch: {e}", win.label());
                return;
            }
        };
        let reports = self.reports.clone();
        let watched = self.engine.platform().watch_window(
            window,
            display,
            // On the UI thread, inside the OS's dispatch: only a send.
            Box::new(move |a| {
                let _ = reports.send(WindowReport::Activity(who, a));
            }),
        );
        match watched {
            Ok(watch) => {
                let old = Self::lock(&self.watches).insert(win.label().to_string(), watch);
                // A label is never reused while its window lives; an old
                // watch here is a dead window's, dropped on this (UI) thread.
                drop(old);
            }
            Err(e) => tracing::warn!("window {}: activation not watched: {e}", win.label()),
        }
    }

    fn drop_on_ui_thread(&self, watch: Box<dyn WindowWatch>) {
        if let Err(e) = self.app.run_on_main_thread(move || drop(watch)) {
            // Only at exit, when the UI thread is gone: so is every window.
            tracing::debug!("window watch not dropped (UI thread gone): {e}");
        }
    }

    /// Make detached window `id` for `remote`'s tab, named `title`, at
    /// `place`: shown and watched. Call from a task, never on the UI thread
    /// (it waits for it).
    pub(crate) async fn open(
        self: &Arc<Self>,
        id: WindowId,
        remote: &str,
        title: &str,
        place: Place,
    ) -> Result<(), String> {
        let label = label_of(id);
        Self::lock(&self.placing).insert(label.clone());
        let url = WebviewUrl::App(format!("index.html?window={remote}").into());
        let win = WebviewWindowBuilder::new(&self.app, &label, url)
            .title(format!("{title} \u{2014} ShareCtrlScreen"))
            .inner_size(1280.0, 800.0)
            .min_inner_size(MIN_INNER.0, MIN_INNER.1)
            .visible(false)
            .build()
            .map_err(|e| {
                Self::lock(&self.placing).remove(&label);
                format!("could not open a window: {e}")
            })?;
        // Placed while hidden, so it never shows in the wrong place first.
        // A step that fails leaves the window usable where the OS put it.
        let step = |what: &str, r: tauri::Result<()>| {
            if let Err(e) = r {
                tracing::warn!("window {label}: {what}: {e}");
            }
        };
        match self.rect_for(&win, place) {
            Some((pos, inner)) => {
                // Position first: the size is then in that monitor's scale.
                step("position", win.set_position(pos));
                step("size", win.set_size(inner));
            }
            None => step("center", win.center()),
        }
        step("show", win.show());
        match place.state {
            WinState::Maximized => step("maximize", win.maximize()),
            WinState::Minimized => step("minimize", win.minimize()),
            WinState::Normal => step("focus", win.set_focus()),
        }
        // The OS delivers the placing's own move / resize events shortly
        // after; from then on they are the user's.
        let me = self.clone();
        let placed = label.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(SAVE_AFTER * 2).await;
            Self::lock(&me.placing).remove(&placed);
        });
        let (tx, rx) = tokio::sync::oneshot::channel();
        let me = self.clone();
        let w = win.clone();
        self.app
            .run_on_main_thread(move || {
                me.watch_on_ui_thread(&w, Who::Window(id));
                let _ = tx.send(());
            })
            .map_err(|e| format!("the UI thread is not running: {e}"))?;
        rx.await
            .map_err(|_| "the UI thread dropped the request".to_string())
    }

    /// The outer position and inner size to give `win` for `place`, in
    /// physical pixels; `None` when the OS reports no monitors.
    fn rect_for(
        &self,
        win: &tauri::WebviewWindow,
        place: Place,
    ) -> Option<(PhysicalPosition<i32>, PhysicalSize<u32>)> {
        let (monitors, scales) = self.monitors(win)?;
        // A tab never placed opens on the main window's monitor.
        let unplaced_on = self
            .app
            .get_webview_window("main")
            .and_then(|m| m.current_monitor().ok().flatten())
            .and_then(|m| {
                let p = *m.position();
                monitors_raw(&self.app)?
                    .iter()
                    .position(|x| *x.position() == p)
            });
        let rect = placement::to_rect(
            place.size,
            place.pos,
            &monitors,
            unplaced_on,
            DEFAULT_SIZE,
            MIN_SIZE,
        )?;
        let rect = to_physical(rect, scales.window);
        // The OS sets the inner size; the saved rectangle is the outer one.
        let deco = decorations(win);
        Some((
            PhysicalPosition::new(rect.x, rect.y),
            PhysicalSize::new(
                rect.w.saturating_sub(deco.0).max(1),
                rect.h.saturating_sub(deco.1).max(1),
            ),
        ))
    }

    /// The monitors in placement units (see `to_units`), and the scale the
    /// window's rectangle converts with.
    fn monitors(&self, win: &tauri::WebviewWindow) -> Option<(Vec<Monitor>, Scales)> {
        let raw = monitors_raw(&self.app)?;
        if raw.is_empty() {
            return None;
        }
        let primary = self.app.primary_monitor().ok().flatten();
        let window_scale = win.scale_factor().unwrap_or(1.0);
        let monitors = raw
            .iter()
            .map(|m| {
                let scale = m.scale_factor();
                let bounds = Rect {
                    x: m.position().x,
                    y: m.position().y,
                    w: m.size().width,
                    h: m.size().height,
                };
                let wa = m.work_area();
                let work = Rect {
                    x: wa.position.x,
                    y: wa.position.y,
                    w: wa.size.width,
                    h: wa.size.height,
                };
                Monitor {
                    bounds: to_units(bounds, scale),
                    work: to_units(work, scale),
                    primary: primary
                        .as_ref()
                        .is_some_and(|p| p.position() == m.position() && p.size() == m.size()),
                }
            })
            .collect();
        Some((
            monitors,
            Scales {
                window: window_scale,
            },
        ))
    }

    /// Detached window `label` moved, was resized or changed state: save its
    /// placement once it settles. Any thread.
    pub(crate) fn moved(self: &Arc<Self>, label: &str) {
        let Some(window) = window_of(label).filter(|w| *w != WindowId::MAIN) else {
            return;
        };
        if Self::lock(&self.placing).contains(label) {
            return;
        }
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        let waiting = {
            let mut moves = Self::lock(&self.moves);
            let entry = moves.entry(label.to_string()).or_insert((serial, false));
            entry.0 = serial;
            std::mem::replace(&mut entry.1, true)
        };
        if waiting {
            return; // the task already waiting sees this move
        }
        let me = self.clone();
        let label = label.to_string();
        tauri::async_runtime::spawn(async move {
            // Until nothing moved for SAVE_AFTER (or the window went).
            let mut seen = serial;
            loop {
                tokio::time::sleep(SAVE_AFTER).await;
                let mut moves = Self::lock(&me.moves);
                match moves.get_mut(&label) {
                    None => return,
                    Some(entry) if entry.0 != seen => seen = entry.0,
                    Some(entry) => {
                        entry.1 = false;
                        break;
                    }
                }
            }
            if let Some(report) = me.read_placement(window, &label) {
                let _ = me.reports.send(report);
            }
        });
    }

    /// What window `label` should save now (09a §5.4).
    fn read_placement(&self, window: WindowId, label: &str) -> Option<WindowReport> {
        let win = self.app.get_webview_window(label)?;
        let minimized = win.is_minimized().unwrap_or(false);
        let maximized = win.is_maximized().unwrap_or(false);
        let state = if minimized {
            WinState::Minimized
        } else if maximized {
            WinState::Maximized
        } else {
            WinState::Normal
        };
        // A minimised window reports a parked rectangle (-32000 on Windows)
        // and a maximised one its maximised one: only the normal state's is
        // the window's own (09 §7).
        let normal = if state == WinState::Normal {
            let (monitors, scales) = self.monitors(&win)?;
            let pos = win.outer_position().ok()?;
            let size = win.outer_size().ok()?;
            let rect = to_units(
                Rect {
                    x: pos.x,
                    y: pos.y,
                    w: size.width,
                    h: size.height,
                },
                scales.window,
            );
            let p = placement::to_placement(rect, &monitors)?;
            Some((p.size, knows_positions(&win).then_some(p.pos)))
        } else {
            None
        };
        Some(WindowReport::Placement {
            window,
            state,
            normal,
        })
    }

    /// Close detached window `id`: its watch goes (on the UI thread), then
    /// the window. Call only once no session draws in it any more.
    pub(crate) fn close(&self, id: WindowId) {
        let label = label_of(id);
        Self::lock(&self.moves).remove(&label);
        let watch = Self::lock(&self.watches).remove(&label);
        let _ = self.reports.send(WindowReport::Gone(Who::Window(id)));
        let app = self.app.clone();
        let destroy = move || {
            drop(watch);
            if let Some(win) = app.get_webview_window(&label) {
                if let Err(e) = win.destroy() {
                    tracing::warn!("window {label}: {e}");
                }
            }
        };
        if let Err(e) = self.app.run_on_main_thread(destroy) {
            tracing::debug!("window not closed (UI thread gone): {e}");
        }
    }

    /// Hide or show every detached window (the tray, 09a §5.7).
    #[cfg(windows)]
    pub(crate) fn set_all_visible(&self, visible: bool) {
        for (label, win) in self.app.webview_windows() {
            if window_of(&label).is_some_and(|w| w != WindowId::MAIN) {
                let _ = if visible { win.show() } else { win.hide() };
            }
        }
    }

    /// Unminimise, show and raise the main window (Dock, 09 §7).
    pub(crate) fn raise_main(&self) {
        if let Some(win) = self.app.get_webview_window("main") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
        }
    }

    /// Give window `id` the foreground (a tab focused from elsewhere).
    pub(crate) fn raise(&self, id: WindowId) {
        if let Some(win) = self.app.get_webview_window(&label_of(id)) {
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    }
}

/// How a window's own rectangle converts into placement units.
#[derive(Clone, Copy)]
struct Scales {
    window: f64,
}

fn monitors_raw(app: &AppHandle) -> Option<Vec<tauri::Monitor>> {
    app.available_monitors().ok()
}

/// A rectangle as Tauri reports it (physical pixels) in placement units:
/// unchanged on Windows and Linux, where monitors and windows share one
/// physical space; logical points on macOS, where tao scales monitor and
/// window positions by different factors (09 §7).
fn to_units(r: Rect, scale: f64) -> Rect {
    if cfg!(target_os = "macos") && scale > 0.0 {
        let s = |v: f64| (v / scale).round();
        Rect {
            x: s(f64::from(r.x)) as i32,
            y: s(f64::from(r.y)) as i32,
            w: s(f64::from(r.w)) as u32,
            h: s(f64::from(r.h)) as u32,
        }
    } else {
        r
    }
}

/// The reverse of [`to_units`].
fn to_physical(r: Rect, scale: f64) -> Rect {
    if cfg!(target_os = "macos") && scale > 0.0 {
        let s = |v: f64| (v * scale).round();
        Rect {
            x: s(f64::from(r.x)) as i32,
            y: s(f64::from(r.y)) as i32,
            w: s(f64::from(r.w)) as u32,
            h: s(f64::from(r.h)) as u32,
        }
    } else {
        r
    }
}

/// Outer minus inner size: the frame the OS draws around the page.
fn decorations(win: &tauri::WebviewWindow) -> (u32, u32) {
    match (win.outer_size(), win.inner_size()) {
        (Ok(o), Ok(i)) => (
            o.width.saturating_sub(i.width),
            o.height.saturating_sub(i.height),
        ),
        _ => (0, 0),
    }
}

/// Whether this computer can know where its windows are: not on Wayland,
/// where a client never learns its position (09a J9).
fn knows_positions(win: &tauri::WebviewWindow) -> bool {
    !matches!(
        win.window_handle().map(|h| h.as_raw()),
        Ok(RawWindowHandle::Wayland(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_windows_round_trip() {
        assert_eq!(label_of(WindowId::MAIN), "main");
        assert_eq!(label_of(WindowId(7)), "detached-7");
        assert_eq!(window_of("detached-7"), Some(WindowId(7)));
        assert_eq!(window_of("main"), Some(WindowId::MAIN));
        assert_eq!(window_of("detached-0"), None, "0 is the main window");
        assert_eq!(window_of("popout-1-2-3"), None);
        assert_eq!(window_of("detached-x"), None);
    }
}
