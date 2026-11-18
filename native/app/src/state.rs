//! Shared Tauri state: the engine, the approve-mode decider and the codec
//! handles every command reads.

use std::collections::HashMap;
use std::sync::Arc;

use codec_openh264::OpenH264;
use engine::handshake::{ApprovalDecision, Decider};
use engine::Engine;
use tokio::sync::{oneshot, Mutex};

/// Approve-mode decider backed by the WebView2 modal: `decide` parks on a
/// oneshot channel that the `approve` command resolves when the human clicks.
#[derive(Default)]
pub(crate) struct AppDecider {
    pending: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
}

impl Decider for AppDecider {
    fn decide<'a>(
        &'a self,
        from: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'a>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(from.to_string(), tx);
            // 30 s auto-deny (contract §3.3): whichever fires first wins.
            match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                Ok(Ok(decision)) => decision,
                _ => ApprovalDecision::Deny,
            }
        })
    }
}

impl AppDecider {
    pub(crate) async fn resolve(&self, from: &str, decision: ApprovalDecision) {
        if let Some(tx) = self.pending.lock().await.remove(from) {
            let _ = tx.send(decision);
        }
    }
}

/// Shared Tauri state.
pub(crate) struct AppState {
    pub(crate) engine: Arc<Engine>,
    /// The accounts on this computer and their sync (accounts.rs).
    pub(crate) accounts: Arc<crate::accounts::AccountManager>,
    pub(crate) decider: Arc<AppDecider>,
    /// The Cisco OpenH264 software codec (Settings → Advanced).
    pub(crate) openh264: Arc<OpenH264>,
    /// `Some(advice)` when this computer has no H.264 codec at all; shown next
    /// to the OpenH264 toggle.
    pub(crate) h264_problem: H264Problem,
}

/// Asks the platform whether H.264 is missing altogether (it can change when
/// the user toggles OpenH264, so it is asked each time, not cached).
pub(crate) type H264Problem = Arc<dyn Fn() -> Option<String> + Send + Sync>;
