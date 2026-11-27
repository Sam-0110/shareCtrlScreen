//! Host media thread: capture → encode → hand AUs to the transport, with
//! viewer-size encoding, a bandwidth-following encoder bitrate, and rate-limited
//! on-demand keyframes.
//!
//! The encoder follows str0m's bandwidth estimate down in BITRATE (the driver
//! clamps it to the floor) and back up, always encoding at the FULL viewer-fit
//! resolution. A constrained link gets a softer but STEADY picture; the encode
//! size changes only when the viewer window actually resizes, never from the
//! bandwidth estimate (V1's resolution ladder caused a constant soften/re-sharpen
//! flap and was removed in V2). str0m's own pacer meters egress; there is no
//! hand-rolled source pacer (it was what pinned egress to the raw estimate and
//! kept the bandwidth estimate from ever probing back up — the self-sustaining
//! stutter V1 replaced, and we keep gone).
//!
//! ## References
//! * str0m 0.21 `Writer::write` paces RTP egress and NACK-repairs from its send
//!   buffer; the encoder need only produce frames sized to the estimate.
//! * Commit 352d56d: the proven-smooth precedent — encoder follows BWE down, no
//!   source pacer.
//! * `crate::tuning`: the pure [`encoder_target`](crate::tuning::encoder_target)
//!   bitrate policy.

use super::av1 as media_av1;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use platform_api::{
    BlockReason, CaptureError, Codec, DisplayInfo, EncoderConfig, Platform, ScreenCapture,
    ScreenState, SecureKind,
};
use protocol::control::hstate;

use crate::session::Shared;
use crate::tuning::{MAX_FPS, MAX_FPS_INTERVAL};

/// How often a paused main stream (a background tab, P6) checks for its
/// resume.
const PAUSED_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// What every host media thread of a session shares: the main stream's and
/// each pop-out stream's (D14). The transport thread holds one too, to start
/// pop-out streams.
#[derive(Clone)]
pub(crate) struct HostCtx {
    pub platform: Arc<dyn Platform>,
    pub codec: Codec,
    pub shared: Arc<Shared>,
    /// The reliable control channel, for cursor images (D9): an image is sent
    /// once, so it must not be lost the way a stale position may be. Also
    /// carries the monitor list to the viewer.
    pub ctl_tx: Sender<Vec<u8>>,
    /// The host's own UI: the monitor list for its picker, and why a monitor
    /// switch failed.
    pub ui: crate::SessionUi,
    /// Encoded AUs to the transport, tagged with their video stream slot.
    pub frame_tx: Sender<(u8, Vec<u8>, bool)>,
    /// Cursor positions to the transport (unreliable-latest channel).
    pub cursor_tx: Sender<Vec<u8>>,
    /// The session's smoothed encoder target; split evenly across the live
    /// streams (`Shared::live_streams`).
    pub bitrate: Arc<AtomicU32>,
    /// The session's stop flag.
    pub stop: Arc<AtomicBool>,
    /// The viewer advertised `Caps::hstate`: the main stream tells it what
    /// the screen is (plans/10 U1). Never sent to a viewer without it.
    pub hstate: bool,
}

/// The `hstate` message for `state` (10a §3.2).
pub(crate) fn host_state_msg(state: ScreenState) -> protocol::ControlMsg {
    let (screen, kind, why) = match state {
        ScreenState::Normal => (protocol::HostScreen::Normal, None, None),
        ScreenState::Secure(k) => (
            protocol::HostScreen::Secure,
            Some(match k {
                SecureKind::Lock => hstate::KIND_LOCK,
                SecureKind::Prompt => hstate::KIND_PROMPT,
                SecureKind::SignIn => hstate::KIND_SIGNIN,
            }),
            None,
        ),
        ScreenState::Blocked(r) => (
            protocol::HostScreen::Blocked,
            None,
            Some(match r {
                BlockReason::NotSupported => hstate::WHY_UNSUPPORTED,
                BlockReason::Disabled => hstate::WHY_DISABLED,
                BlockReason::NotAllowed => hstate::WHY_POLICY,
                BlockReason::AgentUnavailable => hstate::WHY_AGENT,
            }),
        ),
    };
    protocol::ControlMsg::HostState {
        screen,
        kind: kind.map(str::to_string),
        why: why.map(str::to_string),
    }
}

/// The main stream's report of what the host's screen is (plans/10 U1,
/// 10a §3.3, invariant K13). Only the main stream has one: a pop-out's
/// capture shows another monitor of the same machine, so it never says
/// anything the main one does not.
#[derive(Default)]
struct ScreenReport {
    /// What was last observed (and sent). `None` until the first look, so
    /// the first state is always sent: queued until the ctl channel opens,
    /// it is the value the viewer starts from.
    last: Option<ScreenState>,
}

impl ScreenReport {
    /// Record `state`; on a change, tell the viewer (if it understands
    /// `hstate`) and remember for a dropped file whether the screen is the
    /// ordinary one. Cheap when nothing changed: called on every pass.
    fn observe(&mut self, state: ScreenState, ctx: &HostCtx) {
        if self.last == Some(state) {
            return;
        }
        // Not logged for the very first, ordinary state: every session has it.
        if self.last.is_some() || state != ScreenState::Normal {
            tracing::info!("host: the screen is now {state:?}");
        }
        self.last = Some(state);
        ctx.shared
            .host_screen_secure
            .store(state != ScreenState::Normal, Ordering::SeqCst);
        if ctx.hstate {
            let msg = host_state_msg(state);
            let _ = ctx
                .ctl_tx
                .send(serde_json::to_vec(&msg).unwrap_or_default());
        }
    }
}

/// One video stream of the host.
pub(crate) struct StreamSpec {
    /// 0 = the main window's stream (switchable, see `Shared::display_req`);
    /// 1.. = a pop-out window's, pinned to `display`.
    pub slot: u8,
    /// `DisplayInfo::id` to share; `None` = primary (main stream only).
    pub display: Option<String>,
    /// Set to make this stream's encoder emit an IDR (rate-limited).
    pub force_key: Arc<AtomicBool>,
    /// The size the viewer shows this stream at, `w << 32 | h` (0 = unknown).
    pub view_size: Arc<AtomicU64>,
    /// Pop-out only: stops this stream alone (window closed, control revoked).
    pub stream_stop: Option<Arc<AtomicBool>>,
}

/// Most pop-out windows a session offers (one spare video track each).
pub(crate) const MAX_POPOUTS: u8 = 3;

/// The host's video streams: which monitor the main window shows and which
/// are streamed to pop-out windows (D14).
#[derive(Default)]
pub(crate) struct HostStreams {
    /// The main stream's monitor id ("" until known / none listed).
    pub current: String,
    /// Spare video tracks in this session's offer: pop-out slots are
    /// `1..=pop_slots`.
    pub pop_slots: u8,
    pub popouts: std::collections::BTreeMap<u8, HostPopout>,
}

/// A monitor streamed to a pop-out window.
pub(crate) struct HostPopout {
    pub id: String,
    pub stop: Arc<AtomicBool>,
    pub force_key: Arc<AtomicBool>,
    pub view_size: Arc<AtomicU64>,
}

impl HostStreams {
    /// Stop and forget the pop-out showing `id`, if any (e.g. the main window
    /// switched to that monitor). Returns whether one was removed.
    pub fn close_showing(&mut self, id: &str) -> bool {
        let slot = self
            .popouts
            .iter()
            .find(|(_, p)| p.id == id)
            .map(|(s, _)| *s);
        match slot.and_then(|s| self.popouts.remove(&s)) {
            Some(p) => {
                p.stop.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// Stop and forget every pop-out. Returns whether there were any.
    pub fn close_all(&mut self) -> bool {
        let any = !self.popouts.is_empty();
        for (_, p) in std::mem::take(&mut self.popouts) {
            p.stop.store(true, Ordering::SeqCst);
        }
        any
    }

    /// A free pop-out slot, if any.
    pub fn free_slot(&self) -> Option<u8> {
        (1..=self.pop_slots).find(|s| !self.popouts.contains_key(s))
    }
}

/// Keep `Shared::live_streams` (the bitrate divisor) in step with `streams`.
fn update_live(shared: &Shared, streams: &HostStreams) {
    shared
        .live_streams
        .store(1 + streams.popouts.len() as u32, Ordering::SeqCst);
}

/// The id of the display a capture opened with `display` actually shows:
/// that id when it is listed, else the primary (what `create_capture(None)`
/// shares), else the first listed. Empty when the platform lists none.
fn resolve_display_id(displays: &[DisplayInfo], display: Option<&str>) -> String {
    display
        .and_then(|id| displays.iter().find(|d| d.id == id))
        .or_else(|| displays.iter().find(|d| d.primary))
        .or_else(|| displays.first())
        .map(|d| d.id.clone())
        .unwrap_or_default()
}

/// Tell the viewer (ctl) and the host UI which monitors exist, which one the
/// main window shows and which are streamed to pop-out windows.
pub(crate) fn announce(ctx: &HostCtx) {
    let displays = ctx.platform.displays();
    let (current, popouts) = {
        let streams = ctx.shared.host_streams.lock();
        update_live(&ctx.shared, &streams);
        let popouts: Vec<protocol::PopoutStream> = streams
            .popouts
            .iter()
            .map(|(s, p)| protocol::PopoutStream {
                s: *s,
                id: p.id.clone(),
            })
            .collect();
        (streams.current.clone(), popouts)
    };
    announce_displays(&displays, &current, &popouts, &ctx.ctl_tx, &ctx.ui);
}

fn announce_displays(
    displays: &[DisplayInfo],
    current: &str,
    popouts: &[protocol::PopoutStream],
    ctl_tx: &Sender<Vec<u8>>,
    ui: &crate::SessionUi,
) {
    let list: Vec<protocol::RemoteDisplay> = displays
        .iter()
        .map(|d| protocol::RemoteDisplay {
            id: d.id.clone(),
            name: d.name.clone(),
            width: d.width,
            height: d.height,
            primary: d.primary,
        })
        .collect();
    let msg = protocol::ControlMsg::Displays {
        list: list.clone(),
        current: current.to_string(),
        popouts: popouts.to_vec(),
    };
    let _ = ctl_tx.send(serde_json::to_vec(&msg).unwrap_or_default());
    ui.send(crate::SessionEvent::Displays {
        list,
        current: current.to_string(),
        can_switch: true,
        popouts: popouts.to_vec(),
    });
}

/// Start streaming monitor `id` to a pop-out window, on a spare video track.
/// Refused (logged, `false`) when it is not listed, is what the main window
/// already shows, is already popped out, or no spare track is left. The
/// caller announces the result either way.
pub(crate) fn start_popout(ctx: &HostCtx, id: &str) -> bool {
    if !ctx.platform.displays().iter().any(|d| d.id == id) {
        tracing::warn!("host: pop-out of {id:?}: no such display");
        return false;
    }
    let (slot, spec) = {
        let mut streams = ctx.shared.host_streams.lock();
        if streams.current == id || streams.popouts.values().any(|p| p.id == id) {
            tracing::info!("host: pop-out of {id:?}: already shown");
            return false;
        }
        let Some(slot) = streams.free_slot() else {
            tracing::warn!(
                "host: pop-out of {id:?}: all {} spare streams in use",
                streams.pop_slots
            );
            return false;
        };
        let popout = HostPopout {
            id: id.to_string(),
            stop: Arc::new(AtomicBool::new(false)),
            force_key: Arc::new(AtomicBool::new(false)),
            view_size: Arc::default(),
        };
        let spec = StreamSpec {
            slot,
            display: Some(id.to_string()),
            force_key: popout.force_key.clone(),
            view_size: popout.view_size.clone(),
            stream_stop: Some(popout.stop.clone()),
        };
        streams.popouts.insert(slot, popout);
        update_live(&ctx.shared, &streams);
        (slot, spec)
    };
    tracing::info!("host: streaming monitor {id:?} to pop-out window {slot}");
    let thread_ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("sct-host-popout-{slot}"))
        .spawn(move || {
            let stream_stop = spec.stream_stop.clone();
            if let Err(e) = host_media_loop(thread_ctx.clone(), spec) {
                tracing::warn!("host pop-out stream {slot} ended: {e}");
                thread_ctx.ui.send(crate::SessionEvent::Toast(format!(
                    "Could not show that monitor in a separate window: {e}"
                )));
            }
            // Ended by itself (not closed): forget it and tell both sides, so
            // the viewer closes the window instead of showing a frozen frame.
            let mut streams = thread_ctx.shared.host_streams.lock();
            let mine = streams.popouts.get(&slot).is_some_and(|p| {
                stream_stop
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(s, &p.stop))
            });
            if mine && !thread_ctx.stop.load(Ordering::SeqCst) {
                streams.popouts.remove(&slot);
                drop(streams);
                announce(&thread_ctx);
            }
        });
    match spawned {
        Ok(h) => {
            ctx.shared.stream_threads.lock().push(h);
            true
        }
        Err(e) => {
            tracing::error!("could not start a pop-out stream thread: {e}");
            let mut streams = ctx.shared.host_streams.lock();
            streams.popouts.remove(&slot);
            update_live(&ctx.shared, &streams);
            false
        }
    }
}

/// A capture session and the display it shows (`None` = the primary).
type OpenedCapture = (Box<dyn ScreenCapture>, Option<String>);

/// Open the capture session, retrying the recoverable case instead of ending the
/// session before it starts. A service-launched host can be briefly off the active
/// input desktop at startup (the W8 regression) — a startup race, or the lock /
/// UAC secure desktop being up — which the backend reports as [`CaptureError::Lost`];
/// we retry with backoff until it returns or the session stops, so the loop
/// self-recovers exactly as it does for a mid-session desktop switch. A missing
/// saved display falls back to the primary; any other (terminal) error is returned.
/// Also returns the display actually opened (`None` = the primary, including
/// after that fallback). `on_wait` runs once, when the first attempt is
/// refused that way: the main stream tells the viewer then (plans/10 U1), so a
/// session started on a lock screen explains itself instead of staying black.
fn open_capture(
    platform: &dyn Platform,
    display: Option<&str>,
    fallback_to_primary: bool,
    stop: &dyn Fn() -> bool,
    on_wait: &mut dyn FnMut(),
) -> Result<OpenedCapture, Box<dyn std::error::Error>> {
    let mut backoff = std::time::Duration::from_millis(100);
    let mut waited = false;
    loop {
        if stop() {
            return Err("session stopped before screen capture became available".into());
        }
        let attempt = match platform.create_capture(display) {
            // A saved display that no longer exists (unplugged, or an id from an
            // older version): share the primary one rather than nothing. (Not
            // for a pop-out: it would duplicate the main window.)
            Err(CaptureError::NoOutput) if display.is_some() && fallback_to_primary => {
                // Bind first: `display` as a bare arg to the tracing macro is
                // parsed as its `display(...)` value formatter, not this variable.
                let wanted = display.unwrap_or_default();
                tracing::warn!("display {wanted:?} not found — sharing the primary display");
                platform.create_capture(None).map(|c| (c, None))
            }
            other => other.map(|c| (c, display.map(str::to_string))),
        };
        match attempt {
            Ok(c) => {
                if waited {
                    tracing::info!(
                        "host: screen capture became available — starting the fast DDA path"
                    );
                }
                return Ok(c);
            }
            Err(CaptureError::Lost) => {
                if !waited {
                    waited = true;
                    tracing::warn!(
                        "host: screen capture not ready yet — the active input desktop is \
                         unavailable (service host starting off the desktop, or a secure \
                         desktop / lock screen is up); retrying until it returns"
                    );
                    on_wait();
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Host capture → encode loop. Runs on its own thread; the capture session and
/// encoder are created here (they may hold thread-bound GPU/OS objects).
pub(crate) fn host_media_loop(
    ctx: HostCtx,
    spec: StreamSpec,
) -> Result<(), Box<dyn std::error::Error>> {
    let HostCtx {
        ref platform,
        codec,
        ref shared,
        ref ctl_tx,
        ref ui,
        ref frame_tx,
        ref cursor_tx,
        ref bitrate,
        ..
    } = ctx;
    let StreamSpec {
        slot,
        display,
        force_key,
        view_size,
        stream_stop,
    } = spec;
    let main = slot == 0;
    let stopped = || {
        ctx.stop.load(Ordering::SeqCst)
            || stream_stop
                .as_ref()
                .is_some_and(|s| s.load(Ordering::SeqCst))
    };
    // The bitrate this stream may use: the session's target, split evenly
    // across the live streams (main + pop-outs) — a paused main stream (P6)
    // sends nothing, so it takes no share.
    let my_bitrate = || {
        let paused = u32::from(shared.video_paused.load(Ordering::SeqCst));
        let n = shared
            .live_streams
            .load(Ordering::SeqCst)
            .saturating_sub(paused)
            .max(1);
        (bitrate.load(Ordering::SeqCst) / n).max(crate::tuning::MIN_BITRATE)
    };
    // The host's cursor images (D9). Per session: a new session (including a
    // reconnect) starts empty, so the first shape the new capture reports is
    // sent again even if an earlier session had sent it. Every stream shares
    // the record of what the viewer holds (one cache on the viewer).
    let mut cursor_images = shared.host_cursor_root.sharing();
    let codecs = platform.codecs();
    let mut report = main.then(ScreenReport::default);
    // Before a capture object exists there is no backend to ask. A refusal
    // that keeps the capture from even being created is a screen this host
    // cannot reach — before plans/10 U3, every secure desktop on Windows — so
    // the viewer hears `blocked`, once; the first `screen_state` of the
    // capture that finally opens takes it back to what it really is.
    let mut on_wait = || {
        if let Some(r) = report.as_mut() {
            r.observe(ScreenState::Blocked(BlockReason::NotSupported), &ctx);
        }
    };
    let (mut cap, opened) = open_capture(
        platform.as_ref(),
        display.as_deref(),
        main,
        &stopped,
        &mut on_wait,
    )?;
    if let Some(r) = report.as_mut() {
        r.observe(cap.screen_state(), &ctx);
    }
    // The monitor being shared, as an id from `platform.displays()`, announced
    // to both sides so either can offer a switch.
    let mut current_display = {
        let displays = platform.displays();
        let current = resolve_display_id(&displays, opened.as_deref());
        if main {
            shared.host_streams.lock().current = current.clone();
            announce(&ctx);
        }
        current
    };
    // Encode at the screen's REAL size. Assuming 1920x1080 made the GPU
    // converter rescale every frame on any other monitor (soft picture, wrong
    // aspect) — the viewer adapts to whatever size actually arrives.
    let (mut cap_w, mut cap_h) = cap.dimensions();
    let mut cfg = EncoderConfig {
        codec,
        bitrate_bps: my_bitrate(),
        // Even dimensions: H.264 chroma is subsampled 2x2 and MFTs reject odd sizes.
        width: (cap_w & !1).max(2),
        height: (cap_h & !1).max(2),
        fps_num: MAX_FPS,
        fps_den: 1,
    };
    // Encoder shares the capture device (§5c zero-copy).
    let mut encoder = codecs.new_encoder(cap.device(), cfg)?;
    // AV1: the encoder's temporal units in the form RTP expects (decided
    // once, logged; see media/av1.rs).
    let mut av1 = media_av1::Normalizer::default();
    tracing::info!(
        "host: capture+encoder ready ({}x{} @{}fps, {}, {} kbps)",
        cfg.width,
        cfg.height,
        MAX_FPS,
        codec.as_caps_str(),
        cfg.bitrate_bps / 1000,
    );
    // The first emitted frame must be an IDR so the viewer can start decoding.
    encoder.force_keyframe();
    let mut applied_bitrate = cfg.bitrate_bps;
    let mut encoder_size = (cfg.width, cfg.height);
    #[cfg(feature = "test-hooks")]
    if main {
        shared.encode_dims.store(
            (u64::from(encoder_size.0) << 32) | u64::from(encoder_size.1),
            Ordering::SeqCst,
        );
    }
    // A viewer size the encoder refused — don't retry it every frame.
    let mut rejected_size: Option<(u32, u32)> = None;
    let mut sent: u64 = 0;
    let mut last_frame_at = std::time::Instant::now();
    // When capture is down (reinit keeps failing because the input desktop is
    // unavailable — secure desktop/lock/switch), remember since when, so we log
    // the loss once and the recovery once rather than on every 250 ms retry.
    let mut capture_down_since: Option<std::time::Instant> = None;

    // Keyframe rate limiting + attribution (see the emit site below). The first
    // keyframe must not be delayed, so start the clock a full gap in the past.
    const MIN_KEYFRAME_GAP: std::time::Duration = std::time::Duration::from_millis(1000);
    let mut last_keyframe_at = std::time::Instant::now() - MIN_KEYFRAME_GAP;
    let mut requested_idr: u32 = 0; // we asked for it (connect / PLI / resize)
    let mut encoder_idr: u32 = 0; // Media Foundation decided on its own
    let mut deferred_keys: u32 = 0; // requests the rate limiter held back
    let mut last_key_log = std::time::Instant::now();

    while !stopped() {
        // Frame-rate cap: always run at MAX_FPS and never encode faster than
        // MAX_FPS_INTERVAL. The pacing sleep only bites when we're running FASTER
        // than the cap (a busy screen); a slow software encoder or an idle screen
        // sets the real rate. A constrained link is absorbed by the bitrate
        // following BWE down (below), not by dropping the frame rate.
        let since = last_frame_at.elapsed();
        if since < MAX_FPS_INTERVAL {
            std::thread::sleep(MAX_FPS_INTERVAL - since);
        }
        last_frame_at = std::time::Instant::now();
        // What the last acquire / reinit / switch found out (a field read).
        // Before the pause check, so a paused stream still reports what it
        // last saw; it observes nothing new until it acquires again (K13).
        if let Some(r) = report.as_mut() {
            r.observe(cap.screen_state(), &ctx);
        }

        // A monitor switch (host UI, or a controlling viewer). The new capture
        // is opened next to the old one and only replaces it once it AND an
        // encoder on its device both exist, so a failed switch keeps sharing
        // the current monitor. The encoder is always rebuilt: it lives on the
        // capture's GPU device (§5c zero-copy), and each capture has its own.
        let switch_to = if main {
            shared.display_req.lock().take()
        } else {
            None
        };
        if let Some(want_id) = switch_to {
            let displays = platform.displays();
            match displays.iter().find(|d| d.id == want_id) {
                None => {
                    tracing::warn!("host: monitor switch to {want_id:?}: no such display");
                    ui.send(crate::SessionEvent::Toast(
                        "That monitor is no longer connected".into(),
                    ));
                }
                Some(d) if d.id == current_display => {}
                Some(d) => {
                    cap.release();
                    match platform.create_capture(Some(&d.id)) {
                        Ok(new_cap) => {
                            let (w, h) = new_cap.dimensions();
                            let want = match view_size.load(Ordering::SeqCst) {
                                0 => fit_within((w, h), (w, h)),
                                v => fit_within(((v >> 32) as u32, v as u32), (w, h)),
                            };
                            let mut new_cfg = cfg;
                            new_cfg.width = want.0;
                            new_cfg.height = want.1;
                            match codecs.new_encoder(new_cap.device(), new_cfg) {
                                Ok(e) => {
                                    tracing::info!(
                                        "host: switched to monitor {:?} ({w}x{h}, encoding {}x{})",
                                        d.id,
                                        want.0,
                                        want.1
                                    );
                                    // Encoder first: it may hold a reference into
                                    // the old capture's device.
                                    encoder = e;
                                    // A new encoder (new size): its own sequence header comes next.
                                    av1.encoder_changed();
                                    cap = new_cap;
                                    cfg = new_cfg;
                                    (cap_w, cap_h) = (w, h);
                                    encoder_size = want;
                                    applied_bitrate = cfg.bitrate_bps;
                                    rejected_size = None;
                                    capture_down_since = None;
                                    #[cfg(feature = "test-hooks")]
                                    shared.encode_dims.store(
                                        (u64::from(encoder_size.0) << 32)
                                            | u64::from(encoder_size.1),
                                        Ordering::SeqCst,
                                    );
                                    // The main window now shows this monitor:
                                    // a pop-out of it would only duplicate it.
                                    {
                                        let mut streams = shared.host_streams.lock();
                                        streams.current = d.id.clone();
                                        streams.close_showing(&d.id);
                                    }
                                    // A new picture: the viewer's decoder restarts
                                    // here (new SPS/PPS, maybe a new size).
                                    encoder.force_keyframe();
                                    force_key.store(false, Ordering::SeqCst);
                                    last_keyframe_at = std::time::Instant::now();
                                    current_display = d.id.clone();
                                    // Input follows the picture.
                                    *shared.input_rect.lock() =
                                        Some(crate::session::input_rect_for(d));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "host: monitor switch to {:?}: no encoder ({e})",
                                        d.id
                                    );
                                    ui.send(crate::SessionEvent::Toast(format!(
                                        "Could not switch monitors: {e}"
                                    )));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("host: monitor switch to {:?}: {e}", d.id);
                            ui.send(crate::SessionEvent::Toast(format!(
                                "Could not switch monitors: {e}"
                            )));
                        }
                    }
                }
            }
            // Always answer, so a picker that optimistically showed the new
            // choice snaps back when the switch did not happen.
            announce(&ctx);
        }

        // §6 adaptive bitrate: feed the current BWE target to the encoder.
        let target = my_bitrate();
        if target != applied_bitrate {
            let _ = encoder.set_bitrate(target);
            applied_bitrate = target;
            cfg.bitrate_bps = target;
        }

        // Encode at the FULL size the viewer actually displays (never upscaling
        // past our own screen), so it presents 1:1 with no resampling. The GPU
        // converter already scales, so this costs nothing extra — and encoding
        // fewer pixels puts more bits into each one. The encode size tracks ONLY
        // the viewer window size (never the bandwidth estimate — V2 removed V1's
        // resolution ladder, which flapped the image between scales); a
        // constrained link is absorbed by the bitrate, not the resolution.
        // Rebuild only on a material change; the viewer debounces, and every
        // rebuild costs a keyframe.
        let want = match view_size.load(Ordering::SeqCst) {
            0 => (cap_w, cap_h),
            v => {
                let (vw, vh) = ((v >> 32) as u32, v as u32);
                fit_within((vw, vh), (cap_w, cap_h))
            }
        };
        if want != encoder_size && want.0 >= 2 && want.1 >= 2 && rejected_size != Some(want) {
            tracing::info!(
                "host: re-encoding at {}x{} (was {}x{})",
                want.0,
                want.1,
                encoder_size.0,
                encoder_size.1
            );
            cfg.width = want.0;
            cfg.height = want.1;
            match codecs.new_encoder(cap.device(), cfg) {
                Ok(e) => {
                    encoder = e;
                    // A new encoder (new size): its own sequence header comes next.
                    av1.encoder_changed();
                    encoder.force_keyframe(); // new SPS/PPS — restart the decoder
                    applied_bitrate = cfg.bitrate_bps;
                    encoder_size = want;
                    rejected_size = None;
                    #[cfg(feature = "test-hooks")]
                    if main {
                        shared.encode_dims.store(
                            (u64::from(encoder_size.0) << 32) | u64::from(encoder_size.1),
                            Ordering::SeqCst,
                        );
                    }
                }
                Err(e) => {
                    // Keep streaming at the working size; remember the bad one so
                    // we don't rebuild-storm on every frame.
                    tracing::warn!("host: encoder rebuild at {want:?} failed ({e}) — keeping size");
                    cfg.width = encoder_size.0;
                    cfg.height = encoder_size.1;
                    rejected_size = Some(want);
                }
            }
        }

        // Where keyframes actually come from — the open question the field logs
        // could not answer, because a request-side counter can't see the ones
        // Media Foundation inserts by itself.
        if last_key_log.elapsed() >= std::time::Duration::from_secs(5) {
            last_key_log = std::time::Instant::now();
            if requested_idr + encoder_idr + deferred_keys > 0 {
                tracing::info!(
                    "keyframes/5s: {requested_idr} requested, {encoder_idr} encoder-initiated, {deferred_keys} deferred"
                );
            }
            requested_idr = 0;
            encoder_idr = 0;
            deferred_keys = 0;
        }

        // Paused by the viewer (its tab is in the background, P6): no capture,
        // no encode, nothing on the wire for the main stream; monitor switches
        // and size changes above are still applied (and announced). Pop-out
        // streams go on. Nothing is lost by not capturing: the backends keep
        // what changed meanwhile (DXGI accumulates it, SCK and PipeWire keep
        // the newest frame, X11 accumulates damage), so the first capture after
        // the resume carries every change since the pause, and the viewer —
        // which decoded everything up to the pause — follows on from there.
        if main && shared.video_paused.load(Ordering::SeqCst) {
            std::thread::sleep(PAUSED_POLL);
            continue;
        }
        let acquired = cap.acquire(std::time::Duration::from_millis(16));
        match acquired {
            Ok(Some(frame)) => {
                // Cursor moves travel out-of-band on the cursor channel and never
                // wake the video encoder (§5a) — the viewer draws the sprite.
                // Normalize over the CAPTURED frame: the cursor position is in
                // capture pixels, which differ from the encode size whenever the
                // viewer's window is smaller than this screen.
                if let Some(cur) = &frame.cursor {
                    // A changed image goes out on the reliable channel BEFORE the
                    // position naming it, so the viewer usually has the image by
                    // the time it is asked to draw it (until then it keeps the
                    // image it was drawing).
                    let frame_size = (frame.frame.width, frame.frame.height);
                    {
                        // Across streams, the viewer must insert images in
                        // the order the shared record did.
                        let _order = shared.cursor_send.lock();
                        for part in cursor_images.update(cur.shape.as_ref(), frame_size) {
                            let _ = ctl_tx.send(serde_json::to_vec(&part).unwrap_or_default());
                        }
                    }
                    let x =
                        (cur.position.x as f64 / frame.frame.width.max(1) as f64).clamp(0.0, 1.0);
                    let y =
                        (cur.position.y as f64 / frame.frame.height.max(1) as f64).clamp(0.0, 1.0);
                    let msg = protocol::ControlMsg::Cursor {
                        x,
                        y,
                        shape: cursor_images.current(),
                        visible: cur.visible,
                        s: slot,
                    };
                    let _ = cursor_tx.send(serde_json::to_vec(&msg).unwrap_or_default());
                }

                // Any non-pointer-only frame carries new pixels (an empty dirty
                // list means the backend has no damage information).
                let has_change = !frame.pointer_only;

                // KEYFRAMES ON DEMAND ONLY (infinite GOP — the AnyDesk model).
                // No periodic IDR: at CBR every IDR costs ~10x a delta, so the
                // encoder crushes its quality and deltas spend the next second
                // sharpening — a visible blur PULSE every interval ("randomly
                // blurry, then not"). No scene-change IDR either: a full-screen
                // change is just a big delta the encoder intra-codes anyway.
                // Corruption can't linger because the viewer NEVER decodes across
                // a loss gap (it drops deltas and requests an IDR — see the
                // transport driver), so on-demand recovery covers everything:
                // connect, RTCP PLI, ctl KeyframeRequest, capture AccessLost.
                // RATE-LIMITED, and NEVER bypasses pacing. Field logs showed 3-5
                // IDRs every 5 seconds on idle content, and each one is a burst
                // big enough to spike the relay's queuing delay — which congestion
                // control reads as congestion, collapsing the estimate from ~5
                // Mbps to ~150 kbps, which then feeds a PLI → another IDR loop. We
                // break it at two points: at most one IDR per MIN_KEYFRAME_GAP,
                // and the IDR is handed to str0m like any other frame so ITS pacer
                // meters the packets onto the wire — the IDR is never dumped out
                // at once (the old `&& !force_key` pacer bypass is gone). With the
                // encoder following BWE down the IDR is small anyway. A request is
                // never dropped, only deferred: `force_key` stays set and fires as
                // soon as the gap has elapsed.
                let want_key = force_key.load(Ordering::SeqCst);
                let key_now = want_key && last_keyframe_at.elapsed() >= MIN_KEYFRAME_GAP;
                if want_key && !key_now {
                    deferred_keys += 1;
                }
                if !has_change && !key_now {
                    drop(frame);
                    cap.release();
                    continue;
                }
                if key_now {
                    encoder.force_keyframe();
                    force_key.store(false, Ordering::SeqCst);
                    last_keyframe_at = std::time::Instant::now();
                }
                // BGRA→NV12 + encode happen inside the encoder path (§5b).
                match encoder.encode(&frame.frame) {
                    Ok(units) => {
                        for u in units {
                            sent += 1;
                            #[cfg(feature = "test-hooks")]
                            if main {
                                shared.main_units.fetch_add(1, Ordering::SeqCst);
                            }
                            // Attribute every IDR. An IDR we did NOT ask for is
                            // the encoder's own doing (Media Foundation runs its
                            // own scene-change detection and honours GOPSize), and
                            // that is the one keyframe source we cannot see from
                            // the request side — so count it separately.
                            if u.keyframe {
                                if key_now {
                                    requested_idr += 1;
                                } else {
                                    encoder_idr += 1;
                                }
                            }
                            if sent == 1 || sent.is_multiple_of(120) {
                                tracing::info!(
                                    "host: encoded+sent AU #{sent} ({} bytes, keyframe={}, bitrate={}kbps)",
                                    u.data.len(),
                                    u.keyframe,
                                    applied_bitrate / 1000
                                );
                            }
                            let data = if codec == Codec::Av1 {
                                av1.normalize(u.data, u.keyframe)
                            } else {
                                u.data
                            };
                            let _ = frame_tx.send((slot, data, u.keyframe));
                        }
                    }
                    Err(e) => tracing::warn!("encode error: {e}"),
                }
                drop(frame);
                cap.release();
            }
            Ok(None) => { /* §5a idle: WAIT_TIMEOUT, send nothing */ }
            // The one recoverable error (mode change, display sleep, desktop
            // switch, or a service host off the input desktop — the W8 regression,
            // which the Windows backend maps here instead of ending the session).
            // `reinit` re-attaches the capture thread to the active input desktop
            // and re-acquires the duplication; if that still fails (secure desktop
            // up), back off and keep trying until the desktop returns.
            Err(CaptureError::Lost) => {
                let reinit = cap.reinit();
                // At once, not after the back-off below: the viewer hears
                // of a lock within one pass (plans/10 U1 acceptance: 2 s).
                if let Some(r) = report.as_mut() {
                    r.observe(cap.screen_state(), &ctx);
                }
                if let Err(e) = reinit {
                    if capture_down_since.is_none() {
                        capture_down_since = Some(std::time::Instant::now());
                        tracing::warn!(
                            "host: screen capture lost (reinit failed: {e}) — the active input \
                             desktop is unavailable (secure desktop / lock screen / session \
                             switch); retrying until it returns. This is a CAPTURE restart, not \
                             an encoder change."
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    continue;
                }
                if let Some(since) = capture_down_since.take() {
                    tracing::info!(
                        "host: screen capture re-acquired after {:?} — fast DDA path restored \
                         (capture restart, not an encoder change)",
                        since.elapsed()
                    );
                }
                // A mode change alters the screen size: re-read it, and the size
                // check above rebuilds the encoder on the next pass.
                let (w, h) = cap.dimensions();
                if (w, h) != (cap_w, cap_h) {
                    tracing::info!("host: capture size changed to {w}x{h}");
                    cap_w = w;
                    cap_h = h;
                }
                encoder.force_keyframe(); // new surface — resync the viewer
            }
            Err(e) => {
                tracing::warn!("capture: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// The largest `src`-aspect size that fits inside `want`, never upscaling past
/// `src`, rounded to even dimensions (H.264 chroma is subsampled 2x2 and MFTs
/// reject odd sizes). Preserving the source aspect matters: the viewer letterboxes
/// to the same ratio, so anything else would be displayed with bars AND rescaled.
pub(crate) fn fit_within(want: (u32, u32), src: (u32, u32)) -> (u32, u32) {
    if want.0 == 0 || want.1 == 0 || src.0 == 0 || src.1 == 0 {
        return src;
    }
    // Scale by the tighter of the two axes, capped at 1.0 (no upscaling — it
    // would cost bitrate without adding any real detail).
    let sx = want.0 as f64 / src.0 as f64;
    let sy = want.1 as f64 / src.1 as f64;
    let s = sx.min(sy).min(1.0);
    let w = ((src.0 as f64 * s).round() as u32) & !1;
    let h = ((src.1 as f64 * s).round() as u32) & !1;
    (w.max(2), h.max(2))
}

#[cfg(test)]
mod tests {
    use super::{fit_within, host_state_msg, HostCtx, ScreenReport};
    use platform_api::{BlockReason, ScreenState, SecureKind};
    use protocol::{ControlMsg, HostScreen};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn wire(state: ScreenState) -> String {
        serde_json::to_string(&host_state_msg(state)).unwrap()
    }

    #[test]
    fn every_screen_state_has_its_wire_words() {
        assert_eq!(
            wire(ScreenState::Normal),
            r#"{"t":"hstate","screen":"normal"}"#
        );
        for (k, word) in [
            (SecureKind::Lock, "lock"),
            (SecureKind::Prompt, "prompt"),
            (SecureKind::SignIn, "signin"),
        ] {
            assert_eq!(
                wire(ScreenState::Secure(k)),
                format!(r#"{{"t":"hstate","screen":"secure","kind":"{word}"}}"#)
            );
        }
        for (r, word) in [
            (BlockReason::NotSupported, "unsupported"),
            (BlockReason::Disabled, "disabled"),
            (BlockReason::NotAllowed, "policy"),
            (BlockReason::AgentUnavailable, "agent"),
        ] {
            assert_eq!(
                wire(ScreenState::Blocked(r)),
                format!(r#"{{"t":"hstate","screen":"blocked","why":"{word}"}}"#)
            );
        }
    }

    /// A host context whose ctl messages land in the returned receiver.
    fn ctx(hstate: bool) -> (HostCtx, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let (frame_tx, _) = std::sync::mpsc::channel();
        let (cursor_tx, _) = std::sync::mpsc::channel();
        let platform = Arc::new(platform_api::mock::MockPlatform::new(&std::env::temp_dir()));
        let ctx = HostCtx {
            platform,
            codec: platform_api::Codec::H264,
            shared: Arc::default(),
            ctl_tx,
            ui: crate::SessionUi {
                id: crate::SessionId(1),
                peer: "viewer".into(),
                host: true,
                tx: ui_tx,
            },
            frame_tx,
            cursor_tx,
            bitrate: Arc::default(),
            stop: Arc::default(),
            hstate,
        };
        (ctx, ctl_rx)
    }

    fn screens(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<HostScreen> {
        rx.try_iter()
            .map(
                |b| match serde_json::from_slice::<ControlMsg>(&b).unwrap() {
                    ControlMsg::HostState { screen, .. } => screen,
                    other => panic!("unexpected {other:?}"),
                },
            )
            .collect()
    }

    /// K13: the first state goes out (it is what the viewer starts from),
    /// then only changes — a reinit retried every 250 ms on a lock screen
    /// sends `blocked` once.
    #[test]
    fn the_viewer_hears_the_first_state_and_each_change_once() {
        let (ctx, rx) = ctx(true);
        let mut r = ScreenReport::default();
        let blocked = ScreenState::Blocked(BlockReason::NotSupported);
        r.observe(ScreenState::Normal, &ctx);
        for _ in 0..10 {
            r.observe(blocked, &ctx);
        }
        assert!(ctx.shared.host_screen_secure.load(Ordering::SeqCst));
        r.observe(ScreenState::Secure(SecureKind::Lock), &ctx);
        assert!(ctx.shared.host_screen_secure.load(Ordering::SeqCst));
        r.observe(ScreenState::Normal, &ctx);
        r.observe(ScreenState::Normal, &ctx);
        assert!(!ctx.shared.host_screen_secure.load(Ordering::SeqCst));
        assert_eq!(
            screens(&rx),
            [
                HostScreen::Normal,
                HostScreen::Blocked,
                HostScreen::Secure,
                HostScreen::Normal
            ]
        );
    }

    /// An old viewer (no `Caps::hstate`) is never sent `hstate`; the host
    /// still knows its own screen (a dropped file is not pasted onto it).
    #[test]
    fn an_old_viewer_never_receives_hstate() {
        let (ctx, rx) = ctx(false);
        let mut r = ScreenReport::default();
        r.observe(ScreenState::Normal, &ctx);
        r.observe(ScreenState::Blocked(BlockReason::NotSupported), &ctx);
        assert!(ctx.shared.host_screen_secure.load(Ordering::SeqCst));
        r.observe(ScreenState::Normal, &ctx);
        assert!(rx.try_iter().next().is_none());
    }

    #[test]
    fn fit_within_never_upscales() {
        // A viewer window bigger than the screen must not inflate the encode.
        assert_eq!(fit_within((3840, 2160), (1920, 1080)), (1920, 1080));
    }

    #[test]
    fn fit_within_preserves_aspect_and_evenness() {
        // Maximized 1080p viewer: some height lost to chrome. The result keeps
        // 16:9 (the viewer letterboxes to that) and stays even for 4:2:0 chroma.
        let (w, h) = fit_within((1920, 1040), (1920, 1080));
        assert_eq!((w, h), (1848, 1040));
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn fit_within_handles_degenerate_input() {
        // Unknown/zero sizes fall back to the source rather than producing 0x0.
        assert_eq!(fit_within((0, 0), (1920, 1080)), (1920, 1080));
        assert_eq!(fit_within((1280, 720), (0, 0)), (0, 0));
    }
}
