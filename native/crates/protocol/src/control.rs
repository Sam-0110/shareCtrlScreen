//! Host → Viewer session-control messages (contract §4.2), always on the
//! reliable channel. Also the viewer's graceful `bye`.

use super::config::Permission;
use serde::{Deserialize, Serialize};

/// A control message (contract §4.2). `t` is the type discriminant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ControlMsg {
    /// Host changed the live permission. Sent once when the channel opens
    /// (initial value) and again on every change.
    #[serde(rename = "perm")]
    Perm { value: Permission },
    /// Host is ending the session (or viewer disconnecting gracefully).
    #[serde(rename = "bye")]
    Bye,
    /// Viewer → Host: request a fresh keyframe (decoder loss / just joined).
    /// Not in the original browser contract; the native engine uses it to drive
    /// the encoder's forced-IDR/LTR recovery (Plan 04 §5b).
    /// `s`: the video stream (0 = the main one, which is all older peers know;
    /// 1.. = pop-out windows, see [`ControlMsg::Popout`]).
    #[serde(rename = "kf")]
    KeyframeRequest {
        #[serde(default, skip_serializing_if = "is_zero")]
        s: u8,
    },
    /// Host → Viewer: out-of-band cursor position + optional shape id
    /// (Plan 04 §5a/§7 — cursor rendered client-side so it feels instant).
    /// `x`/`y` normalized `[0,1]`: the pointer's hotspot. `shape` names the
    /// image (a [`ControlMsg::CursorShape`] id) the viewer draws there; `None`
    /// means the built-in arrow (older hosts never set it).
    #[serde(rename = "cur")]
    Cursor {
        x: f64,
        y: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        shape: Option<u32>,
        #[serde(default = "default_visible", skip_serializing_if = "is_true")]
        visible: bool,
        /// The video stream this pointer is reported over (0 = main).
        #[serde(default, skip_serializing_if = "is_zero")]
        s: u8,
    },
    /// Viewer → Host: the pixel size the viewer is actually displaying video at.
    /// The host encodes to exactly this (never upscaling past its own screen), so
    /// the viewer can present 1:1. Otherwise the frame is resampled on display —
    /// at a non-integer ratio that visibly softens every glyph — and bits are
    /// spent on pixels the viewer never shows.
    #[serde(rename = "vsize")]
    ViewSize {
        width: u32,
        height: u32,
        /// The video stream (0 = main).
        #[serde(default, skip_serializing_if = "is_zero")]
        s: u8,
    },
    /// Liveness probe, sent by BOTH sides on the reliable channel. The peer
    /// echoes it back as [`ControlMsg::Pong`]. Silence for the grace period means
    /// the session is dead even when the socket never reported an error — the
    /// case where a controller sat on a frozen picture believing it was live.
    #[serde(rename = "ping")]
    Ping { seq: u32 },
    /// Reply to [`ControlMsg::Ping`], echoing its `seq`.
    #[serde(rename = "pong")]
    Pong { seq: u32 },
    /// Clipboard text, synced in BOTH directions during a session. Text only —
    /// never file lists or rendered formats. Size-capped by the sender.
    #[serde(rename = "clip")]
    Clipboard { text: String },
    /// Host → Viewer, on the reliable `ctl` channel: one part of the host's
    /// cursor image (task D9; format, caching and limits in
    /// [`crate::cursor`]). Sent only when an image the viewer does not have
    /// yet becomes current. Older viewers cannot parse it and ignore it.
    #[serde(rename = "curshape")]
    CursorShape(crate::cursor::CursorShapePart),
    /// Host → Viewer: the host's monitors and the one being shared now. Sent
    /// once at session start and again after every switch (from either side),
    /// so the viewer can offer a monitor picker. `list` is empty when the host
    /// cannot enumerate its monitors (e.g. a Wayland portal picks the output);
    /// `current` is then empty too. Older viewers cannot parse it and ignore it.
    #[serde(rename = "displays")]
    Displays {
        list: Vec<RemoteDisplay>,
        current: String,
        /// Monitors also streamed to pop-out windows, each on its own video
        /// stream. The viewer opens a window per entry and closes any window
        /// whose entry is gone.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        popouts: Vec<PopoutStream>,
    },
    /// Viewer → Host: share the monitor with this [`RemoteDisplay::id`]
    /// instead. The host honours it only while the viewer has CONTROL (a
    /// view-only peer must not widen what the host chose to show) and answers
    /// with a fresh [`ControlMsg::Displays`]. Older hosts ignore it.
    #[serde(rename = "seldisp")]
    SelectDisplay { id: String },
    /// Viewer → Host: ALSO stream this monitor, on a spare video stream, for a
    /// separate window. Control only, like [`ControlMsg::SelectDisplay`]; the
    /// host answers with [`ControlMsg::Displays`] (listing it, or not if it
    /// could not). Older hosts ignore it.
    #[serde(rename = "popout")]
    Popout { id: String },
    /// Viewer → Host: the pop-out window for stream `s` was closed; stop
    /// streaming it.
    #[serde(rename = "popclose")]
    PopoutClose { s: u8 },
    /// Viewer → Host, on the ordered ctl channel: the pointer/button/wheel
    /// input that follows is over video stream `s` (0 = main), so its `[0,1]`
    /// coordinates map onto that stream's monitor. Sent only when the stream
    /// changes; keys are not positional and ignore it.
    #[serde(rename = "itgt")]
    InputTarget { s: u8 },
}

/// A monitor streamed to a pop-out window (see [`ControlMsg::Displays`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PopoutStream {
    /// The video stream it travels on (1.. ; 0 is the main window).
    pub s: u8,
    /// The [`RemoteDisplay::id`] shown.
    pub id: String,
}

/// One of the host's monitors, as announced in [`ControlMsg::Displays`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDisplay {
    /// The host's stable display id (opaque to the viewer; echoed back in
    /// [`ControlMsg::SelectDisplay`]).
    pub id: String,
    /// Human-readable name (e.g. `DISPLAY2`, `DELL U2720Q`).
    pub name: String,
    /// Size in the host's virtual-desktop pixels.
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub primary: bool,
}

fn default_visible() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_zero(v: &u8) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_wire_shape() {
        let m = ControlMsg::Perm {
            value: Permission::Control,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"t\":\"perm\""));
        assert!(s.contains("\"value\":\"control\""));
    }

    #[test]
    fn displays_wire_shape_roundtrips() {
        let m = ControlMsg::Displays {
            list: vec![RemoteDisplay {
                id: "dxgi:1".into(),
                name: "DISPLAY2".into(),
                width: 2560,
                height: 1440,
                primary: false,
            }],
            current: "dxgi:1".into(),
            popouts: vec![PopoutStream {
                s: 1,
                id: "dxgi:0".into(),
            }],
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"t\":\"displays\""), "{s}");
        assert!(matches!(
            serde_json::from_str::<ControlMsg>(&s).unwrap(),
            ControlMsg::Displays { ref list, ref current, ref popouts }
                if list.len() == 1 && current == "dxgi:1" && popouts.len() == 1
        ));
    }

    /// Stream 0 keeps the exact wire shapes older peers send and expect; a
    /// pop-out stream adds `s`.
    #[test]
    fn stream_zero_keeps_the_old_wire_shapes() {
        let kf = serde_json::to_string(&ControlMsg::KeyframeRequest { s: 0 }).unwrap();
        assert_eq!(kf, "{\"t\":\"kf\"}");
        assert!(matches!(
            serde_json::from_str::<ControlMsg>("{\"t\":\"kf\"}").unwrap(),
            ControlMsg::KeyframeRequest { s: 0 }
        ));
        let vs = serde_json::to_string(&ControlMsg::ViewSize {
            width: 1280,
            height: 720,
            s: 0,
        })
        .unwrap();
        assert_eq!(vs, "{\"t\":\"vsize\",\"width\":1280,\"height\":720}");
        let d =
            serde_json::from_str::<ControlMsg>("{\"t\":\"displays\",\"list\":[],\"current\":\"\"}")
                .unwrap();
        assert!(matches!(d, ControlMsg::Displays { ref popouts, .. } if popouts.is_empty()));
        let kf2 = serde_json::to_string(&ControlMsg::KeyframeRequest { s: 2 }).unwrap();
        assert_eq!(kf2, "{\"t\":\"kf\",\"s\":2}");
    }

    #[test]
    fn select_display_wire_shape() {
        let s = serde_json::to_string(&ControlMsg::SelectDisplay {
            id: "x11:HDMI-1".into(),
        })
        .unwrap();
        assert_eq!(s, "{\"t\":\"seldisp\",\"id\":\"x11:HDMI-1\"}");
    }

    #[test]
    fn bye_wire_shape() {
        let s = serde_json::to_string(&ControlMsg::Bye).unwrap();
        assert_eq!(s, "{\"t\":\"bye\"}");
    }
}
