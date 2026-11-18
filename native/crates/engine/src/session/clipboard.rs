//! Clipboard text sync, both directions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use platform_api::{Clipboard, Platform, MAX_TEXT_BYTES};
use protocol::ControlMsg;

use super::serialize;

/// How often the local clipboard is polled for changes.
const POLL: Duration = Duration::from_millis(300);

/// Mirror clipboard TEXT between the two machines. The clipboard is created on
/// this thread (some backends are thread-bound) and polled via its change
/// counter. Applying a value received from the peer bumps that same counter, so
/// the resulting value is remembered and skipped — otherwise each side would
/// echo the other forever.
pub(super) fn clipboard_loop(
    platform: Arc<dyn Platform>,
    ctl_tx: Sender<Vec<u8>>,
    inbound: Receiver<String>,
    wake: Option<Receiver<()>>,
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let clipboard = platform.create_clipboard();
    let mut sync = ClipboardSync::new(clipboard.as_ref());
    while !stop.load(Ordering::SeqCst) {
        sync.step(
            clipboard.as_ref(),
            &ctl_tx,
            &inbound,
            enabled.load(Ordering::SeqCst),
        );
        // `wake`: the gate changed (viewer) — step again at once. Without one
        // (or once its sender is gone) a plain poll.
        match wake.as_ref().map(|w| w.recv_timeout(POLL)) {
            Some(Ok(())) => while wake.as_ref().is_some_and(|w| w.try_recv().is_ok()) {},
            Some(Err(RecvTimeoutError::Timeout)) => {}
            Some(Err(RecvTimeoutError::Disconnected)) | None => std::thread::sleep(POLL),
        }
    }
}

/// The echo-guarded sync state machine, separate from the thread so it can be
/// tested deterministically.
struct ClipboardSync {
    last_count: u64,
    last_text: Option<String>,
    /// The gate was open on the last step. Its rising edge re-baselines.
    was_enabled: bool,
}

impl ClipboardSync {
    fn new(clipboard: &dyn Clipboard) -> Self {
        // Whatever is on the clipboard when the session starts is not sent: only
        // copies made during the session are.
        Self {
            last_count: clipboard.change_count(),
            last_text: None,
            was_enabled: true,
        }
    }

    fn step(
        &mut self,
        clipboard: &dyn Clipboard,
        ctl_tx: &Sender<Vec<u8>>,
        inbound: &Receiver<String>,
        enabled: bool,
    ) {
        if !enabled {
            // Closed (another tab is active, sync is off, or control was
            // revoked): text from the peer is discarded, not queued, and the
            // clipboard is not even read — it is nobody's business meanwhile.
            while inbound.try_recv().is_ok() {}
            self.was_enabled = false;
            return;
        }
        if !self.was_enabled {
            // The gate just opened: whatever is on the clipboard now was put
            // there while it was closed — copied for another tab's host, or
            // received from it — and is never sent here (plans/08 B3, 08a §6.4
            // step 4). Taken at the edge, not while closed, so no change can
            // slip in between the last closed poll and the opening.
            self.was_enabled = true;
            self.last_count = clipboard.change_count();
        }
        // Apply anything the peer sent first, so the poll below sees the
        // resulting change count and treats it as ours.
        while let Ok(text) = inbound.try_recv() {
            if self.last_text.as_deref() == Some(text.as_str()) {
                continue;
            }
            if let Some(count) = clipboard.set_text(&text) {
                self.last_count = count;
                self.last_text = Some(text);
            }
        }

        let count = clipboard.change_count();
        if count == self.last_count {
            return;
        }
        self.last_count = count;
        if let Some(text) = clipboard.get_text() {
            if self.last_text.as_deref() != Some(text.as_str()) && text.len() <= MAX_TEXT_BYTES {
                self.last_text = Some(text.clone());
                let _ = ctl_tx.send(serialize(&ControlMsg::Clipboard { text }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockClipboard;
    use std::sync::mpsc;

    fn sent(rx: &Receiver<Vec<u8>>) -> Vec<String> {
        rx.try_iter()
            .map(
                |b| match serde_json::from_slice::<ControlMsg>(&b).unwrap() {
                    ControlMsg::Clipboard { text } => text,
                    other => panic!("unexpected {other:?}"),
                },
            )
            .collect()
    }

    #[test]
    fn local_copies_are_sent_once_and_peer_text_is_never_echoed() {
        let clip = MockClipboard::default();
        clip.user_copies("before the session");
        let (ctl_tx, ctl_rx) = mpsc::channel();
        let (peer_tx, peer_rx) = mpsc::channel();
        let mut s = ClipboardSync::new(&clip);

        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert!(
            sent(&ctl_rx).is_empty(),
            "pre-session clipboard is not sent"
        );

        clip.user_copies("hello");
        s.step(&clip, &ctl_tx, &peer_rx, true);
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(sent(&ctl_rx), ["hello"]);

        peer_tx.send("from peer".into()).unwrap();
        s.step(&clip, &ctl_tx, &peer_rx, true);
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(clip.get_text().as_deref(), Some("from peer"));
        assert!(
            sent(&ctl_rx).is_empty(),
            "applied peer text must not bounce back"
        );
    }

    #[test]
    fn disabled_sync_neither_sends_nor_applies() {
        let clip = MockClipboard::default();
        let (ctl_tx, ctl_rx) = mpsc::channel();
        let (peer_tx, peer_rx) = mpsc::channel();
        let mut s = ClipboardSync::new(&clip);
        clip.user_copies("secret");
        peer_tx.send("injected".into()).unwrap();
        s.step(&clip, &ctl_tx, &peer_rx, false);
        assert!(sent(&ctl_rx).is_empty());
        assert_eq!(clip.get_text().as_deref(), Some("secret"));
        // Re-enabling does not replay text that arrived while disabled.
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(clip.get_text().as_deref(), Some("secret"));
        assert!(
            sent(&ctl_rx).is_empty(),
            "nor sends what was copied while it was disabled"
        );
        clip.user_copies("after");
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(sent(&ctl_rx), ["after"]);
    }

    #[test]
    fn a_copy_made_right_before_the_gate_opens_is_not_sent() {
        // The switch race: the closed gate polled, then a copy (for the tab
        // that was active) landed, then this tab became active.
        let clip = MockClipboard::default();
        let (ctl_tx, ctl_rx) = mpsc::channel();
        let (_peer_tx, peer_rx) = mpsc::channel();
        let mut s = ClipboardSync::new(&clip);
        s.step(&clip, &ctl_tx, &peer_rx, false);
        clip.user_copies("for the other tab");
        s.step(&clip, &ctl_tx, &peer_rx, true);
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert!(sent(&ctl_rx).is_empty());
        clip.user_copies("for this tab");
        s.step(&clip, &ctl_tx, &peer_rx, true);
        assert_eq!(sent(&ctl_rx), ["for this tab"]);
    }
}
