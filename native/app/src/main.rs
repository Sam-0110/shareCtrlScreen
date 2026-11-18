//! Tauri shell for ShareCtrlScreen. The Rust engine owns config, signaling,
//! WebRTC and the session; the OS backend (`platform-windows`/`-macos`/`-linux`)
//! owns capture, codecs, the video surface, input, clipboard and secrets. This
//! binary is the thin bridge:
//!   * `#[tauri::command]`s replace each Electron `ipcMain.handle` (config,
//!     connect, permission, password, approval),
//!   * engine `UiEvent`s are forwarded to the WebView2 UI via Tauri events,
//!     replacing `ipcRenderer.on(...)`,
//!   * the video is a native child surface the platform creates under this
//!     window; the web UI frames it (the toolbar slides over it on hover).
//!
//! The old renderer's WebRTC/signaling JS is obsolete here — that logic moved to
//! the Rust engine. `withGlobalTauri` lets the ported vanilla UI call
//! `window.__TAURI__.core.invoke` with no bundler.

// On Windows release builds, don't spawn a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod accounts;
mod bridge;
mod commands;
mod menu;
mod popout;
mod state;
mod surfaces;
mod tabs;
#[cfg(windows)]
mod tray;
mod uiaccess;
mod updater;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use codec_openh264::OpenH264;
use engine::handshake::Decider;
use engine::Engine;
use platform_api::{Platform, AUTOSTART_FLAG};
use tauri::Manager;

// Re-exported at the crate root so the modules keep reaching them as
// `crate::AppState` / `crate::display_label`.
use commands::display_label;
use state::{AppDecider, AppState, H264Problem};

/// The running app's log file.
const APP_LOG: &str = "app.log";
/// Where a launch that hands over to the running host logs why (Windows, W4).
/// Not `app.log`: opening that would truncate the running host's log.
#[cfg(windows)]
const HANDOVER_LOG: &str = "app-handover.log";

/// Log to `file_name` in the platform's log directory, since release builds
/// have no console. `RUST_LOG` still overrides the level.
fn init_logging(log_dir: &Path, file_name: &str) {
    let path = log_dir.join(file_name);
    let _ = std::fs::create_dir_all(log_dir);
    // Verbose for our own media/transport crates so a black screen leaves a
    // trail (encode/decode errors, ICE state); quiet for noisy dependencies.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "info,engine=debug,transport=debug,signaling=debug,platform_windows=debug,\
             platform_macos=debug,platform_linux=debug,codec=debug,codec_openh264=debug,\
             capture=debug,str0m=warn",
        )
    });
    match std::fs::File::create(&path) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .with_env_filter(filter)
                .init();
            tracing::info!("log file: {}", path.display());
        }
        Err(_) => {
            // Fall back to stdout (debug build with a console).
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}

/// The OS backend for this build, and its Cisco OpenH264 handle (the one
/// setting that reaches into the codec layer, so it is kept before the
/// platform is type-erased).
fn make_platform() -> (Arc<dyn Platform>, Arc<OpenH264>, H264Problem) {
    #[cfg(windows)]
    let platform = Arc::new(platform_windows::WindowsPlatform::new());
    #[cfg(target_os = "macos")]
    let platform = Arc::new(platform_macos::MacPlatform::new());
    #[cfg(target_os = "linux")]
    let platform = Arc::new(platform_linux::LinuxPlatform::new());
    let openh264 = platform.openh264();
    // Only the Windows backend can tell yet whether the OS has its own H.264;
    // elsewhere no claim is made (`None`).
    #[cfg(windows)]
    let problem: H264Problem = {
        let p = platform.clone();
        Arc::new(move || p.h264_problem())
    };
    #[cfg(not(windows))]
    let problem: H264Problem = Arc::new(|| None);
    let platform: Arc<dyn Platform> = platform;
    (platform, openh264, problem)
}

/// Whether closing the main window hides it to the tray (the Windows service
/// host) instead of quitting.
fn hides_to_tray() -> bool {
    #[cfg(windows)]
    {
        tray::is_installed()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Bring the main window up: for a launch that handed over to this host, and
/// from the tray.
#[cfg(windows)]
fn raise_main_window(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    for (what, result) in [
        ("unminimize", win.unminimize()),
        ("show", win.show()),
        ("focus", win.set_focus()),
    ] {
        if let Err(e) = result {
            tracing::warn!("raising the window: {what} failed: {e}");
        }
    }
}

fn main() {
    // `--print-uiaccess` answers and exits before anything else exists: no
    // window, no network, no logging (which truncates app.log) and no instance
    // gate — it reports the status of THIS invocation and nothing more (W5).
    if uiaccess::wants_print(std::env::args().skip(1)) {
        let status = uiaccess::probe();
        // A release build is a GUI-subsystem exe: stdout reaches the caller
        // only when it is redirected or piped; a write error is not fatal.
        let _ = writeln!(std::io::stdout(), "{}", status.line());
        std::process::exit(status.exit_code());
    }

    // One host per session when the Windows service is installed (task W4).
    // Decided before logging, so a launch that hands over never opens (and
    // truncates) the running host's app.log.
    #[cfg(windows)]
    let (mut service_host, gate_note) = match platform_windows::instance::gate(std::env::args()) {
        platform_windows::instance::Gate::Exit { reason } => {
            let (platform, _, _) = make_platform();
            init_logging(&platform.paths().log_dir, HANDOVER_LOG);
            tracing::info!("{reason}");
            return;
        }
        platform_windows::instance::Gate::Run { marker, note } => (marker, note),
    };
    // The service host lives in the tray: closing its window hides it
    // (`tray.rs`). Read now: `service_host` is taken in `setup`.
    #[cfg(windows)]
    let lives_in_tray = service_host.is_some();

    let (platform, openh264, h264_problem) = make_platform();
    init_logging(&platform.paths().log_dir, APP_LOG);
    tracing::info!("platform: {}", platform.info().name);
    #[cfg(windows)]
    {
        if let Some(note) = gate_note {
            tracing::warn!("{note}");
        }
        if service_host.is_some() {
            tracing::info!("started by the ShareCtrlScreen service as this session's host");
        }
        // W5: whether remote Alt+Tab / the Windows key can reach this host's
        // shell. UIAccess is a Windows token flag, so other OSes log nothing.
        tracing::info!("{}", uiaccess::probe().line());
    }

    // The engine spawns its signaling task with `tokio::spawn`, so a Tokio
    // runtime must exist *and be entered* before `Engine::start`. Hand the same
    // runtime to Tauri so `tauri::async_runtime::spawn` shares it rather than
    // standing up a second one.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    tauri::async_runtime::set(runtime.handle().clone());
    let _runtime_guard = runtime.enter();

    let decider = Arc::new(AppDecider::default());
    let (engine, ui_rx, mut sig_rx) =
        Engine::start(platform.clone(), decider.clone() as Arc<dyn Decider>);

    // Keep the autostart entry honest. It records an absolute path, which goes
    // stale whenever the app is updated, reinstalled or run from a different
    // build — and a stale entry fails silently at sign-in.
    platform
        .autostart()
        .reconcile(engine.config().start_with_windows);
    // The saved choice, before any session can create a codec.
    openh264.set_enabled(engine.config().use_openh264);

    let account_manager = accounts::AccountManager::new(engine.clone());
    let state = AppState {
        engine: engine.clone(),
        accounts: account_manager.clone(),
        decider,
        openh264,
        h264_problem,
    };

    tauri::Builder::default()
        // In-app updater (task U1): the updater checks the GitHub "latest" feed
        // and verifies each download against `plugins.updater.pubkey`; the process
        // plugin backs the webview's relaunch (the restart itself is Rust's core
        // `AppHandle::restart`, see `updater.rs`).
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(state)
        .manage(popout::PopoutWindows::default())
        // A pop-out window (D14) is closed only once the engine has let go of
        // its video surface; see `popout::on_close_requested`.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let engine = app.state::<AppState>().engine.clone();
                if popout::on_close_requested(app, &engine, window.label()) {
                    api.prevent_close();
                } else if window.label() == "main" && !hides_to_tray() {
                    // Closing the app: every tab disconnects first (D6, I9) —
                    // held keys released, Byes sent, surfaces gone — then it
                    // exits, waiting at most 3 s (08a §6.17).
                    api.prevent_close();
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        end_sessions_for_exit(&app).await;
                        app.exit(0);
                    });
                }
            }
        })
        // Native menu clicks (macOS + Linux), and the Windows service host's
        // tray menu.
        .on_menu_event(menu::handle_menu_event)
        // Windows service host: closing the window hides it to the tray (a
        // no-op until the tray icon exists, so other instances still quit).
        .on_window_event(|window, event| {
            #[cfg(windows)]
            tray::on_window_event(window, event);
            #[cfg(not(windows))]
            let _ = (window, event);
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::get_platform_info,
            commands::clear_recents,
            commands::set_remote_name,
            accounts::account_sign_in,
            accounts::account_sign_up,
            accounts::account_resolve_local,
            accounts::account_switch,
            accounts::account_remove,
            accounts::account_window_focused,
            accounts::account_sync,
            accounts::account_sign_out,
            commands::get_server_status,
            commands::get_role,
            commands::tab_open,
            commands::tab_connect,
            commands::tab_disconnect,
            commands::tab_close,
            commands::tab_activate,
            commands::tab_next,
            commands::tab_prev,
            commands::tab_rename,
            commands::tab_move,
            commands::tab_connect_all,
            commands::tab_password,
            commands::tab_forget_password,
            commands::get_snapshot,
            commands::set_permission,
            commands::end_session,
            commands::request_refresh,
            commands::select_display,
            commands::popout_display,
            commands::set_video_visible,
            commands::approve,
            commands::save_settings,
            commands::get_permissions,
            commands::request_permission,
            commands::openh264_info,
            commands::openh264_download,
            updater::updater_check,
            updater::updater_install,
            updater::open_releases_page,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Native menu (macOS + Linux only; Windows keeps its web drawer).
            // Not fatal: the app is still fully usable without the menu.
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if let Err(e) = menu::install_native_menu(app) {
                tracing::warn!("native menu: {e}");
            }

            // The service host keeps running when its window is closed, so
            // it needs a way back and a way out: the tray icon. Not fatal:
            // without it the window closes the app, as before.
            #[cfg(windows)]
            if lives_in_tray {
                if let Err(e) = tray::install(app) {
                    tracing::warn!("tray icon: {e}; closing the window will quit");
                }
            }

            // Another launch in this session asks the service host to show
            // its window instead of starting a second host.
            #[cfg(windows)]
            if let Some(marker) = service_host.take() {
                let raise_handle = handle.clone();
                marker.listen(move || raise_main_window(&raise_handle));
            }

            // Bridge engine UiEvents → WebView2 events (replaces ipcRenderer.on).
            // Accounts: finish signing the selected one in, then keep it synced.
            account_manager.start(handle.clone());

            // The tabs (plans/08 P4): every viewer session belongs to one.
            let tabs = tabs::TabManager::new(handle.clone(), engine.clone());
            app.manage(tabs.clone());
            bridge::spawn(handle, engine.clone(), tabs, ui_rx);

            // Drive the signaling event loop into the engine. The loop only
            // dispatches: each peer's messages run on that peer's own task, so a
            // slow step for one (an approval prompt, a keyring unlock) never
            // stalls the others or the server bookkeeping.
            let sig_engine = engine.clone();
            tauri::async_runtime::spawn(async move {
                while let Some(event) = sig_rx.recv().await {
                    sig_engine.dispatch_signaling(event).await;
                }
            });

            if let Some(win) = app.get_webview_window("main") {
                // Started at sign-in: come up minimized rather than throwing a
                // window in the user's face on every boot. The app is useful
                // purely by being reachable, so it does not need focus.
                if std::env::args().any(|a| a == AUTOSTART_FLAG) {
                    let _ = win.minimize();
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building ShareCtrlScreen")
        .run(on_run_event);
}

/// Set once the sessions were ended for an exit, so that exit goes ahead.
static SESSIONS_ENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// An exit from anywhere (macOS ⌘Q, the last window closing, the OS) ends
/// every session first — held keys released, Byes sent — waiting at most 3 s
/// (08a §6.17, I9), then exits.
fn on_run_event(app: &tauri::AppHandle, event: tauri::RunEvent) {
    use std::sync::atomic::Ordering;
    let tauri::RunEvent::ExitRequested { api, code, .. } = event else {
        return;
    };
    // A restart (the updater) cannot be held, and the updater ended the
    // sessions itself.
    if code == Some(tauri::RESTART_EXIT_CODE) {
        return;
    }
    let engine = app.state::<AppState>().engine.clone();
    // Nothing to end (the updater and the tray's Quit end the sessions
    // themselves first; a restart must then go ahead untouched).
    if SESSIONS_ENDED.load(Ordering::SeqCst)
        || (engine.viewer_sessions().is_empty() && engine.host_session().is_none())
    {
        return;
    }
    api.prevent_exit();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        end_sessions_for_exit(&app).await;
        app.exit(code.unwrap_or(0));
    });
}

/// Before any exit: no tab starts a connection any more (Connect all
/// included), then every session ends and is released — held keys let go,
/// Byes sent — waiting at most 3 s (08a §6.17, I9). Marks the sessions
/// ended, so the exit that follows goes ahead.
pub(crate) async fn end_sessions_for_exit(app: &tauri::AppHandle) {
    if let Some(tabs) = app.try_state::<Arc<tabs::TabManager>>() {
        tabs.begin_exit();
    }
    let engine = app.state::<AppState>().engine.clone();
    engine
        .end_all_and_wait(std::time::Duration::from_secs(3))
        .await;
    SESSIONS_ENDED.store(true, std::sync::atomic::Ordering::SeqCst);
}
