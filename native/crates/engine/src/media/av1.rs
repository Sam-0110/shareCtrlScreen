//! AV1 temporal units as the RTP payload format expects them (host side).
//!
//! The AV1 RTP payloader (str0m, per the AV1 RTP specification) splits a
//! temporal unit into OBUs by reading it as the **low-overhead bitstream
//! format** (AV1 §5.2): OBU after OBU, each with its size field except
//! possibly the last. An encoder may instead hand out the **Annex B
//! length-delimited format** (AV1 Annex B: `temporal_unit_size`, then
//! `frame_unit_size`s, then `obu_length`-prefixed OBUs, usually without size
//! fields). Read as low-overhead, Annex B's leading length bytes look like
//! OBU headers. The 2.3.0 macOS report fits that: every frame from a Windows
//! hardware AV1 encoder arrived with "forbidden bit set in an OBU header", a
//! sequence header that "ends early", nonsense picture sizes (2×9, 1×1), and
//! never a key frame. The 2.3.2 host log settled it: that encoder wraps its
//! output in **IVF container framing** (a `DKIF` file header, then a 12-byte
//! header per frame), which [`Normalizer`] now strips too.
//!
//! A [`Normalizer`] (one per encoder) returns every temporal unit in the
//! low-overhead form with **every OBU carrying its size field** (what
//! ISOBMFF and VideoToolbox take as well) and no temporal delimiters (the
//! RTP format omits them):
//! * **The form is decided once**, on the first unit that cannot be
//!   ambiguous — a key frame, or one of 128 bytes or more, whose Annex B
//!   `temporal_unit_size` takes two bytes and so starts with a byte that
//!   cannot be an OBU header. A small unit can read validly both ways; a
//!   per-unit guess would mangle static-screen frames (W-review 2.3.1).
//! * **Key frames carry the sequence header.** The first key frame (which
//!   has it) is usually sent before the link is up and lost; a forced one
//!   may not repeat it, and the viewer cannot decode without it in-band. So
//!   the latest sequence header is kept and put first in a key frame that
//!   lacks one.
//! * A unit that is neither form is passed on unchanged and reported.
//!
//! ## References
//! * IVF framing (32-byte `DKIF` file header; per frame a little-endian u32
//!   size and u64 timestamp): <https://wiki.multimedia.cx/index.php/IVF>
//! * AV1 Bitstream & Decoding Process Specification: §5.2 low-overhead
//!   format, §5.3 OBU syntax (`obu_header`, `obu_has_size_field`,
//!   `obu_reserved_1bit`, leb128), §7.5 (a sequence header starts a coded
//!   video sequence), Annex B length-delimited format:
//!   <https://aomediacodec.github.io/av1-spec/av1-spec.pdf>
//! * RTP Payload Format for AV1 §4.4 (temporal delimiters are not sent; the N
//!   bit marks a unit that starts with a sequence header):
//!   <https://aomediacodec.github.io/av1-rtp-spec/>

/// OBU types (AV1 §6.2.2) that may appear in a temporal unit.
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const VALID_OBU_TYPES: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 15];
/// From this size on, an Annex B unit's `temporal_unit_size` takes two or
/// more bytes, so its first byte has bit 7 set and is no OBU header.
const UNAMBIGUOUS_LEN: usize = 128;

/// What the encoder's temporal units are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    /// Low-overhead (§5.2), already sized throughout.
    LowOverhead,
    /// Low-overhead with an OBU lacking its size field; sizes are added.
    LowOverheadUnsized,
    /// Low-overhead OBUs inside IVF container framing (a 32-byte `DKIF`
    /// file header on the first unit, a 12-byte frame header on every one);
    /// the framing is stripped. What the owner's Windows hardware AV1 MFT
    /// gives (2.3.2 host log: "44 4b 49 46 00 00 20 00 41 56 30 31…").
    Ivf,
    /// Annex B; converted.
    AnnexB,
    /// Neither: passed on unchanged.
    Unknown,
}

/// One OBU: its header bytes (1, or 2 with the extension) and payload.
struct Obu<'a> {
    header: &'a [u8],
    payload: &'a [u8],
}

impl Obu<'_> {
    fn kind(&self) -> u8 {
        (self.header[0] >> 3) & 0x0f
    }

    /// The OBU with its size field, appended to `out`.
    fn write_sized(&self, out: &mut Vec<u8>) {
        out.push(self.header[0] | 0b10);
        out.extend_from_slice(&self.header[1..]);
        write_leb128(self.payload.len(), out);
        out.extend_from_slice(self.payload);
    }
}

fn read_leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    for (i, &byte) in data.iter().enumerate().take(8) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((usize::try_from(value).ok()?, i + 1));
        }
    }
    None
}

fn write_leb128(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// The header of the OBU at the start of `data`, if it is a valid one:
/// forbidden and reserved bits clear, a known type. Returns (header length,
/// has size).
fn header(data: &[u8]) -> Option<(usize, bool)> {
    let first = *data.first()?;
    if first & 0x81 != 0 || !VALID_OBU_TYPES.contains(&((first >> 3) & 0x0f)) {
        return None;
    }
    let len = if first & 0b100 != 0 { 2 } else { 1 };
    (data.len() >= len).then_some((len, first & 0b10 != 0))
}

/// `data` as low-overhead OBUs; `None` unless it parses to the exact end
/// (and a temporal delimiter is empty, as §5.6 requires). The second value:
/// some OBU lacked a size field (only valid for the last).
fn low_overhead(mut data: &[u8]) -> Option<(Vec<Obu<'_>>, bool)> {
    let mut obus = Vec::new();
    let mut unsized_seen = false;
    while !data.is_empty() {
        let (hlen, has_size) = header(data)?;
        let rest = &data[hlen..];
        let (size, size_len) = if has_size {
            read_leb128(rest)?
        } else {
            unsized_seen = true;
            (rest.len(), 0)
        };
        let payload = rest.get(size_len..size_len.checked_add(size)?)?;
        let obu = Obu {
            header: &data[..hlen],
            payload,
        };
        if obu.kind() == OBU_TEMPORAL_DELIMITER && !payload.is_empty() {
            return None;
        }
        obus.push(obu);
        data = &rest[size_len + size..];
    }
    (!obus.is_empty()).then_some((obus, unsized_seen))
}

/// `data` as one Annex B temporal unit; `None` unless every length agrees.
fn annex_b(data: &[u8]) -> Option<Vec<Obu<'_>>> {
    let (tu_size, n) = read_leb128(data)?;
    let mut tu = data.get(n..)?;
    if tu.len() != tu_size {
        return None;
    }
    let mut obus = Vec::new();
    while !tu.is_empty() {
        let (fu_size, n) = read_leb128(tu)?;
        let mut fu = tu.get(n..n.checked_add(fu_size)?)?;
        tu = &tu[n + fu_size..];
        while !fu.is_empty() {
            let (obu_len, n) = read_leb128(fu)?;
            let obu = fu.get(n..n.checked_add(obu_len)?)?;
            fu = &fu[n + obu_len..];
            let (hlen, has_size) = header(obu)?;
            let rest = &obu[hlen..];
            // A size field inside Annex B must agree with the length around it.
            let payload = if has_size {
                let (size, size_len) = read_leb128(rest)?;
                (size_len + size == rest.len()).then_some(&rest[size_len..])?
            } else {
                rest
            };
            obus.push(Obu {
                header: &obu[..hlen],
                payload,
            });
        }
    }
    (!obus.is_empty()).then_some(obus)
}

/// The frame inside IVF framing: an optional file header (`DKIF`, its
/// length at bytes 6..8), then a frame header of a little-endian u32 frame
/// size and a u64 timestamp; `None` unless the size matches what follows.
fn strip_ivf(data: &[u8]) -> Option<&[u8]> {
    let mut d = data;
    if d.starts_with(b"DKIF") {
        let header_len = usize::from(u16::from_le_bytes([*d.get(6)?, *d.get(7)?]));
        d = d.get(header_len..)?;
    }
    let size = usize::try_from(u32::from_le_bytes(d.get(..4)?.try_into().ok()?)).ok()?;
    let frame = d.get(12..)?;
    (frame.len() == size).then_some(frame)
}

/// `data` read as `form`.
fn parse(data: &[u8], form: Form) -> Option<Vec<Obu<'_>>> {
    match form {
        Form::AnnexB => annex_b(data),
        Form::Ivf => strip_ivf(data).and_then(low_overhead).map(|(o, _)| o),
        Form::LowOverhead | Form::LowOverheadUnsized => low_overhead(data).map(|(o, _)| o),
        Form::Unknown => None,
    }
}

/// What one unit looks like on its own. A unit whose first byte cannot be an
/// OBU header and that reads as Annex B is Annex B; otherwise low-overhead
/// wins (a small Annex B unit may read both ways: the caller decides the
/// form only on units where that cannot happen).
fn classify(data: &[u8]) -> Form {
    // IVF framing first: its frame header can also read as an OBU header
    // (the first key frame's "DKIF" did, as one huge unsized OBU).
    if strip_ivf(data).and_then(low_overhead).is_some() {
        return Form::Ivf;
    }
    if data.first().is_some_and(|b| b & 0x80 != 0) && annex_b(data).is_some() {
        return Form::AnnexB;
    }
    match low_overhead(data) {
        Some((_, false)) => Form::LowOverhead,
        Some((_, true)) => Form::LowOverheadUnsized,
        None if annex_b(data).is_some() => Form::AnnexB,
        None => Form::Unknown,
    }
}

/// One encoder's AV1 output, put in the RTP form (see the module docs).
#[derive(Default)]
pub(crate) struct Normalizer {
    /// Decided on the first unambiguous unit.
    form: Option<Form>,
    /// The latest sequence header, with its size field.
    sequence_header: Option<Vec<u8>>,
    /// The first key frame's bytes were logged.
    keyframe_logged: bool,
    /// A unit that did not read as the decided form was reported.
    mismatch_logged: bool,
}

impl Normalizer {
    /// The encoder was replaced (a new size): the sequence header kept is
    /// stale and is never put before a later key frame.
    pub(crate) fn encoder_changed(&mut self) {
        self.sequence_header = None;
    }

    /// Unit `au` (`keyframe` per the encoder) in the RTP form.
    pub(crate) fn normalize(&mut self, au: Vec<u8>, keyframe: bool) -> Vec<u8> {
        if keyframe && !self.keyframe_logged {
            self.keyframe_logged = true;
            tracing::info!(
                "host: first AV1 key frame from the encoder: {} bytes, {}",
                au.len(),
                hex_prefix(&au)
            );
        }
        let form = match self.form {
            Some(form) => form,
            None => {
                let form = classify(&au);
                if form != Form::Unknown && (keyframe || au.len() >= UNAMBIGUOUS_LEN) {
                    self.form = Some(form);
                    tracing::info!("host: AV1 encoder output: {form:?}");
                }
                form
            }
        };
        let obus = parse(&au, form).or_else(|| {
            // Not what the encoder gave so far: read it as it reads on its
            // own, and say so once.
            let own = classify(&au);
            if !self.mismatch_logged {
                self.mismatch_logged = true;
                tracing::warn!(
                    "host: an AV1 unit is not {form:?} but {own:?} ({} bytes: {})",
                    au.len(),
                    hex_prefix(&au)
                );
            }
            parse(&au, own)
        });
        let Some(obus) = obus else {
            return au;
        };
        let has_sequence_header = obus.iter().any(|o| o.kind() == OBU_SEQUENCE_HEADER);
        if let Some(sh) = obus.iter().find(|o| o.kind() == OBU_SEQUENCE_HEADER) {
            let mut sized = Vec::with_capacity(sh.payload.len() + 4);
            sh.write_sized(&mut sized);
            self.sequence_header = Some(sized);
        }
        let prepend = keyframe && !has_sequence_header;
        // Already sized throughout and nothing to add: as it came (the
        // payloader drops the temporal delimiters itself).
        if form == Form::LowOverhead && !prepend && low_overhead(&au).is_some_and(|(_, u)| !u) {
            return au;
        }
        let mut out = Vec::with_capacity(au.len() + 16);
        if prepend {
            if let Some(sh) = &self.sequence_header {
                out.extend_from_slice(sh);
            }
        }
        for obu in obus.iter().filter(|o| o.kind() != OBU_TEMPORAL_DELIMITER) {
            obu.write_sized(&mut out);
        }
        out
    }
}

/// The first bytes of `data` in hex, for a log line.
pub(crate) fn hex_prefix(data: &[u8]) -> String {
    data.iter()
        .take(24)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporal delimiter, a sequence header (3 payload bytes) and a frame
    /// (5 payload bytes), every OBU sized: the canonical low-overhead form.
    fn sized() -> Vec<u8> {
        vec![
            0x12, 0x00, // TD, size 0
            0x0a, 0x03, 0xaa, 0xbb, 0xcc, // sequence header, size 3
            0x32, 0x05, 1, 2, 3, 4, 5, // frame, size 5
        ]
    }

    /// The same OBUs without temporal delimiter, all sized.
    fn expected() -> Vec<u8> {
        vec![0x0a, 0x03, 0xaa, 0xbb, 0xcc, 0x32, 0x05, 1, 2, 3, 4, 5]
    }

    /// `obus` (each a whole OBU, usually without size field) as one Annex B
    /// temporal unit with one frame unit.
    fn annex(obus: &[Vec<u8>]) -> Vec<u8> {
        let mut fu = Vec::new();
        for o in obus {
            write_leb128(o.len(), &mut fu);
            fu.extend_from_slice(o);
        }
        let mut tu = Vec::new();
        write_leb128(fu.len(), &mut tu);
        tu.extend_from_slice(&fu);
        let mut au = Vec::new();
        write_leb128(tu.len(), &mut au);
        au.extend_from_slice(&tu);
        au
    }

    #[test]
    fn a_sized_low_overhead_unit_passes_unchanged() {
        let mut n = Normalizer::default();
        assert_eq!(n.normalize(sized(), true), sized());
        assert_eq!(n.form, Some(Form::LowOverhead));
    }

    #[test]
    fn an_annex_b_key_frame_becomes_sized_low_overhead() {
        let mut n = Normalizer::default();
        let au = annex(&[
            vec![0x10],                   // TD, no size field
            vec![0x08, 0xaa, 0xbb, 0xcc], // sequence header
            vec![0x30, 1, 2, 3, 4, 5],    // frame
        ]);
        assert_eq!(n.normalize(au, true), expected());
        assert_eq!(n.form, Some(Form::AnnexB));
    }

    /// W-review 2.3.1 finding 1: a small Annex B unit that also reads as one
    /// low-overhead OBU is converted as Annex B once the form is known.
    #[test]
    fn a_tiny_annex_b_unit_is_not_mistaken_for_low_overhead() {
        let mut n = Normalizer::default();
        let mut big = vec![0x30];
        big.extend_from_slice(&[9u8; 200]);
        n.normalize(annex(&[vec![0x08, 0xaa], big]), true);
        assert_eq!(n.form, Some(Form::AnnexB));
        // temporal_unit_size 50 = 0x32, which is also a sized frame OBU
        // header; frame_unit_size 49 then reads as exactly its size.
        let mut frame = vec![0x30];
        frame.extend_from_slice(&[7u8; 45]);
        let tiny = annex(&[vec![0x10], frame]);
        assert_eq!(&tiny[..2], &[0x32, 49]);
        assert_eq!(classify(&tiny), Form::LowOverhead, "ambiguous on its own");
        let mut want = vec![0x32];
        write_leb128(45, &mut want);
        want.extend_from_slice(&[7u8; 45]);
        assert_eq!(n.normalize(tiny, false), want);
    }

    #[test]
    fn a_key_frame_without_a_sequence_header_gets_the_last_one() {
        let mut n = Normalizer::default();
        n.normalize(sized(), true);
        let forced = vec![0x12, 0x00, 0x32, 0x02, 8, 9];
        let out = n.normalize(forced, true);
        assert_eq!(out, vec![0x0a, 0x03, 0xaa, 0xbb, 0xcc, 0x32, 0x02, 8, 9]);
        let inter = vec![0x32, 0x02, 8, 9];
        assert_eq!(n.normalize(inter.clone(), false), inter, "only key frames");
    }

    #[test]
    fn an_unsized_last_obu_gets_its_size() {
        let mut n = Normalizer::default();
        let au = vec![
            0x12, 0x00, 0x0a, 0x03, 0xaa, 0xbb, 0xcc, 0x30, 1, 2, 3, 4, 5,
        ];
        assert_eq!(n.normalize(au, true), expected());
        assert_eq!(n.form, Some(Form::LowOverheadUnsized));
    }

    /// The 2.3.2 host log: an IVF file header and frame header before the
    /// first key frame, a frame header before every later unit.
    #[test]
    fn ivf_framing_is_stripped() {
        let frame = |obus: &[u8], ts: u64| {
            let mut v = (obus.len() as u32).to_le_bytes().to_vec();
            v.extend_from_slice(&ts.to_le_bytes());
            v.extend_from_slice(obus);
            v
        };
        let mut first = b"DKIF".to_vec();
        first.extend_from_slice(&[0, 0, 32, 0]); // version 0, header 32 bytes
        first.extend_from_slice(b"AV01");
        first.extend_from_slice(&[0x80, 0x07, 0xb0, 0x04]); // 1920x1200
        first.extend_from_slice(&[0xe8, 0x03, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(first.len(), 32);
        first.extend_from_slice(&frame(&sized(), 0));
        let mut n = Normalizer::default();
        assert_eq!(n.normalize(first, true), expected());
        assert_eq!(n.form, Some(Form::Ivf));
        let later = frame(&[0x12, 0x00, 0x32, 0x02, 8, 9], 33);
        assert_eq!(n.normalize(later, false), vec![0x32, 0x02, 8, 9]);
    }

    #[test]
    fn neither_form_is_passed_on_unchanged() {
        let mut n = Normalizer::default();
        let junk = vec![0xff, 0x01, 0x02];
        assert_eq!(n.normalize(junk.clone(), false), junk);
        assert_eq!(n.form, None, "nothing decided on junk");
        assert_eq!(hex_prefix(&junk), "ff 01 02");
    }

    #[test]
    fn large_sizes_use_multi_byte_leb128() {
        let mut n = Normalizer::default();
        let mut au = vec![0x30];
        au.extend_from_slice(&[7u8; 300]); // one unsized frame OBU
        let out = n.normalize(au, false);
        assert_eq!(&out[..3], &[0x32, 0xac, 0x02]); // 300 = 0x12c
        assert_eq!(out.len(), 3 + 300);
    }
}
