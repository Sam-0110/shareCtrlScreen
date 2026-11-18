//! Windows, the service host only: the app lives in the notification area.
//!
//! The service host is the machine's always-on host (`platform_windows::instance`):
//! its job is to stay registered with signaling so the machine is reachable.
//! Closing its window used to exit the process, and with it the connection to
//! the server. Now closing the window ends any live session and hides the
//! window; the host keeps running and stays reachable. The tray icon brings the
//! window back (left click, or "Open") and is the deliberate way to stop the
//! host ("Quit").
//!
//! Quit exits with code 0. The service treats a clean exit as deliberate and
//! does not relaunch the host until the session changes; any other exit (a
//! crash, End task) is relaunched (`service/src/supervise.rs`).
//!
//! Only the service host hides to the tray. Another instance (portable, or no
//! service installed) quits on close as before: nothing would raise a hidden
//! one again, and a second launch beside it would start a second host.
//!
//! ## References
//!
//! * Tauri 2 system tray (`TrayIconBuilder`, tray menu events reach
//!   `Builder::on_menu_event`): <https://v2.tauri.app/learn/system-tray/>
//! * Tauri 2 `WindowEvent::CloseRequested` / `CloseRequestApi::prevent_close`:
//!   <https://docs.rs/tauri/2/tauri/enum.WindowEvent.html>

use std::sync::atomic::{AtomicBool, Ordering};

use engine::Role;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

use crate::AppState;

/// Tray menu: show the window.
pub const OPEN_ID: &str = "tray-open";
/// Tray menu: end any session and exit (code 0, so the service leaves it down).
pub const QUIT_ID: &str = "tray-quit";

/// Set once the tray icon exists. Closing hides the window only then: without
/// an icon a hidden window could not be brought back.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Put the icon in the notification area. Not fatal: without it the window
/// closes the app, as before.
pub fn install(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, OPEN_ID, "Open ShareCtrlScreen", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT_ID, "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &separator, &quit])?;
    let mut builder = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("ShareCtrlScreen: reachable while this icon is here")
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::raise_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Whether the tray icon is up (closing the window then hides it).
pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::SeqCst)
}

/// Closing the main window: end any live session, then hide instead of exit.
/// A session is ended rather than left running behind a hidden window, so
/// closing the window still stops someone controlling this computer.
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    let tauri::WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    if window.label() != "main" || !INSTALLED.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_close();
    let engine = &window.app_handle().state::<AppState>().engine;
    if !matches!(engine.role(), Role::Idle) {
        engine.end_all();
    }
    if let Err(e) = window.hide() {
        tracing::warn!("hiding the window to the tray failed: {e}");
    } else {
        tracing::info!("window closed: hidden to the tray, still reachable");
    }
}

/// An incoming connection request must be seen to be approved (it is denied
/// after 30 s). If the window is hidden in the tray, bring it up.
pub fn show_for_approval(app: &tauri::AppHandle) {
    let hidden = app
        .get_webview_window("main")
        .is_some_and(|w| !w.is_visible().unwrap_or(true));
    if hidden {
        crate::raise_main_window(app);
    }
}

/// Tray menu clicks. Returns whether the id was a tray item.
pub fn handle_menu_event(app: &tauri::AppHandle, id: &str) -> bool {
    match id {
        OPEN_ID => crate::raise_main_window(app),
        QUIT_ID => {
            tracing::info!("Quit from the tray: ending any session and exiting");
            // Exit only once every session is released (≤ 3 s): a host's
            // transport thread then has released remotely held keys and
            // sent its Bye (08a §6.17, I9).
            let engine = app.state::<AppState>().engine.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                engine
                    .end_all_and_wait(std::time::Duration::from_secs(3))
                    .await;
                app.exit(0);
            });
        }
        _ => return false,
    }
    true
}
