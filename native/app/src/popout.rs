//! Pop-out windows (task D14): while controlling, the viewer can show more of
//! the host's monitors, each in its own window, alongside the main one.
//!
//! The engine owns the streams; this module owns the windows. The host lists
//! the monitors it streams to pop-outs in every `displays` announcement
//! ([`engine::UiEvent::Displays`]), and [`reconcile`] makes the set of open
//! windows match it: a new entry gets a window (built here, its native video
//! surface created on the UI thread, then handed to
//! [`engine::Engine::attach_popout`]); a window whose entry is gone is closed.
//! The user closing a window ([`on_close_requested`]) tells the host to stop
//! that stream.
//!
//! Rules that keep the native surface from ever outliving its window, and a
//! closing window from ever disturbing a newer one:
//! * Every close takes the engine's side down FIRST (blocking until the
//!   window's decode/present thread is gone) and only then destroys the
//!   window. That blocking part runs on its own thread: a presenter may itself
//!   wait on the UI thread (macOS).
//! * A close names the attachment generation the engine handed out, so a late
//!   close of an old window cannot detach a newer one that reuses its slot.
//! * Window labels are unique per window (slot + counter): a slot reopened
//!   while its old window is still closing gets a new window, and attaches
//!   once the old one has let go of the slot.
//! * Attaching happens under this module's lock and only while the window is
//!   still wanted, so a window closed before it attached is never attached.
//!
//! The window loads `popout.html`, a static page with no script, and is not
//! in any capability: it cannot call into the app.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine::{Engine, SessionId};
use protocol::{PopoutStream, RemoteDisplay};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// Window label prefix; then `<slot>-<serial>`.
const LABEL_PREFIX: &str = "popout-";

/// Most pop-out windows the viewer opens, whatever a host announces (the
/// engine's own limit: one spare stream each for slots 1..=3).
const MAX_WINDOWS: usize = 3;

/// How long a new window waits for an old window on the same slot to finish
/// closing before giving up.
const ATTACH_WAIT: Duration = Duration::from_secs(3);

/// One pop-out window.
struct Entry {
    /// The viewer session it belongs to.
    session: SessionId,
    /// The monitor id it shows.
    id: String,
    label: String,
    /// The engine attachment, once attached.
    gen: Option<u64>,
}

/// The wanted pop-out windows, by stream slot.
#[derive(Default)]
pub struct PopoutWindows {
    open: Mutex<HashMap<u8, Entry>>,
    serial: AtomicU64,
}

impl PopoutWindows {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u8, Entry>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn is_popout_label(label: &str) -> bool {
    label.starts_with(LABEL_PREFIX)
}

/// Initial window size (logical pixels) for a monitor: 1280 wide at its
/// aspect ratio, kept within a sensible range.
fn initial_size(d: Option<&RemoteDisplay>) -> (f64, f64) {
    let Some(d) = d.filter(|d| d.width > 0 && d.height > 0) else {
        return (1280.0, 720.0);
    };
    let aspect = f64::from(d.height) / f64::from(d.width);
    let mut w = 1280.0;
    let mut h = w * aspect;
    if h > 900.0 {
        h = 900.0;
        w = h / aspect;
    }
    (w.max(320.0), h.max(240.0))
}

/// The announced pop-outs this viewer will open: slots 1..=3 only, at most
/// [`MAX_WINDOWS`], one per slot (a host cannot make it open any number).
fn wanted(popouts: &[PopoutStream]) -> Vec<PopoutStream> {
    let mut out: Vec<PopoutStream> = Vec::new();
    for p in popouts {
        if (1..=MAX_WINDOWS as u8).contains(&p.s) && !out.iter().any(|o| o.s == p.s) {
            out.push(p.clone());
        }
    }
    out.truncate(MAX_WINDOWS);
    out
}

/// Make the open pop-out windows match the host's latest announcement
/// for viewer session `session` (the caller checks it is a viewer session).
pub fn reconcile(
    app: &AppHandle,
    engine: &Arc<Engine>,
    session: SessionId,
    list: &[RemoteDisplay],
    popouts: &[PopoutStream],
) {
    let Some(state) = app.try_state::<PopoutWindows>() else {
        return;
    };
    let wanted = wanted(popouts);
    // Decide under the lock, act after it: building or destroying a window
    // waits on the UI thread, which may itself want this lock.
    let (stale, new) = {
        let mut open = state.lock();
        let stale_slots: Vec<u8> = open
            .iter()
            .filter(|(s, e)| !wanted.iter().any(|p| p.s == **s && p.id == e.id))
            .map(|(s, _)| *s)
            .collect();
        let stale: Vec<(u8, Entry)> = stale_slots
            .into_iter()
            .filter_map(|s| open.remove(&s).map(|e| (s, e)))
            .collect();
        let mut new = Vec::new();
        for p in &wanted {
            if open.contains_key(&p.s) {
                continue;
            }
            let serial = state.serial.fetch_add(1, Ordering::Relaxed);
            let label = format!("{LABEL_PREFIX}{}-{serial}", p.s);
            open.insert(
                p.s,
                Entry {
                    session,
                    id: p.id.clone(),
                    label: label.clone(),
                    gen: None,
                },
            );
            new.push((p.clone(), label));
        }
        (stale, new)
    };
    for (slot, e) in stale {
        // The host already dropped it; nothing to tell it.
        close_window(app, engine, slot, e, false);
    }
    for (p, label) in new {
        open_window(
            app,
            engine,
            session,
            &p,
            label,
            list.iter().find(|d| d.id == p.id),
        );
    }
}

/// Close the pop-out windows of `session`, which was released: the engine
/// has already stopped their threads and let go of their surfaces, so the
/// windows can go (08a §6.5 step 3).
pub fn close_session(app: &AppHandle, engine: &Arc<Engine>, session: SessionId) {
    let Some(state) = app.try_state::<PopoutWindows>() else {
        return;
    };
    let theirs: Vec<(u8, Entry)> = {
        let mut open = state.lock();
        let slots: Vec<u8> = open
            .iter()
            .filter(|(_, e)| e.session == session)
            .map(|(s, _)| *s)
            .collect();
        slots
            .into_iter()
            .filter_map(|s| open.remove(&s).map(|e| (s, e)))
            .collect()
    };
    for (slot, e) in theirs {
        close_window(app, engine, slot, e, false);
    }
}

/// The user asked to close window `label`. For a pop-out window: keep it until
/// the engine has let go of it, then destroy it, and tell the host to stop
/// that stream; returns `true` so the caller prevents the default close.
pub fn on_close_requested(app: &AppHandle, engine: &Arc<Engine>, label: &str) -> bool {
    if !is_popout_label(label) {
        return false;
    }
    let found = app.try_state::<PopoutWindows>().and_then(|state| {
        let mut open = state.lock();
        let slot = open
            .iter()
            .find(|(_, e)| e.label == label)
            .map(|(s, _)| *s)?;
        open.remove(&slot).map(|e| (slot, e))
    });
    // Not found: it is already closing; its pending close destroys it.
    if let Some((slot, e)) = found {
        close_window(app, engine, slot, e, true);
    }
    true
}

fn open_window(
    app: &AppHandle,
    engine: &Arc<Engine>,
    session: SessionId,
    p: &PopoutStream,
    label: String,
    display: Option<&RemoteDisplay>,
) {
    let slot = p.s;
    let name = display
        .map(crate::display_label)
        .unwrap_or_else(|| p.id.clone());
    let (w, h) = initial_size(display);
    let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("popout.html".into()))
        .title(format!("{name} \u{2014} ShareCtrlScreen"))
        .inner_size(w, h)
        .min_inner_size(320.0, 240.0)
        .build();
    let win = match built {
        Ok(win) => win,
        Err(e) => {
            tracing::warn!("pop-out window {slot}: {e}");
            if let Some(state) = app.try_state::<PopoutWindows>() {
                let mut open = state.lock();
                if open.get(&slot).is_some_and(|e| e.label == label) {
                    open.remove(&slot);
                }
            }
            // Stop the stream nobody can see.
            let engine = engine.clone();
            std::thread::spawn(move || engine.close_popout(session, slot, None));
            return;
        }
    };
    // Wait (off the UI thread) for an old window on this slot to let go, then
    // create the native surface on the UI thread, as for the main window.
    let (engine, app) = (engine.clone(), app.clone());
    std::thread::spawn(move || {
        let deadline = Instant::now() + ATTACH_WAIT;
        while engine.popout_attached(session, slot) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let app2 = app.clone();
        let queued =
            app.run_on_main_thread(move || attach(&app2, &engine, &win, session, slot, &label));
        if let Err(e) = queued {
            tracing::warn!("pop-out window {slot}: {e}");
        }
    });
}

/// On the UI thread: if window `label` is still wanted for `slot`, create its
/// native video surface and give it to the engine. Otherwise, or on failure,
/// the window goes away (and on failure the host stops the stream).
fn attach(
    app: &AppHandle,
    engine: &Arc<Engine>,
    win: &tauri::WebviewWindow,
    session: SessionId,
    slot: u8,
    label: &str,
) {
    let Some(state) = app.try_state::<PopoutWindows>() else {
        return;
    };
    // Held while attaching: a close of this window (which also takes the
    // lock) then sees either "not attached yet, so not attached at all" or
    // the generation to detach.
    let mut open = state.lock();
    let Some(entry) = open.get_mut(&slot).filter(|e| e.label == label) else {
        // Closed (or replaced) before it could attach; its close destroys it.
        let _ = win.destroy();
        return;
    };
    match make_surface(engine, win, slot).and_then(|s| engine.attach_popout(session, slot, s)) {
        Some(gen) => {
            entry.gen = Some(gen);
            tracing::info!("pop-out window {slot} attached");
        }
        None => {
            tracing::warn!("pop-out window {slot}: could not attach; closing it");
            open.remove(&slot);
            let (engine, win) = (engine.clone(), win.clone());
            std::thread::spawn(move || {
                engine.close_popout(session, slot, None);
                let _ = win.destroy();
            });
        }
    }
}

fn make_surface(
    engine: &Engine,
    win: &tauri::WebviewWindow,
    slot: u8,
) -> Option<Arc<dyn platform_api::VideoSurface>> {
    let handles = win
        .window_handle()
        .map_err(|e| e.to_string())
        .and_then(|w| {
            win.display_handle()
                .map(|d| (w.as_raw(), d.as_raw()))
                .map_err(|e| e.to_string())
        });
    match handles {
        Ok((window, display)) => match engine.platform().create_surface(window, display) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("pop-out window {slot}: no native video surface: {e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("pop-out window {slot}: no window handle: {e}");
            None
        }
    }
}

/// Take pop-out `slot`'s attachment down in the engine (telling the host when
/// `tell_host`), then destroy its window. Runs on its own thread: see the
/// module docs.
fn close_window(app: &AppHandle, engine: &Arc<Engine>, slot: u8, e: Entry, tell_host: bool) {
    let win = app.get_webview_window(&e.label);
    let engine = engine.clone();
    std::thread::spawn(move || {
        if tell_host {
            engine.close_popout(e.session, slot, e.gen);
        } else {
            engine.detach_popout(e.session, slot, e.gen);
        }
        if let Some(win) = win {
            let _ = win.destroy();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_popout_windows_are_popouts() {
        assert!(is_popout_label("popout-1-0"));
        assert!(!is_popout_label("main"));
    }

    #[test]
    fn a_host_cannot_make_the_viewer_open_any_number_of_windows() {
        let p = |s: u8, id: &str| PopoutStream { s, id: id.into() };
        let many = vec![
            p(0, "main-slot"),
            p(1, "a"),
            p(1, "dup"),
            p(2, "b"),
            p(3, "c"),
            p(4, "d"),
            p(200, "e"),
        ];
        let w = wanted(&many);
        assert_eq!(
            w.iter().map(|p| (p.s, p.id.as_str())).collect::<Vec<_>>(),
            [(1, "a"), (2, "b"), (3, "c")]
        );
    }

    #[test]
    fn initial_size_keeps_the_monitor_aspect() {
        let d = RemoteDisplay {
            id: "a".into(),
            name: "A".into(),
            width: 2560,
            height: 1440,
            primary: false,
        };
        assert_eq!(initial_size(Some(&d)), (1280.0, 720.0));
        let tall = RemoteDisplay {
            width: 1080,
            height: 1920,
            ..d
        };
        let (w, h) = initial_size(Some(&tall));
        assert_eq!(h, 900.0);
        assert!((w - 506.25).abs() < 1e-9);
        assert_eq!(initial_size(None), (1280.0, 720.0));
    }
}
