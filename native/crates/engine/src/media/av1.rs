//! AV1 temporal units as the RTP payload format expects them (host side).
//!
//! The AV1 RTP payloader (str0m, per the AV1 RTP specification) splits a
//! temporal unit into OBUs by reading it as the **low-overhead bitstream
//! format** (AV1 §5.2): OBU after OBU, each with its size field except
//! possibly the last. An encoder may instead hand out the **Annex B
//! length-delimited format** (AV1 Annex B: `temporal_unit_size`, then
//! `frame_unit_size`s, then `obu_length`-prefixed OBUs, usually without size
//! fields). Read as low-overhead, Annex B's leading length bytes look like
//! OBU headers — the 2.3.0 macOS report: every frame from a Windows hardware
//! AV1 encoder arrived with "forbidden bit set in an OBU header", a sequence
//! header that "ends early", and nonsense picture sizes (2×9, 1×1).
//!
//! [`normalize`] accepts either form and returns the temporal unit in the
//! low-overhead form with **every OBU carrying its size field** (what
//! ISOBMFF and VideoToolbox take as well), temporal delimiters dropped (the
//! RTP format omits them). A unit that is neither is passed on unchanged and
//! reported, so the host's log names the problem.
//!
//! ## References
//! * AV1 Bitstream & Decoding Process Specification: §5.2 low-overhead
//!   format, §5.3 OBU syntax (`obu_header`, `obu_has_size_field`, leb128),
//!   Annex B length-delimited format:
//!   <https://aomediacodec.github.io/av1-spec/av1-spec.pdf>
//! * RTP Payload Format for AV1 §4.4 (temporal delimiters are not sent; OBU
//!   elements without `obu_size`): <https://aomediacodec.github.io/av1-rtp-spec/>

/// OBU types (AV1 §6.2.2) that may appear in a temporal unit.
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const VALID_OBU_TYPES: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 15];

/// What the encoder's temporal unit turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    /// Low-overhead (§5.2), already sized throughout.
    LowOverhead,
    /// Low-overhead with OBUs lacking size fields; sizes were added.
    LowOverheadUnsized,
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
/// forbidden bit clear, a known type. Returns (header length, has size).
fn header(data: &[u8]) -> Option<(usize, bool)> {
    let first = *data.first()?;
    if first & 0x80 != 0 || !VALID_OBU_TYPES.contains(&((first >> 3) & 0x0f)) {
        return None;
    }
    let len = if first & 0b100 != 0 { 2 } else { 1 };
    (data.len() >= len).then_some((len, first & 0b10 != 0))
}

/// `data` as low-overhead OBUs; `None` unless it parses to the exact end.
/// The second value: some OBU lacked a size field (only valid for the last).
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
        obus.push(Obu {
            header: &data[..hlen],
            payload,
        });
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

/// The temporal unit `au` in the low-overhead form, every OBU sized and the
/// temporal delimiters left out (see the module docs), and what it was.
pub(crate) fn normalize(au: Vec<u8>) -> (Vec<u8>, Form) {
    // Low-overhead and fully sized: the common case, unchanged except for
    // the temporal delimiters, which the payloader drops anyway.
    let parsed = match low_overhead(&au) {
        Some((obus, false)) => Some((obus, Form::LowOverhead)),
        Some((obus, true)) => Some((obus, Form::LowOverheadUnsized)),
        None => annex_b(&au).map(|obus| (obus, Form::AnnexB)),
    };
    let Some((obus, form)) = parsed else {
        return (au, Form::Unknown);
    };
    if form == Form::LowOverhead {
        return (au, form);
    }
    let mut out = Vec::with_capacity(au.len() + 8);
    for obu in obus
        .iter()
        .filter(|o| (o.header[0] >> 3) & 0x0f != OBU_TEMPORAL_DELIMITER)
    {
        out.push(obu.header[0] | 0b10);
        out.extend_from_slice(&obu.header[1..]);
        write_leb128(obu.payload.len(), &mut out);
        out.extend_from_slice(obu.payload);
    }
    (out, form)
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

    #[test]
    fn a_sized_low_overhead_unit_passes_unchanged() {
        let (out, form) = normalize(sized());
        assert_eq!(form, Form::LowOverhead);
        assert_eq!(out, sized());
    }

    #[test]
    fn an_annex_b_unit_becomes_sized_low_overhead() {
        // OBUs without size fields, each prefixed by obu_length, in one frame
        // unit, in one temporal unit.
        let obus: Vec<Vec<u8>> = vec![
            vec![0x10],                   // TD, no size field
            vec![0x08, 0xaa, 0xbb, 0xcc], // sequence header
            vec![0x30, 1, 2, 3, 4, 5],    // frame
        ];
        let mut fu = Vec::new();
        for o in &obus {
            write_leb128(o.len(), &mut fu);
            fu.extend_from_slice(o);
        }
        let mut tu = Vec::new();
        write_leb128(fu.len(), &mut tu);
        tu.extend_from_slice(&fu);
        let mut au = Vec::new();
        write_leb128(tu.len(), &mut au);
        au.extend_from_slice(&tu);
        let (out, form) = normalize(au);
        assert_eq!(form, Form::AnnexB);
        assert_eq!(out, expected());
    }

    #[test]
    fn an_unsized_last_obu_gets_its_size() {
        let au = vec![
            0x12, 0x00, 0x0a, 0x03, 0xaa, 0xbb, 0xcc, 0x30, 1, 2, 3, 4, 5,
        ];
        let (out, form) = normalize(au);
        assert_eq!(form, Form::LowOverheadUnsized);
        assert_eq!(out, expected());
    }

    #[test]
    fn neither_form_is_passed_on_unchanged() {
        let junk = vec![0xff, 0x01, 0x02];
        let (out, form) = normalize(junk.clone());
        assert_eq!(form, Form::Unknown);
        assert_eq!(out, junk);
        assert_eq!(hex_prefix(&junk), "ff 01 02");
    }

    #[test]
    fn large_sizes_use_multi_byte_leb128() {
        let payload = vec![7u8; 300];
        let mut au = vec![0x30];
        au.extend_from_slice(&payload); // one unsized frame OBU
        let (out, form) = normalize(au);
        assert_eq!(form, Form::LowOverheadUnsized);
        assert_eq!(&out[..3], &[0x32, 0xac, 0x02]); // 300 = 0x12c
        assert_eq!(out.len(), 3 + 300);
    }
}
