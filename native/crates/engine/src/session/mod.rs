//! One remote-control session: WebRTC setup, the session's
//! threads, and teardown. The host offers and streams; the viewer answers,
//! decodes, presents and sends input.
//!
//! Threads per session (all named `sct-*`, all stopped by one flag and joined by
//! [`Session::stop_and_join`]):
//! * transport — owns the str0m `Rtc` + UDP socket ([`driver`]),
//! * host media (capture → encode) or viewer media (decode → present),
//! * clipboard sync,
//! * viewer only: input translation, file push, and optionally shortcut capture.

mod clipboard;
mod ctl_queue;
mod driver;
mod files;
mod ice;
pub(crate) mod registry;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use platform_api::{FileDrop, KeyForwarder, Rect, VideoInput};
use protocol::{ControlMsg, InputMsg, Permission, SignalData, SignalMsg};

use crate::events::{SessionEvent, SessionId};
use crate::media::host::{host_media_loop, HostCtx, HostStreams, StreamSpec, MAX_POPOUTS};
use crate::media::viewer::{viewer_media_loop, ViewerMedia};
use crate::modifiers::{KeyTranslator, OsFamily};
use crate::Engine;

pub(crate) use crate::tuning::{DESIRED_BITRATE, TARGET_BITRATE};

/// State shared by one session's threads. Per session (not process-wide) so two
/// engines in one process — the loopback test — never see each other's state.
#[derive(Default)]
pub(crate) struct Shared {
    /// Host: the viewer's display size `w << 32 | h` (0 = unknown) for the main
    /// stream. The host encodes to it so the viewer presents 1:1.
    pub view_size: Arc<AtomicU64>,
    /// Viewer: latest cursor from the host, drawn as a sprite by the presenter.
    pub cursor: Mutex<Option<ViewerPointer>>,
    /// Viewer: the host's cursor images received so far (task D9). One cache
    /// for the session: a pop-out stream's `Shared` holds the same `Arc`.
    pub cursor_images: Arc<Mutex<crate::media::cursor::ViewerCursors>>,
    /// Viewer: whether this session currently has CONTROL (task D10). Set from
    /// the granted permission at session start and updated on every inbound
    /// `ControlMsg::Perm`. While `true` and the local pointer is over the
    /// video, the media thread draws the cursor sprite at the LOCAL pointer
    /// (instant) instead of the host's round-trip-delayed out-of-band echo.
    pub control: AtomicBool,
    /// Viewer (task D12): a one-shot "repaint now" request. Set when the video
    /// surface is re-shown (e.g. Settings closed mid-session) — re-showing does
    /// not itself repaint, and the present loop otherwise only paints on a new
    /// decoded frame (or a cursor move, D11), so a STATIC host screen stays
    /// black until the next frame. The idle tick checks this, re-presents the
    /// held frame once and clears it (or, with nothing decoded yet, asks for a
    /// keyframe so a fresh frame arrives to show).
    pub repaint_requested: AtomicBool,
    /// Host: a monitor switch (a `DisplayInfo::id`) waiting for the media
    /// thread, which owns the capture. Set by the host UI or by a controlling
    /// viewer's `seldisp`; the newest request wins.
    pub display_req: Mutex<Option<String>>,
    /// Host: the injector's new target rectangle once a switch has taken
    /// effect (`Some(None)` = the primary display). Set by the media thread
    /// right after the new capture is live, taken by the transport thread
    /// (which owns the injector), so input follows the picture.
    pub input_rect: Mutex<Option<Option<Rect>>>,
    /// Host (D14): the main stream's monitor and the pop-out streams.
    pub host_streams: Mutex<HostStreams>,
    /// Host: how many video streams are live (main + pop-outs) — the encoder
    /// bitrate is split evenly across them. 0 is read as 1.
    pub live_streams: AtomicU32,
    /// Host: the record of cursor images the viewer holds, shared by every
    /// stream (`HostCursor::sharing`), and the lock that keeps their sends in
    /// that record's order.
    pub host_cursor_root: crate::media::cursor::HostCursor,
    pub cursor_send: Mutex<()>,
    /// Threads started after the session began (host pop-out streams), joined
    /// at teardown.
    pub stream_threads: Mutex<Vec<JoinHandle<()>>>,
    /// Viewer (D14): the open pop-out windows by stream slot. Only on the main
    /// stream's `Shared`.
    pub popouts: Mutex<std::collections::BTreeMap<u8, ViewerPopout>>,
    /// Viewer: the stream the host currently maps pointer input onto. Every
    /// window's input thread sends `InputTarget` before its first positional
    /// message when this differs from its own slot (under this lock, so the
    /// switch and the message stay in order on ctl).
    pub input_target: Arc<Mutex<u8>>,
    /// Viewer: the last pop-out attachment generation handed out.
    pub popout_gen: AtomicU64,
    /// Test hook (`test-hooks` feature only): the host's current encode size,
    /// `w << 32 | h`, so the loopback can assert it stays full resolution as the
    /// link varies (V2 has no resolution ladder).
    #[cfg(feature = "test-hooks")]
    pub encode_dims: AtomicU64,
    /// Test hook (`test-hooks` feature only): a forced bandwidth estimate in bps
    /// (0 = none). While set, the driver re-applies it every tick and ignores
    /// str0m's real estimate, so a loopback test can drive a constrained-then-
    /// recovering link deterministically. Never written in a shipping build.
    #[cfg(feature = "test-hooks")]
    pub bwe_inject: AtomicU32,
    /// Test hook (`test-hooks` feature only): make the transport declare the
    /// link dead on its next pass, as a cut network would. Never written in a
    /// shipping build.
    #[cfg(feature = "test-hooks")]
    pub sever: AtomicBool,
}

/// The pointer as the last position message described it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ViewerPointer {
    /// Hotspot, normalized `[0,1]` over the video.
    pub x: f64,
    pub y: f64,
    pub visible: bool,
    /// The image to draw (an id in [`Shared::cursor_images`]); `None` = the
    /// presenter's built-in arrow.
    pub shape: Option<u32>,
}

/// Viewer: one pop-out window's stream (D14).
pub(crate) struct ViewerPopout {
    /// Which attachment this is (a slot can be reused by a later window).
    pub gen: u64,
    /// Its own present-loop state (cursor, repaint); shares the cursor images.
    pub shared: Arc<Shared>,
    /// Reassembled AUs for its decoder.
    pub video_tx: Sender<(Vec<u8>, bool)>,
    pub surface: Arc<dyn platform_api::VideoSurface>,
    pub stop: Arc<AtomicBool>,
    pub threads: Vec<JoinHandle<()>>,
}

/// What a viewer session keeps to open pop-out windows later.
#[derive(Clone)]
pub(crate) struct ViewerCtx {
    platform: Arc<dyn platform_api::Platform>,
    codec: platform_api::Codec,
    keys: Arc<Mutex<KeyTranslator>>,
}

/// The live session, owned by [`Engine`].
pub(crate) struct Session {
    peer: String,
    stop: Arc<AtomicBool>,
    /// Inbound answer/ICE from signaling → transport thread.
    signal_tx: Sender<SignalData>,
    /// Outbound control (perm / input / clipboard) → transport thread.
    ctl_tx: Sender<Vec<u8>>,
    /// Host: whether injecting remote input is allowed right now (the live
    /// `control` permission). Revoking releases held keys/buttons.
    control: Arc<AtomicBool>,
    /// Whether clipboard sync is live. Follows control (and the user setting), so
    /// revoking control also stops clipboard exchange.
    clip_on: Arc<AtomicBool>,
    /// The user's clipboard-sync setting at session start.
    clipboard_setting: bool,
    /// The per-session shared state: on the viewer so [`request_repaint`] can
    /// set the repaint flag when the surface is re-shown (task D12); on the host
    /// so [`request_display`] can hand a monitor switch to the media thread.
    shared: Arc<Shared>,
    /// Viewer: what opening a pop-out window needs. `None` on a host.
    viewer: Option<ViewerCtx>,
    threads: Vec<JoinHandle<()>>,
    /// Test hooks (`test-hooks` feature only): the host's rate state, so the
    /// loopback can observe the encoder target / raw link and inject estimates.
    /// `None` on a viewer session.
    #[cfg(feature = "test-hooks")]
    hooks: Option<TestHooks>,
}

/// Host-side rate state exposed to tests under the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
pub(crate) struct TestHooks {
    pub bitrate: Arc<AtomicU32>,
    pub link: Arc<AtomicU32>,
    pub shared: Arc<Shared>,
}

pub(crate) fn serialize(msg: &ControlMsg) -> Vec<u8> {
    serde_json::to_vec(msg).unwrap_or_default()
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("sct-{name}"))
        .spawn(f)
}

/// Spawn a session thread or abandon the session: stop and join what already
/// started, end the session with a message. (A failed spawn means the OS is out
/// of threads; panicking would abort the whole app in release builds.)
macro_rules! spawn_or_abort {
    ($engine:expr, $id:expr, $stop:expr, $threads:expr, $name:literal, $body:expr) => {
        match spawn($name, $body) {
            Ok(h) => $threads.push(h),
            Err(e) => {
                tracing::error!("could not start the {} thread: {e}", $name);
                $stop.store(true, Ordering::SeqCst);
                for t in $threads.drain(..) {
                    let _ = t.join();
                }
                $engine.fail_session($id, "the session could not be started");
                return;
            }
        }
    };
}

/// The rectangle input should map onto when sharing a non-primary display.
fn target_rect_for(engine: &Engine, display: Option<&str>) -> Option<Rect> {
    let id = display?;
    let d = engine
        .platform()
        .displays()
        .into_iter()
        .find(|d| d.id == id)?;
    input_rect_for(&d)
}

/// Where `[0,1]` input maps for display `d`: `None` (the injector's simple
/// primary-display path) for the primary, its virtual-desktop rectangle
/// otherwise.
pub(crate) fn input_rect_for(d: &platform_api::DisplayInfo) -> Option<Rect> {
    (!d.primary).then_some(Rect {
        x: d.x,
        y: d.y,
        w: d.width,
        h: d.height,
    })
}

// ---- host ------------------------------------------------------------------

/// Host: build the offer and data channels, start the transport, capture →
/// encode and clipboard threads, then send the offer to `peer`.
/// `popouts`: the viewer said it can show pop-out windows (D14), so spare
/// video tracks are worth offering.
pub(crate) fn begin_host(
    engine: &Engine,
    id: SessionId,
    peer: String,
    permission: Permission,
    popouts: bool,
    codec: platform_api::Codec,
) {
    // Set when the session ends any way; the transport thread reports a dead
    // link only if nothing ended it first.
    let Some(ended) = engine.host_ended_flag(id) else {
        return; // ended before it started
    };
    let ui = engine.session_ui(id, &peer, true);
    let platform = engine.platform();
    let cfg = engine.config();
    // The person's synced settings (clipboard, dropped files) come from the
    // active workspace (plans/08 D5); the defaults while signed out.
    let prefs = engine.prefs();
    let shared = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<(u8, Vec<u8>, bool)>();
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Offer exactly the negotiated codec, so the media track's payload type is
    // unambiguous. BWE (congestion control) steers the encoder bitrate.
    let mut rtc = {
        let builder = str0m::RtcConfig::new()
            .enable_bwe(Some(str0m::bwe::Bitrate::bps(TARGET_BITRATE as u64)))
            .clear_codecs();
        let builder = match codec {
            platform_api::Codec::Hevc => builder.enable_h265(true),
            platform_api::Codec::Av1 => builder.enable_av1(true),
            platform_api::Codec::H264 => builder.enable_h264(true),
        };
        builder.build(Instant::now())
    };
    // Bind + gather candidates BEFORE generating the offer so it carries them.
    let ice = ice::ice_config_from(engine);
    let Some((socket, turn_alloc)) = ice::bind_and_gather(&mut rtc, &ice) else {
        tracing::error!("host: failed to bind a UDP socket");
        engine.fail_session(id, "no network socket available");
        return;
    };
    // Data channels (ctl reliable, cursor unreliable-latest, bulk reliable);
    // video is an RTP media track.
    let channels = {
        let [c, cur, blk] = transport::channel_configs();
        let mut dapi = rtc.direct_api();
        let ctl = dapi.create_data_channel(c);
        let cursor = dapi.create_data_channel(cur);
        let bulk = dapi.create_data_channel(blk);
        transport::Channels { ctl, cursor, bulk }
    };
    // One video track for the main window, plus a spare per possible pop-out
    // window (D14): a monitor besides the shared one, up to MAX_POPOUTS. A
    // spare carries nothing until a pop-out is opened, and having them in the
    // offer means opening one needs no renegotiation. Their order is the slot
    // number; the viewer reads it back from the m-line order.
    let pop_slots = if popouts {
        (platform.displays().len().saturating_sub(1)).min(MAX_POPOUTS as usize) as u8
    } else {
        0
    };
    let mut api = rtc.sdp_api();
    let video_mids: Vec<str0m::media::Mid> = (0..=pop_slots)
        .map(|_| {
            api.add_media(
                str0m::media::MediaKind::Video,
                str0m::media::Direction::SendOnly,
                None,
                None,
                None,
            )
        })
        .collect();
    shared.host_streams.lock().pop_slots = pop_slots;
    // One SDP-negotiated channel forces the m=application section (the SCTP
    // association the direct channels ride on). Its label is never used.
    api.add_channel("init".to_string());
    let Some((offer, pending)) = api.apply() else {
        tracing::error!("host: str0m produced no offer");
        engine.fail_session(id, "the connection could not be set up");
        return;
    };

    let control = Arc::new(AtomicBool::new(permission == Permission::Control));
    let bitrate = Arc::new(AtomicU32::new(TARGET_BITRATE));
    let link = Arc::new(AtomicU32::new(TARGET_BITRATE));
    let force_key = Arc::new(AtomicBool::new(false));
    let host_ctx = HostCtx {
        platform: platform.clone(),
        codec,
        shared: shared.clone(),
        ctl_tx: ctl_tx.clone(),
        ui: ui.clone(),
        frame_tx,
        cursor_tx,
        bitrate: bitrate.clone(),
        stop: stop.clone(),
    };
    // Clipboard sync is gated on the SAME live permission as input: a view-only
    // peer must not read this machine's clipboard (it can hold a password that
    // was never on screen) or write to it.
    let clip_on = Arc::new(AtomicBool::new(
        prefs.clipboard_sync && permission == Permission::Control,
    ));
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<String>();
    let mut threads = Vec::new();

    {
        let (platform, ctl, gate, stop_c) = (
            platform.clone(),
            ctl_tx.clone(),
            clip_on.clone(),
            stop.clone(),
        );
        spawn_or_abort!(engine, id, stop, threads, "clipboard", move || {
            clipboard::clipboard_loop(platform, ctl, clip_rx, gate, stop_c)
        });
    }

    let driver = driver::Driver {
        rtc,
        pending: Some(pending),
        signal_rx,
        ctl_rx,
        frame_rx: Some(frame_rx),
        video_tx: None,
        inject: Some(control.clone()),
        cursor_rx: Some(cursor_rx),
        bulk_rx: None,
        clip_tx,
        channels: Some(channels),
        video_mids,
        force_key: Some(force_key.clone()),
        bitrate: Some(bitrate.clone()),
        link: Some(link.clone()),
        ui: ui.clone(),
        ended: ended.clone(),
        end_tx: engine.ended_sender(),
        id,
        socket,
        turn: turn_alloc,
        stop: stop.clone(),
        platform: platform.clone(),
        surface: None,
        shared: shared.clone(),
        downloads_dir: platform.paths().downloads_dir,
        paste_dropped: prefs.paste_dropped_files,
        target_rect: target_rect_for(engine, cfg.share_display_id.as_deref()),
        host: Some(host_ctx.clone()),
    };
    spawn_or_abort!(engine, id, stop, threads, "transport", move || {
        driver::transport_driver(driver)
    });

    // Clone the host's rate state for the test hooks before the Arcs are moved
    // into the media loop (no-op without the `test-hooks` feature).
    #[cfg(feature = "test-hooks")]
    let (hooks_bitrate, hooks_link, hooks_shared) = (bitrate.clone(), link.clone(), shared.clone());
    let host_shared = shared.clone();

    {
        let spec = StreamSpec {
            slot: 0,
            display: cfg.share_display_id.clone(),
            force_key,
            view_size: shared.view_size.clone(),
            stream_stop: None,
        };
        let ui = ui.clone();
        spawn_or_abort!(engine, id, stop, threads, "host-media", move || {
            if let Err(e) = host_media_loop(host_ctx, spec) {
                tracing::warn!("host media loop ended: {e}");
                ui.send(SessionEvent::Toast(format!("Screen sharing stopped: {e}")));
            }
        });
    }

    // The initial permission, delivered once the ctl channel opens (§4.2).
    let _ = ctl_tx.send(serialize(&ControlMsg::Perm { value: permission }));

    // Register the session BEFORE the offer leaves: the answer must find it.
    let session = Session {
        peer: peer.clone(),
        stop,
        signal_tx,
        ctl_tx,
        control,
        clip_on,
        clipboard_setting: prefs.clipboard_sync,
        shared: host_shared,
        viewer: None,
        threads,
        #[cfg(feature = "test-hooks")]
        hooks: Some(TestHooks {
            bitrate: hooks_bitrate,
            link: hooks_link,
            shared: hooks_shared,
        }),
    };
    if !engine.store_host_session(id, session) {
        return; // ended while starting: its threads were stopped
    }
    let _ = engine.signaling().send(SignalMsg::Signal {
        to: Some(peer),
        from: None,
        data: SignalData::Offer {
            sdp: offer.to_sdp_string(),
        },
    });
}

// ---- viewer ------------------------------------------------------------------

/// What a viewer session needs from its registry slot to start.
pub(crate) struct ViewerStart {
    pub(crate) peer: String,
    pub(crate) surface: Option<Arc<dyn platform_api::VideoSurface>>,
    pub(crate) codec: platform_api::Codec,
    pub(crate) host_os: OsFamily,
    pub(crate) permission: Permission,
    pub(crate) ended: Arc<AtomicBool>,
}

/// Viewer: the host's offer arrived for session `id` — answer it and start the
/// session's threads.
pub(crate) fn start_viewer(engine: &Engine, id: SessionId, offer_sdp: String) {
    let Some(start) = engine.viewer_start(id) else {
        return; // ended meanwhile
    };
    let ViewerStart {
        peer,
        surface,
        codec,
        host_os,
        permission,
        ended,
    } = start;
    let Some(surface) = surface else {
        tracing::error!("viewer: no video surface on this platform");
        engine.fail_session(id, "this computer cannot display the remote screen yet");
        return;
    };
    let ui = engine.session_ui(id, &peer, false);
    let platform = engine.platform();
    let prefs = engine.prefs();
    let shared = Arc::new(Shared::default());
    // Viewer control state (task D10): drives the local-pointer cursor
    // prediction and the OS-cursor hide. The host re-asserts it over ctl via
    // `Perm`, but seeding it from the permission the host already granted means
    // the first frames are correct before that message arrives.
    let has_control = permission == Permission::Control;
    shared.control.store(has_control, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (video_tx, video_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();

    let mut rtc = str0m::Rtc::new(Instant::now());
    let ice = ice::ice_config_from(engine);
    let Some((socket, turn_alloc)) = ice::bind_and_gather(&mut rtc, &ice) else {
        tracing::error!("viewer: failed to bind a UDP socket");
        engine.fail_session(id, "no network socket available");
        return;
    };
    let answer = match str0m::change::SdpOffer::from_sdp_string(&offer_sdp)
        .map_err(|e| e.to_string())
        .and_then(|offer| rtc.sdp_api().accept_offer(offer).map_err(|e| e.to_string()))
    {
        Ok(answer) => answer,
        Err(e) => {
            tracing::warn!("viewer: could not accept the host's offer: {e}");
            engine.fail_session(id, "the connection could not be set up");
            return;
        }
    };

    let mut threads = Vec::new();

    // File push: drops on the video surface are streamed as bulk frames from
    // their own thread, so a slow disk stays off the transport loop.
    let (bulk_tx, bulk_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (drop_tx, drop_rx) = std::sync::mpsc::channel::<FileDrop>();
    {
        let (ui, stop_f) = (ui.clone(), stop.clone());
        spawn_or_abort!(engine, id, stop, threads, "file-push", move || {
            files::file_send_loop(drop_rx, bulk_tx, ui, stop_f)
        });
    }

    // Clipboard sync. The host applies its own permission gate; here the user's
    // setting governs.
    let clip_on = Arc::new(AtomicBool::new(prefs.clipboard_sync));
    let (clip_tx, clip_rx) = std::sync::mpsc::channel::<String>();
    {
        let (platform, ctl, gate, stop_c) = (
            platform.clone(),
            ctl_tx.clone(),
            clip_on.clone(),
            stop.clone(),
        );
        spawn_or_abort!(engine, id, stop, threads, "clipboard", move || {
            clipboard::clipboard_loop(platform, ctl, clip_rx, gate, stop_c)
        });
    }

    // Every forwarded key — from the surface and from the shortcut hook — goes
    // through this one translator, which rewrites modifiers for the host's OS
    // and remembers what it sent for each held key (D8).
    let keys = {
        let viewer_os = OsFamily::from_name(platform.info().name);
        tracing::info!("key translation: {viewer_os:?} viewer → {host_os:?} host");
        Arc::new(Mutex::new(KeyTranslator::new(
            viewer_os,
            host_os,
            engine.translate_modifiers_handle(),
        )))
    };

    // Input from the video surface → protocol messages on ctl. The host enforces
    // its own permission (defence in depth).
    let (input_tx, input_rx) = std::sync::mpsc::channel::<VideoInput>();
    {
        let target = InputTarget {
            slot: 0,
            current: shared.input_target.clone(),
        };
        let (ctl, keys, stop_i) = (ctl_tx.clone(), keys.clone(), stop.clone());
        spawn_or_abort!(engine, id, stop, threads, "input", move || {
            input_loop(input_rx, ctl, keys, target, stop_i)
        });
    }

    let driver = driver::Driver {
        rtc,
        pending: None,
        signal_rx,
        ctl_rx,
        frame_rx: None,
        video_tx: Some(video_tx),
        inject: None,
        cursor_rx: None,
        bulk_rx: Some(bulk_rx),
        clip_tx,
        channels: None, // learned from ChannelOpen by label
        // Stream slots in the offer's m-line order (D14); MediaAdded fills
        // them in if the offer could not be read.
        video_mids: transport::video_mids_in_sdp(&offer_sdp),
        force_key: None,
        bitrate: None,
        link: None,
        ui: ui.clone(),
        ended: ended.clone(),
        end_tx: engine.ended_sender(),
        id,
        socket,
        turn: turn_alloc,
        stop: stop.clone(),
        platform: platform.clone(),
        surface: Some(surface.clone()),
        shared: shared.clone(),
        downloads_dir: platform.paths().downloads_dir,
        paste_dropped: false,
        target_rect: None,
        host: None,
    };
    spawn_or_abort!(engine, id, stop, threads, "transport", move || {
        driver::transport_driver(driver)
    });

    {
        let media = ViewerMedia {
            platform: platform.clone(),
            surface: surface.clone(),
            codec,
            shared: shared.clone(),
            slot: 0,
        };
        let (ctl, stop_r, ui) = (ctl_tx.clone(), stop.clone(), ui.clone());
        spawn_or_abort!(engine, id, stop, threads, "viewer-media", move || {
            if let Err(e) = viewer_media_loop(media, video_rx, ctl, stop_r) {
                tracing::warn!("viewer media loop ended: {e}");
                ui.send(SessionEvent::Toast(format!(
                    "Cannot display the remote screen: {e}"
                )));
            }
        });
    }

    // OS shortcut capture (Win/Alt+Tab, Cmd+Tab, Super), when the user enabled it.
    if prefs.capture_shortcuts {
        let (platform, surface, ctl, keys, stop_k, hook_input) = (
            platform.clone(),
            surface.clone(),
            ctl_tx.clone(),
            keys.clone(),
            stop.clone(),
            input_tx.clone(),
        );
        spawn_or_abort!(engine, id, stop, threads, "shortcuts", move || {
            let Some(mut hook) = platform.create_shortcut_hook() else {
                tracing::info!("shortcut capture is not available on this platform");
                return;
            };
            let forward = hook_forwarder(hook_input, keys, ctl);
            if hook.install(surface.as_ref(), forward) {
                hook.run(&stop_k);
            } else {
                tracing::info!("shortcut capture could not be installed");
            }
            hook.uninstall();
        });
    }

    surface.set_drop_sink(Some(drop_tx));
    surface.set_input_sink(Some(input_tx));
    // Hide the OS cursor over the video while controlling (task D10); a
    // view-only session leaves it shown. Mirrors `shared.control`.
    surface.set_cursor_control(has_control);
    let session = Session {
        peer: peer.clone(),
        stop,
        signal_tx,
        ctl_tx,
        control: Arc::new(AtomicBool::new(false)),
        clip_on,
        clipboard_setting: prefs.clipboard_sync,
        shared,
        viewer: Some(ViewerCtx {
            platform: platform.clone(),
            codec,
            keys,
        }),
        threads,
        #[cfg(feature = "test-hooks")]
        hooks: None,
    };
    if !engine.store_viewer_session(id, session) {
        return; // ended while starting: its threads were stopped
    }
    let _ = engine.signaling().send(SignalMsg::Signal {
        to: Some(peer),
        from: None,
        data: SignalData::Answer {
            sdp: answer.to_sdp_string(),
        },
    });
}

/// Queue one input message for the host on the ctl channel.
fn send_input(ctl: &Sender<Vec<u8>>, msg: &InputMsg) {
    let _ = ctl.send(serde_json::to_vec(msg).unwrap_or_default());
}

/// The shortcut hook's forwarder: its keys join the surface's input channel,
/// so the translator sees hook keys and surface keys in the order they were
/// produced (M12).
///
/// WHY not lock the translator directly from the hook thread (D8's first
/// version): the surface's keys wait in `input` (behind a pointer move the
/// input thread may be pacing for up to 10 ms), so a Cmd sent by the surface
/// could reach the translator **after** the Cmd+C that the hook took a moment
/// later, and the host would get C, then Ctrl. The hook guarantees that the
/// modifier is in the channel before its own key (it forwards the modifier
/// itself unless the surface already did); one FIFO channel turns that into
/// the order the host sees. A channel send never blocks, as the hook's OS
/// callback requires.
///
/// Fallback to the translator directly: a code outside the shared keymap
/// (cannot be a `&'static str` for [`VideoInput::Key`]), or the input thread
/// already gone at session end (a release must still reach the host).
fn hook_forwarder(
    input: Sender<VideoInput>,
    keys: Arc<Mutex<KeyTranslator>>,
    ctl: Sender<Vec<u8>>,
) -> KeyForwarder {
    Box::new(move |code: &str, down: bool| {
        if let Some(code) = platform_api::keymap::from_dom(code).map(platform_api::keymap::dom_for)
        {
            if input.send(VideoInput::Key { code, down }).is_ok() {
                return;
            }
        }
        keys.lock()
            .key(code, down, &mut |msg| send_input(&ctl, &msg));
    })
}

/// Translate surface input to protocol messages, coalescing pointer moves.
/// Keys go through `keys` (modifier translation for the host's OS).
///
/// Gaming mice emit up to 1000 moves/s; relayed over the reliable ordered
/// channel they would queue AHEAD of clicks and keys and make input feel drunk on
/// slow links. Moves are capped at ~100/s and always carry the LATEST position;
/// clicks, keys and wheel are never delayed beyond the coalescing drain.
fn input_loop(
    input_rx: std::sync::mpsc::Receiver<VideoInput>,
    ctl: Sender<Vec<u8>>,
    keys: Arc<Mutex<KeyTranslator>>,
    target: InputTarget,
    stop: Arc<AtomicBool>,
) {
    const MOVE_INTERVAL: Duration = Duration::from_millis(10);
    let send = |ev: VideoInput| {
        if let VideoInput::Key { code, down } = ev {
            keys.lock()
                .key(code, down, &mut |msg| send_input(&ctl, &msg));
        } else if let Some(msg) = translate_input(ev) {
            // Positional: make sure the host maps it onto THIS window's
            // monitor. Held across the send so another window's switch cannot
            // slip in between.
            let mut current = target.current.lock();
            if *current != target.slot {
                *current = target.slot;
                let _ = ctl.send(serialize(&ControlMsg::InputTarget { s: target.slot }));
            }
            send_input(&ctl, &msg);
        }
    };
    let mut last_move_sent = Instant::now() - MOVE_INTERVAL;
    while !stop.load(Ordering::SeqCst) {
        match input_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(mut ev) => {
                let mut follow: Option<VideoInput> = None;
                if matches!(ev, VideoInput::Move { .. }) {
                    let since = last_move_sent.elapsed();
                    if since < MOVE_INTERVAL {
                        std::thread::sleep(MOVE_INTERVAL - since);
                    }
                    while let Ok(next) = input_rx.try_recv() {
                        if matches!(next, VideoInput::Move { .. }) {
                            ev = next; // newer position supersedes
                        } else {
                            follow = Some(next);
                            break;
                        }
                    }
                    last_move_sent = Instant::now();
                }
                send(ev);
                if let Some(f) = follow {
                    send(f);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Keys still queued when the session stops (the surface's and the
    // shortcut hook's releases at teardown) go out anyway: a dropped key-up
    // would leave that key held on the host until it notices the disconnect.
    for ev in input_rx.try_iter() {
        if matches!(ev, VideoInput::Key { .. }) {
            send(ev);
        }
    }
}

/// Which video stream an input thread's window shows, and the session-wide
/// record of which stream the host maps pointer input onto (D14).
struct InputTarget {
    slot: u8,
    current: Arc<Mutex<u8>>,
}

/// Pointer and wheel input → wire message. Buttons outside 0..=2 are dropped.
/// Keys are not mapped here: they need the session's held-key state, which
/// lives in [`KeyTranslator`], so `None`.
fn translate_input(ev: VideoInput) -> Option<InputMsg> {
    use protocol::Button;
    Some(match ev {
        VideoInput::Move { nx, ny } => InputMsg::Move { x: nx, y: ny },
        VideoInput::Button {
            button,
            down,
            nx,
            ny,
        } => {
            let b = Button::try_from(button).ok()?;
            if down {
                InputMsg::ButtonDown { b, x: nx, y: ny }
            } else {
                InputMsg::ButtonUp { b, x: nx, y: ny }
            }
        }
        VideoInput::Wheel { dx, dy } => InputMsg::Wheel { dx, dy },
        VideoInput::Key { .. } => return None,
    })
}

// ---- session control -----------------------------------------------------------

impl Session {
    /// Hand an inbound WebRTC payload (answer, ICE) to the transport thread.
    pub(crate) fn signal(&self, data: SignalData) {
        let _ = self.signal_tx.send(data);
    }

    /// Send a control message (perm change, keyframe request) on the ctl channel.
    pub(crate) fn send_ctl(&self, msg: &ControlMsg) {
        let _ = self.ctl_tx.send(serialize(msg));
    }

    /// Host: flip the injection gate when the live permission changes (§4.2).
    /// The transport thread releases held input when control is revoked.
    pub(crate) fn set_control(&self, allow: bool) {
        self.control.store(allow, Ordering::SeqCst);
        // Clipboard exchange is part of "control": revoking it must stop the
        // peer reading this machine's clipboard too, not just its input.
        self.clip_on
            .store(allow && self.clipboard_setting, Ordering::SeqCst);
    }

    /// Viewer (task D12): ask the present loop to repaint the last decoded
    /// frame once, immediately. Called after the video surface is re-shown
    /// (e.g. Settings closed mid-session): `show()` does not itself repaint,
    /// and on a static host screen the surface would otherwise stay black
    /// until the next frame.
    pub(crate) fn request_repaint(&self) {
        self.shared.repaint_requested.store(true, Ordering::SeqCst);
    }

    /// Host: switch the shared monitor to `id`. The media thread picks it up
    /// on its next pass, swaps the capture, re-targets input and announces the
    /// result to both UIs.
    pub(crate) fn request_display(&self, id: String) {
        *self.shared.display_req.lock() = Some(id);
    }

    /// Stop every thread of this session (the flag first, so they all wind
    /// down together) and join them, its pop-out streams and windows too.
    /// Blocking: called on a reaper thread, never under a lock.
    pub(crate) fn stop_and_join(self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads {
            let _ = t.join();
        }
        // Host pop-out streams, and viewer pop-out windows (D14).
        let extra: Vec<_> = self.shared.stream_threads.lock().drain(..).collect();
        for t in extra {
            let _ = t.join();
        }
        let popouts = std::mem::take(&mut *self.shared.popouts.lock());
        for (_, p) in popouts {
            stop_viewer_popout(p);
        }
    }

    /// Set the stop flag only (the threads wind down; joining comes later).
    pub(crate) fn signal_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// A viewer session's surface as it ends: input and drops stop at once (held
/// keys are released first, by the surface contract), the OS cursor is
/// restored (task D10) and the picture is hidden. Synchronous, so a session
/// that ends never keeps capturing input or showing a frozen frame while its
/// threads are being joined.
pub(crate) fn detach_surface(s: &dyn platform_api::VideoSurface) {
    s.set_input_sink(None);
    s.set_drop_sink(None);
    s.set_cursor_control(false);
    s.hide();
}

/// Viewer: no stream known — the next positional input from any window sends
/// `InputTarget` again (after a pop-out opens or closes, a slot may be reused
/// by another monitor; the host may also have mapped input elsewhere).
const NO_INPUT_TARGET: u8 = u8::MAX;

/// Viewer (D14): whether pop-out stream `slot` of session `id` currently has
/// a window attached.
pub(crate) fn viewer_popout_attached(engine: &Engine, id: SessionId, slot: u8) -> bool {
    engine
        .with_viewer(id, |s| s.shared.popouts.lock().contains_key(&slot))
        .unwrap_or(false)
}

/// What opening a pop-out window takes from its session.
struct PopoutParent {
    ctx: ViewerCtx,
    main: Arc<Shared>,
    ctl_tx: Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    ui: crate::SessionUi,
}

/// Viewer (D14): the app opened a window for pop-out stream `slot` of session
/// `id` and made `surface` in it. Start that stream's decode → present and
/// input threads. Returns the attachment's generation (what
/// [`close_viewer_popout`] takes), or `None` without that viewer session, for
/// slot 0, or when the slot is already attached.
pub(crate) fn open_viewer_popout(
    engine: &Engine,
    id: SessionId,
    slot: u8,
    surface: Arc<dyn platform_api::VideoSurface>,
) -> Option<u64> {
    let parent = engine
        .with_viewer(id, |session| {
            let ctx = session.viewer.clone()?;
            if slot == 0 || session.shared.popouts.lock().contains_key(&slot) {
                return None;
            }
            Some(PopoutParent {
                ctx,
                main: session.shared.clone(),
                ctl_tx: session.ctl_tx.clone(),
                stop: session.stop.clone(),
                ui: engine.session_ui(id, &session.peer, false),
            })
        })
        .flatten()?;
    let PopoutParent {
        ctx,
        main,
        ctl_tx,
        stop: session_stop,
        ui,
    } = parent;

    let control = main.control.load(Ordering::SeqCst);
    let shared = Arc::new(Shared {
        cursor_images: main.cursor_images.clone(),
        ..Shared::default()
    });
    shared.control.store(control, Ordering::SeqCst);
    // The window's threads stop when it closes OR the session stops (the
    // watcher carries the latter over, so a dead session never leaves a
    // pop-out presenting).
    let loop_stop = Arc::new(AtomicBool::new(false));
    let (video_tx, video_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    let (input_tx, input_rx) = std::sync::mpsc::channel::<VideoInput>();
    let mut threads = Vec::new();
    let watcher = {
        let (loop_stop, session_stop) = (loop_stop.clone(), session_stop.clone());
        spawn(&format!("popout-{slot}-stop"), move || {
            while !loop_stop.load(Ordering::SeqCst) {
                if session_stop.load(Ordering::SeqCst) {
                    loop_stop.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    {
        let media = ViewerMedia {
            platform: ctx.platform.clone(),
            surface: surface.clone(),
            codec: ctx.codec,
            shared: shared.clone(),
            slot,
        };
        let (ctl, stop_r) = (ctl_tx.clone(), loop_stop.clone());
        let media_thread = spawn(&format!("popout-{slot}-media"), move || {
            if let Err(e) = viewer_media_loop(media, video_rx, ctl, stop_r) {
                tracing::warn!("pop-out {slot} media loop ended: {e}");
                ui.send(SessionEvent::Toast(format!(
                    "Cannot display that monitor: {e}"
                )));
            }
        });
        let target = InputTarget {
            slot,
            current: main.input_target.clone(),
        };
        let (ctl, keys, stop_i) = (ctl_tx.clone(), ctx.keys.clone(), loop_stop.clone());
        let input_thread = spawn(&format!("popout-{slot}-input"), move || {
            input_loop(input_rx, ctl, keys, target, stop_i)
        });
        for t in [watcher, media_thread, input_thread] {
            match t {
                Ok(h) => threads.push(h),
                Err(e) => {
                    tracing::error!("could not start a pop-out thread: {e}");
                    loop_stop.store(true, Ordering::SeqCst);
                    for t in threads {
                        let _ = t.join();
                    }
                    return None;
                }
            }
        }
    }
    surface.set_input_sink(Some(input_tx));
    surface.set_cursor_control(control);
    let popout = ViewerPopout {
        gen: main.popout_gen.fetch_add(1, Ordering::SeqCst) + 1,
        shared,
        video_tx,
        surface,
        stop: loop_stop,
        threads,
    };
    let gen = popout.gen;
    // Register under the registry lock, and only into the session we started
    // from: ending a session takes it out under this lock and its reaper then
    // stops every registered pop-out, so nothing registered here can be
    // missed — and a session that ended meanwhile gets nothing registered.
    let mut pending = Some(popout);
    engine.with_viewer(id, |s| {
        let live = Arc::ptr_eq(&s.shared, &main) && !s.stop.load(Ordering::SeqCst);
        let mut popouts = main.popouts.lock();
        if live && !popouts.contains_key(&slot) {
            if let Some(p) = pending.take() {
                popouts.insert(slot, p);
                *main.input_target.lock() = NO_INPUT_TARGET;
            }
        }
    });
    if let Some(p) = pending {
        // Not registered (the session ended, or the slot was taken): its
        // threads must not outlive this call.
        stop_viewer_popout(p);
        return None;
    }
    // The host forced an IDR when it started this stream, probably before
    // this window existed to decode it.
    let _ = ctl_tx.send(serialize(&ControlMsg::KeyframeRequest { s: slot }));
    Some(gen)
}

/// Viewer (D14): close pop-out stream `slot`'s threads and take its surface
/// down (blocking until its threads are gone, so the app may then destroy the
/// window). With `tell_host`, also asks the host to stop streaming it (the
/// user closed the window; when the host dropped it, it already knows).
///
/// `gen` names the attachment ([`open_viewer_popout`]'s result): only that one
/// is taken down, so a late close of an old window can never take down a
/// newer window that reuses the slot. `None` = the window never attached;
/// nothing is taken down (the host is still told, with `tell_host`).
pub(crate) fn close_viewer_popout(
    engine: &Engine,
    id: SessionId,
    slot: u8,
    gen: Option<u64>,
    tell_host: bool,
) {
    let found = engine
        .with_viewer(id, |session| {
            let mut popouts = session.shared.popouts.lock();
            let mine = gen.is_some() && popouts.get(&slot).map(|p| p.gen) == gen;
            if gen.is_some() && !mine {
                // Already gone (session teardown, or closed twice): the host
                // was told then, if it needed telling.
                return None;
            }
            let popout = if mine { popouts.remove(&slot) } else { None };
            drop(popouts);
            *session.shared.input_target.lock() = NO_INPUT_TARGET;
            Some((popout, session.ctl_tx.clone()))
        })
        .flatten();
    let Some((popout, ctl_tx)) = found else {
        return;
    };
    if let Some(p) = popout {
        stop_viewer_popout(p);
    }
    // After the window's threads are gone, so none of its late messages for
    // this slot (`vsize`, `kf`) can follow and reach a stream the host has
    // meanwhile given the slot to.
    if tell_host {
        let _ = ctl_tx.send(serialize(&ControlMsg::PopoutClose { s: slot }));
    }
}

fn stop_viewer_popout(p: ViewerPopout) {
    p.surface.set_input_sink(None);
    p.surface.set_cursor_control(false);
    p.stop.store(true, Ordering::SeqCst);
    for t in p.threads {
        let _ = t.join();
    }
    p.surface.hide();
}

/// Host-side rate observation + bandwidth injection for the loopback test. Never
/// compiled into a shipping build.
#[cfg(feature = "test-hooks")]
impl Session {
    /// The encoder's current CBR target (bps), or `None` on a viewer session.
    pub(crate) fn encoder_target_bps(&self) -> Option<u32> {
        self.hooks
            .as_ref()
            .map(|h| h.bitrate.load(Ordering::SeqCst))
    }
    /// The raw bandwidth estimate (bps), or `None` on a viewer session.
    pub(crate) fn raw_link_bps(&self) -> Option<u32> {
        self.hooks.as_ref().map(|h| h.link.load(Ordering::SeqCst))
    }
    /// The host's current encode size `(w, h)`, or `None` on a viewer session.
    pub(crate) fn encode_dims(&self) -> Option<(u32, u32)> {
        self.hooks.as_ref().map(|h| {
            let v = h.shared.encode_dims.load(Ordering::SeqCst);
            ((v >> 32) as u32, v as u32)
        })
    }
    /// Force the bandwidth estimate to `bps` (0 = release to the real estimate).
    pub(crate) fn inject_bwe(&self, bps: u32) {
        if let Some(h) = &self.hooks {
            h.shared.bwe_inject.store(bps, Ordering::SeqCst);
        }
    }

    /// Make the transport declare the link dead on its next pass.
    pub(crate) fn sever(&self) {
        self.shared.sever.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_target() -> InputTarget {
        InputTarget {
            slot: 0,
            current: Arc::default(),
        }
    }

    /// D14: a pop-out window's pointer input is preceded by ONE switch of the
    /// host's input target to its stream; keys never are.
    #[test]
    fn popout_pointer_input_switches_the_target_once_and_keys_do_not() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        let current: Arc<Mutex<u8>> = Arc::default();
        tx.send(VideoInput::Key {
            code: "KeyA",
            down: true,
        })
        .unwrap();
        tx.send(VideoInput::Button {
            button: 0,
            down: true,
            nx: 0.5,
            ny: 0.5,
        })
        .unwrap();
        tx.send(VideoInput::Wheel { dx: 0, dy: 120 }).unwrap();
        drop(tx);
        let target = InputTarget {
            slot: 2,
            current: current.clone(),
        };
        input_loop(
            rx,
            ctl_tx,
            identity_keys(),
            target,
            Arc::new(AtomicBool::new(false)),
        );
        let kinds: Vec<String> = ctl_rx
            .try_iter()
            .map(|b| {
                let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
                v["t"].as_str().unwrap_or_default().to_string()
                    + &v["s"].as_u64().map(|s| s.to_string()).unwrap_or_default()
            })
            .collect();
        assert_eq!(kinds, ["kd", "itgt2", "md", "wh"], "{kinds:?}");
        assert_eq!(*current.lock(), 2);
    }

    fn identity_keys() -> Arc<Mutex<KeyTranslator>> {
        Arc::new(Mutex::new(KeyTranslator::new(
            OsFamily::Windows,
            OsFamily::Windows,
            Arc::new(AtomicBool::new(true)),
        )))
    }

    fn sent(ctl_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<InputMsg> {
        ctl_rx
            .try_iter()
            .map(|b| serde_json::from_slice(&b).unwrap())
            .collect()
    }

    #[test]
    fn input_loop_sends_keys_through_the_translator() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        for (code, down) in [("MetaLeft", true), ("KeyC", true), ("KeyC", false)] {
            tx.send(VideoInput::Key { code, down }).unwrap();
        }
        drop(tx);
        let keys = Arc::new(Mutex::new(KeyTranslator::new(
            OsFamily::Mac,
            OsFamily::Windows,
            Arc::new(AtomicBool::new(true)),
        )));
        input_loop(
            rx,
            ctl_tx,
            keys,
            main_target(),
            Arc::new(AtomicBool::new(false)),
        );
        let codes: Vec<(String, bool)> = sent(&ctl_rx)
            .into_iter()
            .map(|m| match m {
                InputMsg::KeyDown { code } => (code, true),
                InputMsg::KeyUp { code } => (code, false),
                other => panic!("not a key: {other:?}"),
            })
            .collect();
        assert_eq!(
            codes,
            [
                ("ControlLeft".to_string(), true),
                ("KeyC".to_string(), true),
                ("KeyC".to_string(), false)
            ]
        );
    }

    fn key_codes(ctl_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<(String, bool)> {
        sent(ctl_rx)
            .into_iter()
            .map(|m| match m {
                InputMsg::KeyDown { code } => (code, true),
                InputMsg::KeyUp { code } => (code, false),
                other => panic!("not a key: {other:?}"),
            })
            .collect()
    }

    fn mac_to_windows_keys() -> Arc<Mutex<KeyTranslator>> {
        Arc::new(Mutex::new(KeyTranslator::new(
            OsFamily::Mac,
            OsFamily::Windows,
            Arc::new(AtomicBool::new(true)),
        )))
    }

    /// M12: Cmd travels the surface's path, Cmd+C's C the hook's. Queued in
    /// that order, the translator must see them in that order even though
    /// the input thread has not run yet when the hook forwards C.
    #[test]
    fn hook_keys_queue_behind_the_surface_keys_sent_before_them() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        let keys = mac_to_windows_keys();
        let forward = hook_forwarder(tx.clone(), keys.clone(), ctl_tx.clone());
        tx.send(VideoInput::Key {
            code: "MetaLeft",
            down: true,
        })
        .unwrap();
        forward("KeyC", true);
        forward("KeyC", false);
        tx.send(VideoInput::Key {
            code: "MetaLeft",
            down: false,
        })
        .unwrap();
        // Nothing reached the host yet: the hook did not jump the queue.
        assert!(key_codes(&ctl_rx).is_empty());
        drop((tx, forward));
        input_loop(
            rx,
            ctl_tx,
            keys,
            main_target(),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            key_codes(&ctl_rx),
            [
                ("ControlLeft".to_string(), true),
                ("KeyC".to_string(), true),
                ("KeyC".to_string(), false),
                ("ControlLeft".to_string(), false)
            ]
        );
    }

    #[test]
    fn hook_keys_still_reach_the_host_once_the_input_thread_is_gone() {
        let (tx, rx) = std::sync::mpsc::channel::<VideoInput>();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        drop(rx);
        let forward = hook_forwarder(tx, mac_to_windows_keys(), ctl_tx);
        forward("MetaLeft", false);
        // A code outside the shared keymap goes straight to the translator.
        forward("NotAKey", true);
        assert_eq!(
            key_codes(&ctl_rx),
            [
                ("ControlLeft".to_string(), false),
                ("NotAKey".to_string(), true)
            ]
        );
    }

    #[test]
    fn keys_queued_when_the_session_stops_are_still_sent() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        tx.send(VideoInput::Move { nx: 0.5, ny: 0.5 }).unwrap();
        tx.send(VideoInput::Key {
            code: "KeyA",
            down: false,
        })
        .unwrap();
        // Already stopped: the loop body never runs, only the drain.
        input_loop(
            rx,
            ctl_tx,
            identity_keys(),
            main_target(),
            Arc::new(AtomicBool::new(true)),
        );
        assert_eq!(key_codes(&ctl_rx), [("KeyA".to_string(), false)]);
    }

    #[test]
    fn translate_input_maps_every_variant() {
        assert!(translate_input(VideoInput::Key {
            code: "KeyA",
            down: true
        })
        .is_none());
        assert!(matches!(
            translate_input(VideoInput::Button {
                button: 2,
                down: false,
                nx: 0.5,
                ny: 0.25
            }),
            Some(InputMsg::ButtonUp {
                b: protocol::Button::Right,
                ..
            })
        ));
        assert!(translate_input(VideoInput::Button {
            button: 7,
            down: true,
            nx: 0.0,
            ny: 0.0
        })
        .is_none());
        assert!(matches!(
            translate_input(VideoInput::Wheel { dx: 0, dy: -120 }),
            Some(InputMsg::Wheel { dx: 0, dy: -120 })
        ));
    }

    #[test]
    fn input_loop_coalesces_moves_but_keeps_clicks_in_order() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        for i in 0..50 {
            tx.send(VideoInput::Move {
                nx: i as f64 / 100.0,
                ny: 0.0,
            })
            .unwrap();
        }
        tx.send(VideoInput::Button {
            button: 0,
            down: true,
            nx: 0.49,
            ny: 0.0,
        })
        .unwrap();
        drop(tx);
        input_loop(rx, ctl_tx, identity_keys(), main_target(), stop);
        let msgs: Vec<InputMsg> = ctl_rx
            .try_iter()
            .map(|b| serde_json::from_slice(&b).unwrap())
            .collect();
        // The 50 moves collapse into very few, and the newest position wins.
        let moves: Vec<f64> = msgs
            .iter()
            .filter_map(|m| match m {
                InputMsg::Move { x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        assert!(moves.len() <= 2, "{moves:?}");
        assert_eq!(moves.last().copied(), Some(0.49));
        assert!(matches!(msgs.last(), Some(InputMsg::ButtonDown { .. })));
    }
}
