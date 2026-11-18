//! Activation and minimise of a top-level app window (plans/09 W3, 09a §3.3):
//! GTK's `is-active` (the toplevel has the keyboard; the video sink is inside
//! it, so a click into the video keeps it active) and `window-state-event`
//! (`ICONIFIED`). On Wayland nothing reports minimise (xdg-shell has no such
//! state, 09 §7), so `minimized` is `None` there and only activation follows.
//!
//! Everything runs on the GTK main thread: the handlers are connected there
//! and called there, and the connections are dropped there ([`MainBound`]).
//!
//! ## References
//! * `GtkWindow:is-active`, `GtkWidget::window-state-event`,
//!   `GdkWindowState::ICONIFIED`: <https://docs.gtk.org/gtk3/property.Window.is-active.html>,
//!   <https://docs.gtk.org/gtk3/signal.Widget.window-state-event.html>,
//!   <https://docs.gtk.org/gdk3/flags.WindowState.html>
//! * xdg-shell has no minimised state (only `xdg_toplevel.set_minimized`, a
//!   request): <https://wayland.app/protocols/xdg-shell>

use std::cell::Cell;
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::glib::SignalHandlerId;
use gtk::prelude::*;
use platform_api::{RawWindowHandle, SurfaceError, WindowActivity, WindowActivityFn, WindowWatch};

use super::gtk_host::{self, MainBound};

/// The handlers on one toplevel; disconnected when dropped (on the main
/// thread, by [`MainBound`]).
struct Connected {
    window: gtk::Window,
    handlers: Vec<SignalHandlerId>,
}

impl Drop for Connected {
    fn drop(&mut self) {
        for id in self.handlers.drain(..) {
            self.window.disconnect(id);
        }
    }
}

pub(crate) struct ActivationWatch {
    _connected: MainBound<Connected>,
}

impl WindowWatch for ActivationWatch {}

/// Watch the toplevel behind `window`. Call on the GTK main thread.
pub(crate) fn watch(
    window: RawWindowHandle,
    on_change: WindowActivityFn,
) -> Result<Box<dyn WindowWatch>, SurfaceError> {
    let (top, wayland) = match window {
        RawWindowHandle::Xlib(w) => {
            // X11 window ids are 32-bit (`c_ulong` only carries them).
            let xid = u32::try_from(w.window).map_err(|_| {
                SurfaceError::Backend(format!("X11 window id {:#x} is out of range", w.window))
            })?;
            (toplevel_with_xid(u64::from(xid))?, false)
        }
        RawWindowHandle::Xcb(w) => (toplevel_with_xid(u64::from(w.window.get()))?, false),
        RawWindowHandle::Wayland(w) => (
            gtk_host::find_toplevel(|g| super::wayland::wl_surface_of(g) == Some(w.surface))?,
            true,
        ),
        _ => return Err(SurfaceError::UnsupportedHandle),
    };
    let window = top.window;
    let on_change: Rc<dyn Fn(WindowActivity) + Send + Sync> = Rc::from(on_change);
    let last: Rc<Cell<Option<WindowActivity>>> = Rc::default();
    // `iconified`: what a window-state event being handled says (GDK's own
    // state may not show it yet while the event is delivered).
    let report = Rc::new(move |w: &gtk::Window, iconified: Option<bool>| {
        let minimized = if wayland {
            None
        } else {
            Some(iconified.unwrap_or_else(|| {
                w.window()
                    .is_some_and(|g| g.state().contains(gdk::WindowState::ICONIFIED))
            }))
        };
        let now = WindowActivity {
            active: w.is_active() && minimized != Some(true),
            minimized,
        };
        if last.get() != Some(now) {
            last.set(Some(now));
            on_change(now);
        }
    });
    let mut handlers = Vec::new();
    {
        let report = report.clone();
        handlers.push(window.connect_is_active_notify(move |w| report(w, None)));
    }
    if !wayland {
        let report = report.clone();
        handlers.push(window.connect_window_state_event(move |w, ev| {
            let iconified = ev.new_window_state().contains(gdk::WindowState::ICONIFIED);
            report(w, Some(iconified));
            glib::Propagation::Proceed
        }));
    }
    report(&window, None);
    Ok(Box::new(ActivationWatch {
        _connected: MainBound::new(Connected { window, handlers }),
    }))
}

fn toplevel_with_xid(xid: u64) -> Result<gtk_host::Toplevel, SurfaceError> {
    gtk_host::find_toplevel(|g| {
        g.downcast_ref::<gdkx11::X11Window>()
            .is_some_and(|x| x.xid() == xid)
    })
}
