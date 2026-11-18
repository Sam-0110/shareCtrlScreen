//! Every `#[tauri::command]` the web UI invokes (one per old Electron
//! `ipcMain.handle`), plus the JSON shapes they return.

use codec_openh264::Availability;
use engine::handshake::ApprovalDecision;
use engine::Role;
use platform_api::{OsPermission, PermissionReport, PermissionState, Platform};
use protocol::Permission;

use std::sync::Arc;

use tauri::Manager;

use crate::state::AppState;
use crate::tabs::TabManager;

/// UI-shaped config (camelCase, as the ported renderer expects). Deliberately
/// omits `passwordHash` — the webview never needs it — and exposes a
/// `hasPassword` boolean instead, which is all the settings modal uses.
///
/// The person's data (recents, names, synced preferences, saved-password
/// count) comes from the active workspace; while the selected account is
/// signed out none is reachable (I13) and those read as empty and default.
#[tauri::command]
pub fn get_config(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let c = state.engine.config();
    let ws = state.engine.workspace().unwrap_or_default();
    // A signed-out account's preferences still apply (they are not private).
    let p = state.engine.prefs();
    serde_json::json!({
        "uuid": c.uuid,
        "serverUrl": c.server_url,
        "mode": match c.mode { protocol::Mode::Password => "password", _ => "approve" },
        "hasPassword": c.password_hash.is_some(),
        "passwordPermission": c.password_permission.as_str(),
        "shareAudio": c.share_audio,
        "shareDisplayId": c.share_display_id,
        "recentIds": ws.recent_ids,
        "remoteNames": ws.remote_names,
        // Accounts on this computer and the selected one's state (no tokens).
        "account": state.accounts.view(),
        "captureShortcuts": p.capture_shortcuts,
        "clipboardSync": p.clipboard_sync,
        "pasteDroppedFiles": p.paste_dropped_files,
        "startWithWindows": c.start_with_windows,
        "useOpenH264": c.use_openh264,
        "translateModifiers": p.translate_modifiers,
        // Count only — the saved passwords themselves never reach the WebView.
        "savedLoginCount": ws.saved_logins.len(),
    })
}

/// What the UI needs to word itself for this OS and to list the monitors.
/// Built from the platform alone so it can be tested against the mock.
fn platform_info_json(platform: &dyn Platform) -> serde_json::Value {
    let info = platform.info();
    let displays: Vec<_> = platform
        .displays()
        .into_iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "name": d.name,
                "x": d.x,
                "y": d.y,
                "width": d.width,
                "height": d.height,
                "scale": d.scale,
                "primary": d.primary,
            })
        })
        .collect();
    serde_json::json!({
        "name": info.name,
        "startAtLoginLabel": info.start_at_login_label,
        "shortcutCaptureHint": info.shortcut_capture_hint,
        "loginScreenSupported": info.login_screen_supported,
        "downloadsDir": platform.paths().downloads_dir.display().to_string(),
        "displays": displays,
    })
}

#[tauri::command]
pub fn get_platform_info(state: tauri::State<'_, AppState>) -> serde_json::Value {
    platform_info_json(state.engine.platform().as_ref())
}

#[tauri::command]
pub fn clear_recents(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.engine.clear_recents()
}

/// Name a remote ID (an empty name removes it). Returns all names.
#[tauri::command]
pub fn set_remote_name(
    state: tauri::State<'_, AppState>,
    id: String,
    name: String,
) -> std::collections::BTreeMap<String, String> {
    state.engine.set_remote_name(&id, &name)
}

/// Current signaling status, queried by the UI on boot. The one-shot
/// `server-status` event can fire before the WebView subscribes, so the boot
/// path pulls the live value instead of relying solely on that event.
#[tauri::command]
pub fn get_server_status(state: tauri::State<'_, AppState>) -> String {
    if state.engine.server_connected() {
        "connected".into()
    } else {
        "disconnected".into()
    }
}

// ---- tabs (plans/08 P4, 08a §4.1) ----------------------------------------------
// Each command changes the TabManager; the UI redraws from the `tabs-changed`
// snapshot that follows, never from a command's answer (only `needsConfirm`,
// D2, is an answer the UI acts on).

type Tabs<'a> = tauri::State<'a, Arc<TabManager>>;

/// Open (or show) the tab for `remote_id` and connect it.
#[tauri::command]
pub async fn tab_open(
    tabs: Tabs<'_>,
    remote_id: String,
    confirmed: bool,
) -> Result<serde_json::Value, String> {
    let tabs = tabs.inner().clone();
    tabs.open(&remote_id, confirmed).await.map(|s| s.to_json())
}

#[tauri::command]
pub async fn tab_connect(
    tabs: Tabs<'_>,
    remote_id: String,
    confirmed: bool,
) -> Result<serde_json::Value, String> {
    let tabs = tabs.inner().clone();
    tabs.connect(&remote_id, confirmed)
        .await
        .map(|s| s.to_json())
}

#[tauri::command]
pub fn tab_disconnect(tabs: Tabs<'_>, remote_id: String) {
    tabs.disconnect(&remote_id);
}

#[tauri::command]
pub fn tab_close(tabs: Tabs<'_>, remote_id: String) {
    tabs.close(&remote_id);
}

/// Show a tab; `null`: the connect page (the "+").
#[tauri::command]
pub fn tab_activate(tabs: Tabs<'_>, remote_id: Option<String>) -> Result<(), String> {
    tabs.activate(remote_id.as_deref())
}

#[tauri::command]
pub fn tab_next(tabs: Tabs<'_>) {
    tabs.step(engine::TabStep::Next);
}

#[tauri::command]
pub fn tab_prev(tabs: Tabs<'_>) {
    tabs.step(engine::TabStep::Prev);
}

/// Rename the remote (its tab, chips and connect box together); empty clears.
#[tauri::command]
pub fn tab_rename(tabs: Tabs<'_>, remote_id: String, name: String) {
    tabs.rename(&remote_id, &name);
}

/// Move a tab before another (`null`: to the end).
#[tauri::command]
pub fn tab_move(tabs: Tabs<'_>, remote_id: String, before: Option<String>) -> Result<(), String> {
    tabs.move_tab(&remote_id, before.as_deref())
}

#[tauri::command]
pub fn tab_connect_all(tabs: Tabs<'_>, confirmed: bool) -> serde_json::Value {
    tabs.inner().connect_all(confirmed).to_json()
}

/// Answer a tab's password prompt.
#[tauri::command]
pub fn tab_password(tabs: Tabs<'_>, remote_id: String, password: String, remember: bool) {
    tabs.password(&remote_id, password, remember);
}

#[tauri::command]
pub fn tab_forget_password(tabs: Tabs<'_>, remote_id: String) {
    tabs.forget_password(&remote_id);
}

/// The current strip (UI boot or reload).
#[tauri::command]
pub fn get_snapshot(tabs: Tabs<'_>) -> serde_json::Value {
    tabs.snapshot()
}

/// "Disconnect" in the session bar or menu: the tab on screen's session, or —
/// while this computer is being viewed — that session.
pub(crate) fn end_current(app: &tauri::AppHandle) {
    let engine = &app.state::<AppState>().engine;
    if matches!(engine.role(), Role::Host { .. }) {
        engine.end_hosting();
    } else if let Some(id) = engine.focused_session() {
        engine.disconnect(id);
    }
}

#[tauri::command]
pub fn set_permission(state: tauri::State<'_, AppState>, value: String) {
    let perm = if value == "control" {
        Permission::Control
    } else {
        Permission::View
    };
    state.engine.set_permission(perm);
}

/// The session bar's Disconnect: `remote_id`'s tab (the bar names its
/// tab: there may be several on screen, plans/09); none — the host screen,
/// or an older page — ends what `end_current` ends.
#[tauri::command]
pub fn end_session(app: tauri::AppHandle, tabs: Tabs<'_>, remote_id: Option<String>) {
    match remote_id {
        Some(remote) => tabs.disconnect(&remote),
        None => end_current(&app),
    }
}

/// The viewer session a session-bar command is for: `remote_id`'s tab, or
/// the focused one when none is named.
fn bar_session(
    state: &AppState,
    tabs: &TabManager,
    remote_id: Option<&str>,
) -> Option<engine::SessionId> {
    match remote_id {
        Some(remote) => tabs.session_of(remote),
        None => state.engine.focused_session(),
    }
}

/// Put `remote_id`'s tab in a window of its own (plans/09 W3).
#[tauri::command]
pub async fn tab_detach(tabs: Tabs<'_>, remote_id: String) -> Result<(), String> {
    let tabs = tabs.inner().clone();
    tabs.detach(&remote_id).await
}

/// Put `remote_id`'s detached tab back in the strip (plans/09 W3).
#[tauri::command]
pub async fn tab_dock(tabs: Tabs<'_>, remote_id: String) -> Result<(), String> {
    let tabs = tabs.inner().clone();
    tabs.dock(&remote_id).await
}

/// Viewer: ask the host for a fresh keyframe (clears smear after packet loss).
#[tauri::command]
pub fn request_refresh(
    state: tauri::State<'_, AppState>,
    tabs: Tabs<'_>,
    remote_id: Option<String>,
) {
    if let Some(id) = bar_session(&state, &tabs, remote_id.as_deref()) {
        state.engine.request_refresh(id);
    }
}

/// Switch the shared monitor mid-session (host: directly; viewer: asks the
/// host, which needs control). The result arrives as a `displays` event.
#[tauri::command]
pub fn select_display(
    state: tauri::State<'_, AppState>,
    tabs: Tabs<'_>,
    remote_id: Option<String>,
    id: String,
) {
    if matches!(state.engine.role(), Role::Host { .. }) && remote_id.is_none() {
        state.engine.host_select_display(id);
        return;
    }
    match bar_session(&state, &tabs, remote_id.as_deref()) {
        Some(viewer) => state.engine.select_display(viewer, id),
        None => state.engine.host_select_display(id),
    }
}

/// The `displays` event payload: the host's monitors, the one shared now, and
/// whether this side may switch.
pub(crate) fn displays_json(
    list: &[protocol::RemoteDisplay],
    current: &str,
    can_switch: bool,
    popouts: &[protocol::PopoutStream],
) -> serde_json::Value {
    let popouts: Vec<_> = popouts
        .iter()
        .map(|p| serde_json::json!({ "slot": p.s, "id": p.id }))
        .collect();
    let displays: Vec<_> = list
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "name": d.name,
                "width": d.width,
                "height": d.height,
                "primary": d.primary,
            })
        })
        .collect();
    serde_json::json!({
        "displays": displays,
        "current": current,
        "canSwitch": can_switch,
        "popouts": popouts,
    })
}

/// Viewer: ask the host to also show monitor `id` in a separate window (D14;
/// needs control). The window opens when the host's `displays` lists it.
#[tauri::command]
pub fn popout_display(
    state: tauri::State<'_, AppState>,
    tabs: Tabs<'_>,
    remote_id: Option<String>,
    id: String,
) {
    if let Some(viewer) = bar_session(&state, &tabs, remote_id.as_deref()) {
        state.engine.popout_display(viewer, id);
    }
}

/// How a monitor is labelled in the pickers and pop-out titles (the web UI
/// builds the same label in `ui/session.js`).
pub(crate) fn display_label(d: &protocol::RemoteDisplay) -> String {
    let name = if d.name.is_empty() {
        d.id.as_str()
    } else {
        d.name.as_str()
    };
    let primary = if d.primary { ", primary" } else { "" };
    format!("{name} ({}\u{d7}{}{primary})", d.width, d.height)
}

/// A web dialog (Settings, a confirm) opened over the video (`visible` false)
/// or the last one closed: the native surface would otherwise cover it, so
/// the engine's gate hides every main video meanwhile (08a §6.18).
/// Async so it runs off the UI thread: the hide it waits for may itself need
/// the UI thread (macOS dispatches show/hide to the main thread).
#[tauri::command]
pub async fn set_video_visible(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    visible: bool,
) -> Result<(), String> {
    // The calling window's video only (plans/09 J4).
    let id = crate::windows::window_of(window.label()).unwrap_or(engine::WindowId::MAIN);
    state.engine.set_overlay(id, !visible).await;
    Ok(())
}

#[tauri::command]
pub async fn approve(
    state: tauri::State<'_, AppState>,
    from: String,
    decision: String,
) -> Result<(), ()> {
    let d = match decision.as_str() {
        "control" => ApprovalDecision::Allow(Permission::Control),
        "view" => ApprovalDecision::Allow(Permission::View),
        _ => ApprovalDecision::Deny,
    };
    state.decider.resolve(&from, d).await;
    Ok(())
}

#[tauri::command]
pub fn save_settings(
    state: tauri::State<'_, AppState>,
    patch: serde_json::Value,
) -> Result<(), String> {
    state.engine.update_config(|cfg| {
        if let Some(mode) = patch.get("mode").and_then(|v| v.as_str()) {
            cfg.mode = if mode == "password" {
                protocol::Mode::Password
            } else {
                protocol::Mode::Approve
            };
        }
        if let Some(pw) = patch.get("password").and_then(|v| v.as_str()) {
            if pw.is_empty() {
                cfg.set_password(None);
            } else {
                cfg.set_password(Some(pw));
            }
        }
        if let Some(perm) = patch.get("passwordPermission").and_then(|v| v.as_str()) {
            cfg.password_permission = if perm == "control" {
                Permission::Control
            } else {
                Permission::View
            };
        }
        if let Some(url) = patch.get("serverUrl").and_then(|v| v.as_str()) {
            cfg.server_url = url.to_string();
        }
        if let Some(b) = patch.get("shareAudio").and_then(|v| v.as_bool()) {
            cfg.share_audio = b;
        }
        if let Some(v) = patch.get("shareDisplayId") {
            cfg.share_display_id = v.as_str().filter(|s| !s.is_empty()).map(String::from);
        }
        if let Some(b) = patch.get("startWithWindows").and_then(|v| v.as_bool()) {
            cfg.start_with_windows = b;
        }
        if let Some(b) = patch.get("useOpenH264").and_then(|v| v.as_bool()) {
            cfg.use_openh264 = b;
        }
    });
    // The settings that follow the account (D5) live in the active workspace.
    // Save sends every field, so each is stamped only if its value changed —
    // otherwise this computer's stale values would beat newer changes made on
    // another one (08a §6.12 step 7). A change wakes the account sync. The
    // translator of a running session reads `translateModifiers` on its next
    // key event; held keys release as sent.
    for key in protocol::sync::SYNCED_PREFS {
        if let Some(b) = patch.get(key).and_then(|v| v.as_bool()) {
            state.engine.set_pref(key, b);
        }
    }
    // Explicit "forget everything" from the settings modal. Through the engine,
    // not by clearing the map here: the sealed values are only handles (a
    // Keychain / Secret Service item id on macOS and Linux), so dropping them from
    // the config alone would orphan the stored passwords in the OS store.
    if patch
        .get("clearSavedLogins")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        state.engine.clear_saved_logins();
    }
    // Takes effect for the next codec the engine creates (Cisco's licence: the
    // user can disable and re-enable the binary at any time).
    if let Some(b) = patch.get("useOpenH264").and_then(|v| v.as_bool()) {
        state.openh264.set_enabled(b);
    }

    // Apply the startup registration AFTER the config is persisted. A failure is
    // surfaced rather than swallowed (a startup toggle that silently does nothing
    // is worse than one that says why) — and the saved setting is rolled back, so
    // the config never claims an autostart that is not registered.
    if let Some(b) = patch.get("startWithWindows").and_then(|v| v.as_bool()) {
        if let Err(e) = state.engine.platform().autostart().set_enabled(b) {
            tracing::warn!("start at sign-in: {e}");
            state
                .engine
                .update_config(|cfg| cfg.start_with_windows = !b);
            return Err(e);
        }
    }
    Ok(())
}

/// Wire name of a permission state. Spelled out (not `{:?}`) so the UI contract
/// cannot drift if the enum's Debug output ever changes.
fn permission_state_name(s: PermissionState) -> &'static str {
    match s {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::NotDetermined => "notDetermined",
        PermissionState::NotApplicable => "notApplicable",
    }
}

fn permissions_json(report: &PermissionReport) -> serde_json::Value {
    serde_json::json!({
        "screenCapture": permission_state_name(report.screen_capture),
        "inputInjection": permission_state_name(report.input_injection),
        "inputMonitoring": permission_state_name(report.input_monitoring),
        "notes": report.notes,
    })
}

fn parse_os_permission(name: &str) -> Result<OsPermission, String> {
    match name {
        "screenCapture" => Ok(OsPermission::ScreenCapture),
        "inputInjection" => Ok(OsPermission::InputInjection),
        "inputMonitoring" => Ok(OsPermission::InputMonitoring),
        other => Err(format!("unknown permission '{other}'")),
    }
}

/// OS permissions the preflight modal lists. The UI polls this while the modal
/// is open, so it only reads state — it never prompts.
#[tauri::command]
pub fn get_permissions(state: tauri::State<'_, AppState>) -> serde_json::Value {
    permissions_json(&state.engine.platform().permissions())
}

/// Show the OS prompt (or open the matching settings pane) for one permission.
/// The outcome is not returned: the user answers out of band and the UI
/// re-polls `get_permissions`.
#[tauri::command]
pub fn request_permission(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    let permission = parse_os_permission(&name)?;
    state.engine.platform().request_permission(permission);
    Ok(())
}

/// What Settings → Advanced shows about the software codec: Cisco's required
/// notice and licence text, and where the download stands.
#[tauri::command]
pub fn openh264_info(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let availability = state.openh264.availability();
    serde_json::json!({
        "notice": codec_openh264::NOTICE,
        "license": codec_openh264::LICENSE_TEXT,
        "version": codec_openh264::VERSION,
        "ready": availability == Availability::Ready,
        "status": availability.describe(),
        "problem": (state.h264_problem)(),
    })
}

/// "Download now": fetch the Cisco binary ahead of need (or retry after an
/// offline first run). Blocking network I/O, so off the async runtime.
#[tauri::command]
pub async fn openh264_download(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let openh264 = state.openh264.clone();
    tauri::async_runtime::spawn_blocking(move || openh264.ensure())
        .await
        .map_err(|e| format!("download task failed: {e}"))?
        .map_err(|e| e.to_string())?;
    Ok(state.openh264.availability().describe())
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockPlatform;

    fn platform_with(report: PermissionReport) -> MockPlatform {
        let platform = MockPlatform::new(&std::env::temp_dir());
        *platform.permissions.lock().expect("mock lock") = report;
        platform
    }

    #[test]
    fn denied_report_serialises_to_the_ui_shape() {
        let platform = platform_with(PermissionReport {
            screen_capture: PermissionState::Denied,
            input_injection: PermissionState::NotDetermined,
            input_monitoring: PermissionState::NotApplicable,
            notes: vec!["a caveat".into()],
        });
        assert_eq!(
            permissions_json(&platform.permissions()),
            serde_json::json!({
                "screenCapture": "denied",
                "inputInjection": "notDetermined",
                "inputMonitoring": "notApplicable",
                "notes": ["a caveat"],
            })
        );
    }

    #[test]
    fn granted_report_serialises_to_the_ui_shape() {
        let platform = platform_with(PermissionReport {
            screen_capture: PermissionState::Granted,
            input_injection: PermissionState::Granted,
            input_monitoring: PermissionState::Granted,
            notes: Vec::new(),
        });
        assert_eq!(
            permissions_json(&platform.permissions()),
            serde_json::json!({
                "screenCapture": "granted",
                "inputInjection": "granted",
                "inputMonitoring": "granted",
                "notes": [],
            })
        );
    }

    #[test]
    fn request_names_map_to_permissions_and_unknown_is_an_error() {
        assert_eq!(
            parse_os_permission("screenCapture"),
            Ok(OsPermission::ScreenCapture)
        );
        assert_eq!(
            parse_os_permission("inputInjection"),
            Ok(OsPermission::InputInjection)
        );
        assert_eq!(
            parse_os_permission("inputMonitoring"),
            Ok(OsPermission::InputMonitoring)
        );
        assert!(parse_os_permission("camera").is_err());
    }

    #[test]
    fn a_parsed_request_reaches_the_platform() {
        let platform = platform_with(PermissionReport::all_not_applicable());
        let p = parse_os_permission("inputInjection").expect("known name");
        platform.request_permission(p);
        assert_eq!(
            *platform.permission_requests.lock().expect("mock lock"),
            vec![OsPermission::InputInjection]
        );
    }

    #[test]
    fn platform_info_has_the_shape_the_ui_reads() {
        let root = std::env::temp_dir().join("sct-app-platform-info");
        let platform = MockPlatform::new(&root);
        let v = platform_info_json(&platform);

        assert_eq!(v["name"], "mock");
        assert_eq!(v["startAtLoginLabel"], "Start at sign-in");
        assert!(v["shortcutCaptureHint"].is_string());
        assert_eq!(v["loginScreenSupported"], false);
        assert_eq!(
            v["downloadsDir"],
            root.join("downloads").display().to_string()
        );

        let displays = v["displays"].as_array().expect("displays is an array");
        assert_eq!(displays.len(), 1);
        let d = &displays[0];
        assert_eq!(d["id"], "mock-0");
        assert_eq!(d["name"], "Mock display");
        assert_eq!((d["x"].as_i64(), d["y"].as_i64()), (Some(0), Some(0)));
        assert!(d["width"].is_u64() && d["height"].is_u64());
        assert_eq!(d["scale"], 1.0);
        assert_eq!(d["primary"], true);
    }
}
