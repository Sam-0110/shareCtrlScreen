//! Transport over `str0m` — sans-IO WebRTC, **no jitter buffer of our own, no
//! threads/timers** (Plan 04 §6). str0m gives standards-compliant ICE (NAT
//! traversal) + DTLS (encryption); we own all I/O so latency goes to the floor.
//! It reuses our Cloudflare WebSocket signaling unchanged (str0m's offer/answer +
//! trickle ICE relayed as the opaque `signal.data` payload).
//!
//! **Video rides an RTP media track, not a data channel.** This is the whole
//! point of the rewrite: a media track gets str0m's real-time video stack —
//! Google Congestion Control bandwidth estimation (BWE/TWCC), RTP packetization,
//! automatic NACK loss-repair from the send buffer, pacing, and in-order frame
//! reassembly with a bounded reorder window. That is exactly what the browser
//! build used to feel smooth; a data channel only offers general-purpose SCTP
//! flow control, which is why the data-channel video stalled and never matched
//! AnyDesk. Control and cursor stay on data channels where reliability/ordering
//! semantics matter more than a specialized congestion controller.
//!
//! | carrier            | reliability            | carries                      |
//! |--------------------|------------------------|------------------------------|
//! | `video` RTP media  | unreliable + NACK + BWE| encoded H.264 frames         |
//! | `ctl` data channel | reliable, ordered      | input, permission, cursor sh.|
//! | `cursor` data ch.  | unreliable-latest      | high-rate cursor position    |

pub mod fec;
pub mod packet;

use str0m::bwe::BweKind;
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::media::{Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid, Pt};
use str0m::{Event, Input, Output, Rtc, RtcError};

pub use std::time::Instant;
pub use str0m::net::{Protocol, Receive, Transmit};
pub use str0m::{Candidate, IceConnectionState};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("str0m: {0}")]
    Rtc(#[from] RtcError),
    #[error("channel {0} not open")]
    ChannelClosed(&'static str),
}

/// The data-channel ids once opened (§6). Video is an RTP media track (see
/// [`Transport::set_video_mid`]), negotiated via the engine's SDP API, so it is
/// not a channel here.
#[derive(Debug, Clone, Copy)]
pub struct Channels {
    pub ctl: ChannelId,
    pub cursor: ChannelId,
    pub bulk: ChannelId,
}

/// The §6 data-channel configs. The host creates them when building the offer;
/// the viewer matches them by label. Video is added separately as a media track
/// in the SDP offer (see `begin_host` in the engine).
pub fn channel_configs() -> [ChannelConfig; 3] {
    [
        ChannelConfig {
            label: "ctl".to_string(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        },
        ChannelConfig {
            label: "cursor".to_string(),
            ordered: false,
            // Unreliable-latest: a stale cursor position is worthless.
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            negotiated: None,
            protocol: String::new(),
        },
        ChannelConfig {
            // Bulk transfer (file contents, large clipboard payloads) on its OWN
            // reliable channel. Sharing `ctl` would head-of-line block every
            // keystroke and mouse click behind a multi-megabyte file — the
            // session would feel frozen for the whole transfer.
            label: "bulk".to_string(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        },
    ]
}

/// Inbound payloads surfaced to the engine after routing a str0m event.
#[derive(Debug, Clone)]
pub enum Inbound {
    Connected,
    Disconnected,
    /// A whole decoded video frame from the RTP media track (viewer side). str0m
    /// has already depacketized and reassembled it; `contiguous` is false when a
    /// gap (unrecovered loss) preceded it, which may warrant a keyframe request.
    Video {
        /// Which video stream (0 = main; 1.. = pop-out windows), by the
        /// track's position in the offer.
        slot: u8,
        data: Vec<u8>,
        keyframe: bool,
        contiguous: bool,
    },
    /// The remote peer asked us (the host) for a keyframe (PLI/FIR over RTCP)
    /// on video stream `slot`.
    KeyframeRequest(u8),
    /// A fresh send-bandwidth estimate in bits/sec from str0m's congestion
    /// control (host side). This is the real BWE that steers the encoder bitrate,
    /// replacing the old hand-rolled AIMD on the SCTP send-queue depth.
    BweEstimate(u32),
    /// A reliable control message (host: input; viewer: perm/bye/cursor-shape).
    Ctl(Vec<u8>),
    /// A cursor position update.
    Cursor(Vec<u8>),
    /// One framed message on the bulk channel (file metadata/chunks).
    Bulk(Vec<u8>),
    /// A data channel opened; once all are known the engine has [`Channels`].
    ChannelOpen(ChannelId, String),
}

/// str0m wrapper holding the data channels and the video media track.
pub struct Transport {
    rtc: Rtc,
    ctl: Option<ChannelId>,
    cursor: Option<ChannelId>,
    bulk: Option<ChannelId>,
    /// The video RTP media tracks' mids, indexed by stream slot (0 = main,
    /// 1.. = pop-outs). Host: from `add_media` via [`Self::set_video_mids`].
    /// Viewer: from the offer's m-line order ([`video_mids_in_sdp`]), or, if
    /// that found none, learned from `Event::MediaAdded`.
    video_mids: Vec<Mid>,
    /// Negotiated video payload type + clock per slot, resolved lazily on first
    /// send once the SDP answer is in (the remote PTs aren't known before that).
    video_pt: Vec<Option<(Pt, Frequency)>>,
    /// Monotonic origin for RTP media timestamps.
    media_start: Instant,
}

impl Transport {
    /// Wrap an already-built [`Rtc`]. The host builds it as offerer (adding the
    /// video media track + data channels), the viewer as answerer.
    pub fn new(rtc: Rtc) -> Self {
        Self {
            rtc,
            ctl: None,
            cursor: None,
            bulk: None,
            video_mids: Vec::new(),
            video_pt: Vec::new(),
            media_start: Instant::now(),
        }
    }

    /// Host side: adopt the two §6 data-channel ids created on the `Rtc` before it
    /// was handed to this transport.
    pub fn set_channels(&mut self, ch: Channels) {
        self.ctl = Some(ch.ctl);
        self.cursor = Some(ch.cursor);
        self.bulk = Some(ch.bulk);
    }

    /// Adopt the video media tracks' mids, in slot order: the host's from
    /// `add_media`, the viewer's from [`video_mids_in_sdp`] on the offer.
    pub fn set_video_mids(&mut self, mids: Vec<Mid>) {
        self.video_pt = vec![None; mids.len()];
        self.video_mids = mids;
    }

    fn slot_of(&self, mid: Mid) -> Option<u8> {
        self.video_mids
            .iter()
            .position(|m| *m == mid)
            .and_then(|i| u8::try_from(i).ok())
    }

    pub fn rtc_mut(&mut self) -> &mut Rtc {
        &mut self.rtc
    }

    /// Drive the sans-IO state machine one step (§6: we own the loop).
    pub fn poll_output(&mut self) -> Result<Output, Error> {
        Ok(self.rtc.poll_output()?)
    }

    /// Feed a timeout or a received datagram back into str0m.
    pub fn handle_input(&mut self, input: Input<'_>) -> Result<(), Error> {
        self.rtc.handle_input(input)?;
        Ok(())
    }

    /// Host: tell the BWE the bitrate we'd like to eventually reach, so it probes
    /// up toward it. The actual estimate comes back via
    /// `Event::EgressBitrateEstimate` → [`Inbound::BweEstimate`].
    pub fn set_desired_bitrate(&mut self, bps: u32) {
        self.rtc
            .bwe()
            .set_desired_bitrate(str0m::bwe::Bitrate::bps(bps as u64));
    }

    /// Host: write one encoded H.264 access unit (Annex-B) onto the video media
    /// track. str0m packetizes it into RTP (FU-A as needed), paces it, and repairs
    /// loss via NACK from its send buffer. A no-op until the track is negotiated
    /// and connected — str0m drops pre-connect media anyway, and the engine forces
    /// a keyframe on connect so the first *delivered* frame is decodable.
    pub fn send_video(&mut self, slot: u8, au: &[u8]) -> Result<(), Error> {
        let i = usize::from(slot);
        let Some(&mid) = self.video_mids.get(i) else {
            return Ok(());
        };
        // Resolve the negotiated PT + clock once. `payload_params()` is filtered by
        // the remote's PTs, which are only populated after the answer is accepted;
        // before that it's empty and we skip (the frame would be dropped anyway).
        if self.video_pt[i].is_none() {
            self.video_pt[i] = self.rtc.writer(mid).and_then(|w| {
                w.payload_params()
                    .next()
                    .map(|p| (p.pt(), p.spec().clock_rate))
            });
        }
        let Some((pt, freq)) = self.video_pt[i] else {
            return Ok(());
        };

        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.media_start);
        // RTP timestamp in the codec clock (90 kHz for video).
        let ticks = (elapsed.as_micros() as u64) * freq.get() as u64 / 1_000_000;
        let rtp_time = MediaTime::new(ticks, freq);

        if let Some(writer) = self.rtc.writer(mid) {
            writer.write(pt, now, rtp_time, au)?;
        }
        Ok(())
    }

    /// Viewer: ask the sender for a fresh keyframe via native RTCP PLI. (The
    /// engine also has a reliable `ctl`-channel keyframe request; this is the
    /// RTP-native path, used when str0m's reorder window reports a gap.)
    pub fn request_keyframe(&mut self, slot: u8) {
        if let Some(&mid) = self.video_mids.get(usize::from(slot)) {
            if let Some(mut writer) = self.rtc.writer(mid) {
                let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
            }
        }
    }

    /// Send a reliable control payload (input event / perm / bye / cursor
    /// image part). `Ok(false)` = **not accepted**: str0m's send buffer
    /// (128 KiB across all channels, freed as the peer acknowledges) has no
    /// room for it right now. The caller must keep it and retry, in order;
    /// treating `false` as sent silently loses a "reliable" message.
    pub fn send_ctl(&mut self, bytes: &[u8]) -> Result<bool, Error> {
        let id = self.ctl.ok_or(Error::ChannelClosed("ctl"))?;
        match self.rtc.channel(id) {
            Some(mut ch) => Ok(ch.write(true, bytes)?),
            None => Err(Error::ChannelClosed("ctl")),
        }
    }

    /// Send a cursor position update on the unreliable-latest cursor channel.
    pub fn send_cursor(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let id = self.cursor.ok_or(Error::ChannelClosed("cursor"))?;
        if let Some(mut ch) = self.rtc.channel(id) {
            ch.write(true, bytes)?;
        }
        Ok(())
    }

    /// Send one framed message on the reliable bulk channel (file transfer).
    pub fn send_bulk(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let id = self.bulk.ok_or(Error::ChannelClosed("bulk"))?;
        if let Some(mut ch) = self.rtc.channel(id) {
            ch.write(true, bytes)?;
        }
        Ok(())
    }

    /// Bytes still queued on the bulk channel. The file sender waits on this so a
    /// transfer can't queue the whole file at once and starve the video stream of
    /// bandwidth for the entire duration.
    pub fn bulk_buffered(&mut self) -> usize {
        match self.bulk {
            Some(id) => self
                .rtc
                .channel(id)
                .map(|mut ch| ch.buffered_amount())
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Route a str0m [`Event`] into an [`Inbound`]. Returns `None` for events the
    /// engine does not need to act on.
    pub fn on_event(&mut self, event: Event) -> Option<Inbound> {
        match event {
            Event::Connected => {
                tracing::info!("transport: DTLS/ICE connected");
                Some(Inbound::Connected)
            }
            Event::IceConnectionStateChange(state) => {
                tracing::info!("transport: ICE state = {state:?}");
                if state == IceConnectionState::Disconnected {
                    Some(Inbound::Disconnected)
                } else {
                    None
                }
            }
            Event::MediaAdded(m) => {
                // Viewer: learn the incoming video track's mid (host set it from
                // add_media). Nothing for the engine to act on directly.
                if m.kind == MediaKind::Video {
                    if self.slot_of(m.mid).is_none() {
                        self.video_mids.push(m.mid);
                        self.video_pt.push(None);
                    }
                    tracing::info!("transport: video media track added ({:?})", m.mid);
                }
                None
            }
            Event::MediaData(d) => {
                if d.data.is_empty() {
                    return None;
                }
                let slot = self.slot_of(d.mid)?;
                Some(Inbound::Video {
                    slot,
                    keyframe: d.is_keyframe(),
                    contiguous: d.contiguous,
                    data: d.data.to_vec(),
                })
            }
            Event::KeyframeRequest(r) => Some(Inbound::KeyframeRequest(self.slot_of(r.mid)?)),
            Event::EgressBitrateEstimate(kind) => {
                let bps = match kind {
                    BweKind::Twcc(b) | BweKind::Remb(_, b) => b.as_u64(),
                    _ => return None,
                };
                Some(Inbound::BweEstimate(bps.min(u32::MAX as u64) as u32))
            }
            Event::ChannelOpen(id, label) => {
                // Learn ids on the viewer side (channels created by the host).
                match label.as_str() {
                    "ctl" => self.ctl = Some(id),
                    "cursor" => self.cursor = Some(id),
                    "bulk" => self.bulk = Some(id),
                    _ => {}
                }
                Some(Inbound::ChannelOpen(id, label))
            }
            Event::ChannelData(data) => {
                if Some(data.id) == self.ctl {
                    Some(Inbound::Ctl(data.data))
                } else if Some(data.id) == self.cursor {
                    Some(Inbound::Cursor(data.data))
                } else if Some(data.id) == self.bulk {
                    Some(Inbound::Bulk(data.data))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// The `a=mid` of every video m-line in `sdp`, in order: the stream slots the
/// host created them for (0 = main, then the pop-out spares).
pub fn video_mids_in_sdp(sdp: &str) -> Vec<Mid> {
    let mut out = Vec::new();
    let mut in_video = false;
    for line in sdp.lines() {
        let line = line.trim_end();
        if let Some(m) = line.strip_prefix("m=") {
            in_video = m.starts_with("video ");
        } else if in_video {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                out.push(Mid::from(mid));
                in_video = false;
            }
        }
    }
    out
}

#[cfg(test)]
mod sdp_tests {
    use super::*;

    #[test]
    fn video_mids_come_out_in_m_line_order_and_skip_other_media() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=sendonly\r\na=mid:abc\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:dc0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:xyz\r\n";
        let mids = video_mids_in_sdp(sdp);
        assert_eq!(mids, vec![Mid::from("abc"), Mid::from("xyz")]);
    }
}
