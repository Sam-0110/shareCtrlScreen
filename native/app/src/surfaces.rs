//! The native video surfaces (plans/08a §2.3, §6.2, §6.5): one per connected
//! tab, in the main window or its detached window (plans/09), plus the
//! pop-out windows' (popout.rs). Surfaces are
//! created and destroyed only on the UI thread — the OS backends require it,
//! and a surface torn down by whichever thread dropped the last reference
//! would leave Windows child windows behind (B6).
//!
//! A tab's surface sits below the tab strip ([`TAB_BAR_CSS_PX`], a real layout
//! inset: its client size and input mapping cover only the area below), so
//! the strip stays visible and clickable over a live session.
//!
//! ## References
//! * Tauri `AppHandle::run_on_main_thread`:
//!   <https://docs.rs/tauri/2/tauri/struct.AppHandle.html#method.run_on_main_thread>
//! * raw-window-handle `HasWindowHandle` / `HasDisplayHandle`:
//!   <https://docs.rs/raw-window-handle/0.6/raw_window_handle/>

use std::sync::Arc;

use engine::{Engine, WindowId};
use platform_api::VideoSurface;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use tauri::{AppHandle, Manager};

/// Height of the web tab strip in CSS px (`ui/style.css` `--tab-bar-h`).
/// Tab surfaces leave it uncovered.
pub(crate) const TAB_BAR_CSS_PX: u32 = 36;

/// Create a native video surface in `win`. Call on the UI thread.
pub(crate) fn create_in(
    engine: &Engine,
    win: &tauri::WebviewWindow,
) -> Result<Arc<dyn VideoSurface>, String> {
    let window = win
        .window_handle()
        .map_err(|e| format!("no window handle: {e}"))?
        .as_raw();
    let display = win
        .display_handle()
        .map_err(|e| format!("no display handle: {e}"))?
        .as_raw();
    engine
        .platform()
        .create_surface(window, display)
        .map_err(|e| format!("no native video surface: {e}"))
}

/// Makes and destroys the tabs' surfaces, always on the UI thread.
pub(crate) struct SurfaceFactory {
    app: AppHandle,
    engine: Arc<Engine>,
}

impl SurfaceFactory {
    pub(crate) fn new(app: AppHandle, engine: Arc<Engine>) -> Self {
        SurfaceFactory { app, engine }
    }

    /// A hidden surface in `window`: below the tab strip in the main window,
    /// the whole page in a detached one (plans/09: it has no strip). Waits
    /// for the UI thread, so call it from a task, never from the UI thread.
    pub(crate) async fn create(&self, window: WindowId) -> Result<Arc<dyn VideoSurface>, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (app, engine) = (self.app.clone(), self.engine.clone());
        let label = crate::windows::label_of(window);
        self.app
            .run_on_main_thread(move || {
                let made = match app.get_webview_window(&label) {
                    Some(win) => create_in(&engine, &win),
                    None => Err("that window is gone".to_string()),
                };
                // The caller may have given up (app closing); nothing to do.
                let _ = tx.send(made);
            })
            .map_err(|e| format!("the UI thread is not running: {e}"))?;
        let surface = rx
            .await
            .map_err(|_| "the UI thread dropped the request".to_string())??;
        if window == WindowId::MAIN {
            surface.set_top_inset(TAB_BAR_CSS_PX);
        }
        Ok(surface)
    }

    /// Tear `surface` down on the UI thread. Call only once the session that
    /// used it was released (its threads are joined; nothing draws into it).
    pub(crate) fn destroy(&self, surface: Arc<dyn VideoSurface>) {
        if let Err(e) = self.app.run_on_main_thread(move || surface.destroy()) {
            // Only at exit: the window, and every surface in it, goes anyway.
            tracing::debug!("video surface not destroyed (UI thread gone): {e}");
        }
    }
}
