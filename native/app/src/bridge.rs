//! Engine `UiEvent`s → Tauri events for the web UI (replaces Electron's
//! `ipcRenderer.on`). Translation only: every decision lives in the engine or
//! in the module it hands off to (menus, pop-outs, the tray).
//!
//! The engine tags every session event with its [`SessionId`]; until the tab
//! UI arrives (plans/08 P4) the web UI shows one session at a time, so the
//! tagged events are translated into the single-session events it already
//! understands (`role-changed`, `password-required`, `displays`, ...).

use std::sync::Arc;

use engine::{Engine, PwReason, SessionEvent, SessionId, SessionPhase, UiEvent};
use serde_json::{json, Value};
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
            for (name, payload) in translate(&handle, &engine, ev) {
                let _ = handle.emit(name, payload);
            }
        }
    });
}

/// The Tauri events for one engine event, after its side effects on native
/// UI (tray, menus, pop-out windows).
fn translate(
    handle: &tauri::AppHandle,
    engine: &Arc<Engine>,
    ev: UiEvent,
) -> Vec<(&'static str, Value)> {
    match ev {
        UiEvent::ServerStatus(s) => vec![("server-status", json!({ "status": s }))],
        UiEvent::ApprovalRequest { from } => {
            #[cfg(windows)]
            tray::show_for_approval(handle);
            vec![("approval-request", json!({ "from": from }))]
        }
        UiEvent::Toast(msg) => vec![("toast", json!({ "message": msg }))],
        UiEvent::WorkspaceChanged => vec![("workspace-changed", json!({}))],
        UiEvent::Session { id, peer, host, ev } => session(handle, engine, id, peer, host, ev),
    }
}

fn session(
    handle: &tauri::AppHandle,
    engine: &Arc<Engine>,
    id: SessionId,
    peer: String,
    host: bool,
    ev: SessionEvent,
) -> Vec<(&'static str, Value)> {
    match ev {
        // The UI already shows "requesting" when it asked to connect.
        SessionEvent::Phase(SessionPhase::Requesting) => Vec::new(),
        SessionEvent::Phase(SessionPhase::PasswordRequired { reason }) => {
            let mut out = Vec::new();
            if reason == PwReason::SavedRejected {
                out.push((
                    "toast",
                    json!({ "message": "Saved password no longer works" }),
                ));
            }
            out.push(("password-required", json!({ "from": peer })));
            out
        }
        // Structured, not Debug-formatted: the UI needs the peer id and
        // permission as real fields.
        SessionEvent::Phase(SessionPhase::Negotiating { permission }) => vec![(
            "role-changed",
            json!({
                "role": if host { "host" } else { "viewer" },
                "peer": peer,
                "permission": permission.as_str(),
            }),
        )],
        SessionEvent::Phase(SessionPhase::Ended(reason)) => {
            // A session that ended takes its monitor list and its pop-out
            // windows with it.
            update_monitor_menu(handle, &[], "", false, &[]);
            popout::close_all(handle, engine);
            let mut out = Vec::new();
            if let Some(message) = reason.message() {
                out.push(("toast", json!({ "message": message })));
            }
            out.push(("role-changed", json!({ "role": "idle" })));
            out
        }
        SessionEvent::LinkTrouble { secs_left } => {
            vec![("link-trouble", json!({ "secsLeft": secs_left }))]
        }
        SessionEvent::LinkRestored => vec![("link-restored", json!({}))],
        SessionEvent::Toast(msg) => vec![("toast", json!({ "message": msg }))],
        SessionEvent::Displays {
            list,
            current,
            can_switch,
            popouts,
        } => {
            update_monitor_menu(handle, &list, &current, can_switch, &popouts);
            // Viewer: open/close pop-out windows to match.
            if !host {
                popout::reconcile(handle, engine, id, &list, &popouts);
            }
            vec![(
                "displays",
                displays_json(&list, &current, can_switch, &popouts),
            )]
        }
        // Nothing to release yet: every session shares the one main surface.
        SessionEvent::Released => Vec::new(),
    }
}
