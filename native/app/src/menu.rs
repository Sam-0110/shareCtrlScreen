//! Native menus: the macOS/Linux app menu (task M15) — the "Session" submenu
//! and the "Monitor" submenu rebuilt from each `displays` event — and the click
//! dispatch shared with the Windows service host's tray menu.

use tauri::{Emitter, Manager};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::commands::display_label;
use crate::state::AppState;
#[cfg(windows)]
use crate::tray;

/// Build the native app menu and install it. macOS + Linux only (task M15):
/// Windows keeps its existing web drawer, so it gets no menu. A single "Session"
/// submenu with exactly three always-enabled items — Settings, Disconnect / End
/// Session, Check for updates. On macOS the standard app menu (About, Hide and
/// ⌘Q — the video view's key handling relies on ⌘Q reaching the menu bar) is
/// kept and ours is appended; Linux has no system app menu, so only ours shows
/// (in the window's menu bar).
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn install_native_menu(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, Submenu};
    let settings = MenuItem::with_id(app, "menu-settings", "Settings", true, None::<&str>)?;
    let disconnect = MenuItem::with_id(
        app,
        "menu-end-session",
        "Disconnect / End Session",
        true,
        None::<&str>,
    )?;
    let updates = MenuItem::with_id(
        app,
        "menu-check-updates",
        "Check for updates",
        true,
        None::<&str>,
    )?;
    let session = Submenu::with_items(app, "Session", true, &[&settings, &disconnect, &updates])?;
    // Tabs (plans/08 P4). No accelerators: the switch chords (D3) are taken
    // out of the video's key stream by the engine and handled by the page
    // when it has the keyboard; a menu key equivalent as well would switch
    // twice.
    let next = MenuItem::with_id(app, "menu-tab-next", "Next Tab", true, None::<&str>)?;
    let prev = MenuItem::with_id(app, "menu-tab-prev", "Previous Tab", true, None::<&str>)?;
    let new_tab = MenuItem::with_id(app, "menu-tab-new", "New Tab", true, None::<&str>)?;
    let close_tab = MenuItem::with_id(app, "menu-tab-close", "Close Tab", true, None::<&str>)?;
    let tabs = Submenu::with_items(app, "Tabs", true, &[&new_tab, &next, &prev, &close_tab])?;
    // The host's monitors during a session, rebuilt on every `displays` event
    // (see `update_monitor_menu`); a placeholder until then.
    let monitor = Submenu::with_id(app, "menu-monitor", "Monitor", true)?;
    #[cfg(target_os = "macos")]
    {
        let menu = Menu::default(app.handle())?;
        menu.append(&session)?;
        menu.append(&tabs)?;
        menu.append(&monitor)?;
        app.set_menu(menu)?;
    }
    #[cfg(target_os = "linux")]
    {
        let menu = Menu::with_items(app, &[&session, &tabs, &monitor])?;
        app.set_menu(menu)?;
    }
    app.manage(MonitorMenu(std::sync::Mutex::new(Some(monitor.clone()))));
    update_monitor_menu(app.handle(), &[], "", false, &[]);
    Ok(())
}

/// The native "Monitor" submenu (macOS + Linux), kept so it can be rebuilt.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct MonitorMenu(std::sync::Mutex<Option<tauri::menu::Submenu<tauri::Wry>>>);

/// The tab manager (managed from setup on).
fn tabs_of(app: &tauri::AppHandle) -> Option<std::sync::Arc<crate::tabs::TabManager>> {
    app.try_state::<std::sync::Arc<crate::tabs::TabManager>>()
        .map(|t| t.inner().clone())
}

/// Menu item id prefix for a monitor; the rest is the display id.
const MENU_DISPLAY_PREFIX: &str = "menu-display:";
/// Menu item id prefix for "open this monitor in a new window" (D14).
const MENU_POPOUT_PREFIX: &str = "menu-popout:";

/// Rebuild the native "Monitor" submenu: one checkable item per monitor, the
/// shared one checked, all disabled while this side may not switch. Fewer than
/// two monitors (or no session) shows one disabled placeholder. A no-op on
/// Windows, where the web drawer has the picker.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn update_monitor_menu(
    app: &tauri::AppHandle,
    list: &[protocol::RemoteDisplay],
    current: &str,
    can_switch: bool,
    popouts: &[protocol::PopoutStream],
) {
    use tauri::menu::{CheckMenuItem, MenuItem, PredefinedMenuItem, Submenu};
    let Some(state) = app.try_state::<MonitorMenu>() else {
        return;
    };
    let Some(sub) = state.0.lock().ok().and_then(|g| g.clone()) else {
        return;
    };
    let rebuild = || -> tauri::Result<()> {
        while sub.remove_at(0)?.is_some() {}
        if list.len() < 2 {
            let none = MenuItem::with_id(
                app,
                "menu-display-none",
                "No other monitors",
                false,
                None::<&str>,
            )?;
            sub.append(&none)?;
            return Ok(());
        }
        for d in list {
            let mut label = display_label(d);
            if !can_switch {
                label.push_str(" \u{2014} needs control");
            }
            let item = CheckMenuItem::with_id(
                app,
                format!("{MENU_DISPLAY_PREFIX}{}", d.id),
                label,
                can_switch,
                d.id == current,
                None::<&str>,
            )?;
            sub.append(&item)?;
        }
        // D14: "Open in New Window" for every monitor not already on screen
        // (only the viewer acts on it; on the host the engine ignores it).
        let others: Vec<&protocol::RemoteDisplay> = list
            .iter()
            .filter(|d| d.id != current && !popouts.iter().any(|p| p.id == d.id))
            .collect();
        if !others.is_empty() {
            sub.append(&PredefinedMenuItem::separator(app)?)?;
            let open = Submenu::with_id(app, "menu-popout", "Open in New Window", can_switch)?;
            for d in others {
                let item = MenuItem::with_id(
                    app,
                    format!("{MENU_POPOUT_PREFIX}{}", d.id),
                    display_label(d),
                    can_switch,
                    None::<&str>,
                )?;
                open.append(&item)?;
            }
            sub.append(&open)?;
        }
        Ok(())
    };
    if let Err(e) = rebuild() {
        tracing::warn!("monitor menu: {e}");
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn update_monitor_menu(
    _app: &tauri::AppHandle,
    _list: &[protocol::RemoteDisplay],
    _current: &str,
    _can_switch: bool,
    _popouts: &[protocol::PopoutStream],
) {
}

/// Dispatch a click on one of the native menu items (task M15). Settings and
/// "Check for updates" go to the webview as events — the same `openSettings`
/// flow the gear buttons use; "Check for updates" additionally runs the quiet
/// updater check in the Updates panel. Disconnect ends the session straight
/// through the engine, exactly as the `end_session` command does. On Windows
/// only the service host's tray menu fires ([`tray`]).
pub(crate) fn handle_menu_event(app: &tauri::AppHandle, event: tauri::menu::MenuEvent) {
    #[cfg(windows)]
    if tray::handle_menu_event(app, event.id().as_ref()) {
        return;
    }
    match event.id().as_ref() {
        "menu-settings" => {
            let _ = app.emit("menu://settings", ());
        }
        "menu-check-updates" => {
            let _ = app.emit("menu://check-updates", ());
        }
        "menu-end-session" => crate::commands::end_current(app),
        "menu-tab-next" => {
            if let Some(tabs) = tabs_of(app) {
                tabs.step(engine::TabStep::Next);
            }
        }
        "menu-tab-prev" => {
            if let Some(tabs) = tabs_of(app) {
                tabs.step(engine::TabStep::Prev);
            }
        }
        "menu-tab-new" => {
            if let Some(tabs) = tabs_of(app) {
                let _ = tabs.activate(None);
            }
        }
        "menu-tab-close" => {
            let Some(tabs) = tabs_of(app) else {
                return;
            };
            if let Some(remote) = app
                .state::<AppState>()
                .engine
                .workspace()
                .and_then(|w| w.active_tab)
            {
                tabs.close(&remote);
            }
        }
        other => {
            // The Monitor menu belongs to the tab on screen — or, when
            // hosting, to this computer's own shared monitor.
            let engine = &app.state::<AppState>().engine;
            let viewer = engine.active_session();
            if let Some(id) = other.strip_prefix(MENU_DISPLAY_PREFIX) {
                match viewer {
                    Some(session) => engine.select_display(session, id.to_string()),
                    None => engine.host_select_display(id.to_string()),
                }
            } else if let Some(id) = other.strip_prefix(MENU_POPOUT_PREFIX) {
                if let Some(session) = viewer {
                    engine.popout_display(session, id.to_string());
                }
            }
        }
    }
}
