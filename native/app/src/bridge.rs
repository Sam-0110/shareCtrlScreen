//! Engine `UiEvent`s → Tauri events for the web UI (replaces Electron's
//! `ipcRenderer.on`). Translation only: every decision lives in the engine or
//! in the module it hands off to (menus, pop-outs, the tray).

use std::sync::Arc;

use engine::{Engine, Role, UiEvent};
use tauri::Emitter;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::commands::displays_json;
use crate::menu::update_monitor_menu;
use crate::popout;
#[cfg(windows)]
use crate::tray;

/// Forward every engine event to the web UI until the engine's channel closes.
pub(crate) fn spawn(
    handle: tauri::AppHandle,
    engine: Arc<Engine>,
    mut ui_rx: UnboundedReceiver<UiEvent>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = ui_rx.recv().await {
            let (name, payload) = match ev {
                UiEvent::ServerStatus(s) => ("server-status", serde_json::json!({ "status": s })),
                UiEvent::ApprovalRequest { from } => {
                    #[cfg(windows)]
                    tray::show_for_approval(&handle);
                    ("approval-request", serde_json::json!({ "from": from }))
                }
                UiEvent::PasswordRequired { from } => {
                    ("password-required", serde_json::json!({ "from": from }))
                }
                // Structured, not Debug-formatted: the UI needs the peer id and
                // permission as real fields.
                UiEvent::RoleChanged(role) => (
                    "role-changed",
                    match role {
                        Role::Idle => serde_json::json!({ "role": "idle" }),
                        Role::Host { peer, permission } => serde_json::json!({
                            "role": "host",
                            "peer": peer,
                            "permission": permission.as_str(),
                        }),
                        Role::Viewer { peer, permission } => serde_json::json!({
                            "role": "viewer",
                            "peer": peer,
                            "permission": permission.as_str(),
                        }),
                    },
                ),
                UiEvent::Toast(msg) => ("toast", serde_json::json!({ "message": msg })),
                UiEvent::LinkTrouble { secs_left } => {
                    ("link-trouble", serde_json::json!({ "secsLeft": secs_left }))
                }
                UiEvent::LinkRestored => ("link-restored", serde_json::json!({})),
                UiEvent::Displays {
                    list,
                    current,
                    can_switch,
                    popouts,
                } => {
                    update_monitor_menu(&handle, &list, &current, can_switch, &popouts);
                    // Viewer: open/close pop-out windows to match.
                    if matches!(engine.role(), Role::Viewer { .. }) {
                        popout::reconcile(&handle, &engine, &list, &popouts);
                    }
                    (
                        "displays",
                        displays_json(&list, &current, can_switch, &popouts),
                    )
                }
            };
            // A session that ended takes its monitor list and its pop-out
            // windows with it.
            if name == "role-changed" && payload["role"] == "idle" {
                update_monitor_menu(&handle, &[], "", false, &[]);
                popout::close_all(&handle, &engine);
            }
            let _ = handle.emit(name, payload);
        }
    });
}
