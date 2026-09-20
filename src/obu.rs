//! AV1 open bitstream units, AV1 specification sections 5.3 and 7.5:
//! where each OBU begins and ends, where a temporal unit begins, and a
//! metadata OBU carrying a payload of the caller's own.
//!
//! Only the low-overhead bitstream format of section 5 is handled,
//! which is the one MP4 and WebM samples hold and the one ffmpeg's `obu`
//! muxer writes. The length-delimited Annex B format of the same
//! specification is a different framing around the same OBUs and
//! nothing here would change but the outer lengths.
//!
//! Two choices the specification makes, and this module follows:
//!
//! - `metadata_type` 6 to 31 is left for unregistered private use, so a
//!   format of one's own needs no registration and the UUID inside the
//!   payload tells its OBUs from anyone else's.
//! - `open_bitstream_unit` calls `trailing_bits` for every OBU that is
//!   not a tile group, tile list or frame, so a metadata OBU ends with
//!   a set bit and zeroes to the byte: one 0x80 byte here, counted in
//!   `obu_size`. Some writers leave it off, so a reader here accepts an
//!   OBU without it.

use crate::{Error, Result};

/// The OBU type of metadata.
pub const OBU_METADATA: u8 = 5;
/// The OBU type that opens a temporal unit.
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
/// The OBU type of a sequence header.
pub const OBU_SEQUENCE_HEADER: u8 = 1;
/// The OBU type of a frame header.
pub const OBU_FRAME_HEADER: u8 = 3;
/// The OBU type of a tile group.
pub const OBU_TILE_GROUP: u8 = 4;
/// The OBU type of a frame: a frame header and its tile group in one.
pub const OBU_FRAME: u8 = 6;
/// The OBU type of a repeated frame header.
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;

/// One OBU's place in a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObuRef<'a> {
    pub kind: u8,
    /// Where the OBU header begins.
    pub start: usize,
    /// Where the OBU ends.
    pub end: usize,
    /// The bytes after the header and the size field.
    pub payload: &'a [u8],
    /// Whether the header carried its extension byte.
    pub has_extension: bool,
}

/// The OBUs of a low-overhead stream, temporal unit or sample.
///
/// An OBU without a size field runs to the end of the bytes, which is
/// what the specification says and what makes a stream of exactly one
/// such OBU readable.
pub fn scan_obus(bytes: &[u8]) -> Result<Vec<ObuRef<'_>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let start = at;
        let header = bytes[at];
        if header & 0x80 != 0 {
            return Err(Error::Malformed {
                what: "an OBU with its forbidden bit set",
                at: start,
            });
        }
        let kind = header >> 3 & 0x0f;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        at += 1;
        if has_extension {
            if at >= bytes.len() {
                return Err(Error::Truncated { at: start });
            }
            at += 1;
        }
        let size = if has_size {
            let (size, next) = leb128(bytes, at)?;
            at = next;
            usize::try_from(size).map_err(|_| Error::TooLarge { at: start })?
        } else {
            bytes.len() - at
        };
        let end = at.checked_add(size).ok_or(Error::TooLarge { at: start })?;
        let payload = bytes.get(at..end).ok_or(Error::Truncated { at: start })?;
        out.push(ObuRef {
            kind,
            start,
            end,
            payload,
            has_extension,
        });
        at = end;
    }
    Ok(out)
}

/// One leb128 value and where it ended.
///
/// The AV1 specification reads at most eight bytes and this refuses a
/// ninth rather than reading on.
pub fn leb128(bytes: &[u8], mut at: usize) -> Result<(u64, usize)> {
    let start = at;
    let mut value = 0u64;
    for index in 0..8 {
        let byte = *bytes.get(at).ok_or(Error::Truncated { at: start })?;
        at += 1;
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok((value, at));
        }
    }
    Err(Error::Malformed {
        what: "a leb128 longer than eight bytes",
        at: start,
    })
}

/// Appends a leb128, in as few bytes as the value needs.
pub fn put_leb128(out: &mut Vec<u8>, mut value: u64) {
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

/// A payload as a metadata OBU of `metadata_type`, ready to splice into
/// a temporal unit.
pub fn write_metadata(metadata_type: u64, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 2);
    put_leb128(&mut body, metadata_type);
    body.extend_from_slice(payload);
    // trailing_bits: a set bit, then zeroes to the byte.
    body.push(0x80);

    let mut out = Vec::with_capacity(body.len() + 4);
    // Forbidden bit 0, type OBU_METADATA, no extension, size field
    // present, reserved bit 0.
    out.push(OBU_METADATA << 3 | 0x02);
    put_leb128(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    out
}

/// The payload of one OBU, if it is a metadata OBU of `metadata_type`.
///
/// The trailing bits come back off: the zero padding, then the byte
/// holding the set bit. A writer that left them off loses nothing,
/// because then the last byte is the payload's own.
pub fn metadata_payload<'a>(obu: &ObuRef<'a>, metadata_type: u64) -> Option<&'a [u8]> {
    if obu.kind != OBU_METADATA {
        return None;
    }
    let (kind, at) = leb128(obu.payload, 0).ok()?;
    if kind != metadata_type {
        return None;
    }
    let body = obu.payload.get(at..)?;
    let mut end = body.len();
    while end > 0 && body[end - 1] == 0 {
        end -= 1;
    }
    if end > 0 && body[end - 1] == 0x80 {
        end -= 1;
    }
    Some(&body[..end])
}

/// Every metadata payload of `metadata_type` in a low-overhead stream,
/// temporal unit or sample. Bytes that do not parse hold none.
pub fn metadata_of_type(bytes: &[u8], metadata_type: u64) -> Vec<Vec<u8>> {
    match scan_obus(bytes) {
        Ok(obus) => obus
            .iter()
            .filter_map(|obu| metadata_payload(obu, metadata_type).map(<[u8]>::to_vec))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The same, narrowed to the payloads that open with `uuid`: the AV1
/// spelling of a `user_data_unregistered` SEI.
pub fn user_data(bytes: &[u8], metadata_type: u64, uuid: &[u8; 16]) -> Vec<Vec<u8>> {
    metadata_of_type(bytes, metadata_type)
        .into_iter()
        .filter(|payload| payload.starts_with(uuid))
        .collect()
}

/// One temporal unit of a low-overhead stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemporalUnit {
    pub start: usize,
    pub end: usize,
    /// Where a metadata OBU goes: before the first frame header, which
    /// is after the delimiter, the sequence header and any metadata
    /// already there.
    pub insert_at: usize,
    /// Whether the temporal unit carries a sequence header. AV1 writers
    /// repeat one before each key frame, so this is as close to a sync
    /// sample as a reader gets without decoding the frame header.
    pub has_sequence_header: bool,
}

/// The temporal units of a low-overhead stream.
///
/// A temporal unit opens at a temporal delimiter OBU. Bytes before the
/// first delimiter are one unit of their own, which is what a single
/// MP4 or WebM sample looks like when its muxer dropped the delimiter.
pub fn temporal_units(bytes: &[u8]) -> Result<Vec<TemporalUnit>> {
    let obus = scan_obus(bytes)?;
    let mut out: Vec<TemporalUnit> = Vec::new();
    for obu in &obus {
        let opens = obu.kind == OBU_TEMPORAL_DELIMITER;
        if out.is_empty() || opens {
            if let Some(last) = out.last_mut() {
                last.end = obu.start;
            }
            out.push(TemporalUnit {
                start: obu.start,
                end: bytes.len(),
                insert_at: usize::MAX,
                has_sequence_header: false,
            });
        }
        let Some(unit) = out.last_mut() else { continue };
        if obu.kind == OBU_SEQUENCE_HEADER {
            unit.has_sequence_header = true;
        }
        let frame = matches!(
            obu.kind,
            OBU_FRAME_HEADER | OBU_FRAME | OBU_REDUNDANT_FRAME_HEADER | OBU_TILE_GROUP
        );
        if frame && unit.insert_at == usize::MAX {
            unit.insert_at = obu.start;
        }
    }
    for unit in out.iter_mut() {
        if unit.insert_at == usize::MAX {
            unit.insert_at = unit.end;
        }
    }
    Ok(out)
}

/// A temporal unit with `obu` spliced in before its frame header.
pub fn insert_metadata_obu(temporal_unit: &[u8], obu: &[u8]) -> Result<Vec<u8>> {
    let units = temporal_units(temporal_unit)?;
    let at = match units.first() {
        Some(unit) => unit.insert_at,
        None => {
            return Err(Error::Malformed {
                what: "the bytes hold no temporal unit",
                at: 0,
            })
        }
    };
    let mut out = Vec::with_capacity(temporal_unit.len() + obu.len());
    out.extend_from_slice(&temporal_unit[..at]);
    out.extend_from_slice(obu);
    out.extend_from_slice(&temporal_unit[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A UUID of this test's own, standing in for a carried format's.
    const UUID: [u8; 16] = [
        0x04, 0x1f, 0x74, 0xa3, 0x80, 0x90, 0x5e, 0x08, 0xbc, 0xfc, 0x76, 0x4d, 0xf2, 0xdc, 0xd4,
        0x66,
    ];

    /// A `metadata_type` from the unregistered private range.
    const MINE: u64 = 25;

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind << 3 | 0x02];
        put_leb128(&mut out, payload.len() as u64);
        out.extend_from_slice(payload);
        out
    }

    /// Two temporal units shaped like what an AV1 encoder writes: a
    /// delimiter, a sequence header before the key frame, a frame.
    fn a_stream() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&obu(OBU_TEMPORAL_DELIMITER, &[]));
        out.extend_from_slice(&obu(OBU_SEQUENCE_HEADER, &[0x0a, 0x0b]));
        out.extend_from_slice(&obu(OBU_FRAME, &[0x10; 20]));
        out.extend_from_slice(&obu(OBU_TEMPORAL_DELIMITER, &[]));
        out.extend_from_slice(&obu(OBU_FRAME, &[0x11; 12]));
        out
    }

    fn a_payload() -> Vec<u8> {
        let mut payload = UUID.to_vec();
        payload.push(1);
        payload.extend_from_slice(&[0x02, 0x03, 0x01, 0x00, 0x00]);
        payload
    }

    #[test]
    fn leb128_round_trips() {
        for value in [0u64, 1, 127, 128, 16383, 16384, 1 << 35, u32::MAX as u64] {
            let mut out = Vec::new();
            put_leb128(&mut out, value);
            assert_eq!(leb128(&out, 0).expect("a value"), (value, out.len()));
        }
        // Nine continuation bytes is longer than the specification
        // reads, and is refused rather than read on.
        assert_eq!(
            leb128(&[0x80; 9], 0),
            Err(Error::Malformed {
                what: "a leb128 longer than eight bytes",
                at: 0
            })
        );
        assert_eq!(leb128(&[0x80], 0), Err(Error::Truncated { at: 0 }));
        assert_eq!(leb128(&[0x00, 0x80], 1), Err(Error::Truncated { at: 1 }));
    }

    #[test]
    fn a_metadata_obu_carries_a_payload_and_reads_back() {
        let payload = a_payload();
        let bytes = write_metadata(MINE, &payload);
        assert_eq!(bytes[0] >> 3 & 0x0f, OBU_METADATA);
        assert_eq!(bytes[0] & 0x02, 0x02, "has_size_field");
        assert_eq!(bytes[0] & 0x04, 0, "no extension");
        assert_eq!(bytes[0] & 0x80, 0, "the forbidden bit is zero");
        let obus = scan_obus(&bytes).expect("obus");
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].payload[0], MINE as u8);
        assert_eq!(
            *obus[0].payload.last().expect("trailing bits"),
            0x80,
            "the OBU ends with trailing_bits"
        );
        assert_eq!(metadata_payload(&obus[0], MINE), Some(&payload[..]));
        assert_eq!(metadata_payload(&obus[0], 26), None, "another type");
        assert_eq!(user_data(&bytes, MINE, &UUID), vec![payload]);
    }

    #[test]
    fn a_payload_ending_in_zeroes_survives_the_trailing_bits() {
        let mut payload = UUID.to_vec();
        payload.push(1);
        payload.extend_from_slice(&[0x02, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00]);
        let bytes = write_metadata(MINE, &payload);
        assert_eq!(user_data(&bytes, MINE, &UUID), vec![payload]);
    }

    #[test]
    fn a_metadata_type_past_one_byte_reads_back() {
        // The type is a leb128, and a reader has to walk it rather than
        // read one byte.
        for kind in [0u64, 25, 127, 128, 5000] {
            let bytes = write_metadata(kind, &a_payload());
            assert_eq!(user_data(&bytes, kind, &UUID), vec![a_payload()]);
            assert!(user_data(&bytes, kind + 1, &UUID).is_empty());
        }
    }

    #[test]
    fn another_writers_metadata_is_left_alone() {
        // Metadata of the same private type, somebody else's UUID.
        let mut payload = vec![MINE as u8];
        payload.extend_from_slice(&[0xab; 16]);
        payload.push(0x80);
        let theirs = obu(OBU_METADATA, &payload);
        assert!(user_data(&theirs, MINE, &UUID).is_empty());
        assert_eq!(metadata_of_type(&theirs, MINE).len(), 1, "theirs is read");

        // ITU-T T.35 metadata, which encoders really do write.
        let t35 = obu(OBU_METADATA, &[4, 0xb5, 0x00, 0x3c, 0x80]);
        assert!(user_data(&t35, MINE, &UUID).is_empty());
        assert!(metadata_of_type(&t35, MINE).is_empty());
    }

    #[test]
    fn a_metadata_obu_goes_in_before_the_frame_header() {
        let stream = a_stream();
        let units = temporal_units(&stream).expect("temporal units");
        assert_eq!(units.len(), 2);
        assert!(units[0].has_sequence_header, "the key frame's unit");
        assert!(!units[1].has_sequence_header);
        assert_eq!(units[0].start, 0);
        assert_eq!(units.last().expect("a unit").end, stream.len());

        let obu_bytes = write_metadata(MINE, &a_payload());
        let first = &stream[units[0].start..units[0].end];
        let woven = insert_metadata_obu(first, &obu_bytes).expect("woven");
        let kinds: Vec<u8> = scan_obus(&woven)
            .expect("obus")
            .iter()
            .map(|obu| obu.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                OBU_TEMPORAL_DELIMITER,
                OBU_SEQUENCE_HEADER,
                OBU_METADATA,
                OBU_FRAME
            ]
        );
        assert_eq!(user_data(&woven, MINE, &UUID), vec![a_payload()]);
    }

    #[test]
    fn a_written_stream_keeps_every_other_obu_byte_for_byte() {
        let stream = a_stream();
        let obu_bytes = write_metadata(MINE, &a_payload());
        let mut woven = Vec::new();
        let mut at = 0usize;
        for unit in temporal_units(&stream).expect("temporal units") {
            woven.extend_from_slice(&stream[at..unit.insert_at]);
            woven.extend_from_slice(&obu_bytes);
            at = unit.insert_at;
        }
        woven.extend_from_slice(&stream[at..]);

        assert_eq!(user_data(&woven, MINE, &UUID), vec![a_payload(); 2]);
        let kept: Vec<Vec<u8>> = scan_obus(&woven)
            .expect("obus")
            .iter()
            .filter(|obu| metadata_payload(obu, MINE).is_none())
            .map(|obu| woven[obu.start..obu.end].to_vec())
            .collect();
        let original: Vec<Vec<u8>> = scan_obus(&stream)
            .expect("obus")
            .iter()
            .map(|obu| stream[obu.start..obu.end].to_vec())
            .collect();
        assert_eq!(kept, original);
        assert_eq!(temporal_units(&woven).expect("units").len(), 2);
    }

    #[test]
    fn an_obu_without_a_size_field_runs_to_the_end() {
        let mut bytes = vec![OBU_FRAME << 3];
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let obus = scan_obus(&bytes).expect("obus");
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn an_obu_with_an_extension_byte_reads() {
        let mut bytes = vec![OBU_FRAME << 3 | 0x04 | 0x02, 0x28];
        put_leb128(&mut bytes, 3);
        bytes.extend_from_slice(&[7, 8, 9]);
        let obus = scan_obus(&bytes).expect("obus");
        assert_eq!(obus.len(), 1);
        assert!(obus[0].has_extension);
        assert_eq!(obus[0].payload, &[7, 8, 9]);
        assert_eq!(
            scan_obus(&[OBU_FRAME << 3 | 0x04]),
            Err(Error::Truncated { at: 0 })
        );
    }

    #[test]
    fn an_obu_whose_size_overruns_is_refused_with_its_offset() {
        let mut bytes = vec![OBU_TEMPORAL_DELIMITER << 3 | 0x02, 0];
        bytes.push(OBU_FRAME << 3 | 0x02);
        put_leb128(&mut bytes, 100);
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(scan_obus(&bytes), Err(Error::Truncated { at: 2 }));
        assert_eq!(
            scan_obus(&[0x80]),
            Err(Error::Malformed {
                what: "an OBU with its forbidden bit set",
                at: 0
            })
        );
    }

    #[test]
    fn truncated_and_random_obus_never_panic() {
        let stream = {
            let mut bytes = a_stream();
            bytes.extend_from_slice(&write_metadata(MINE, &a_payload()));
            bytes
        };
        for cut in 0..stream.len() {
            let _ = scan_obus(&stream[..cut]);
            let _ = temporal_units(&stream[..cut]);
            let _ = user_data(&stream[..cut], MINE, &UUID);
            let _ = insert_metadata_obu(&stream[..cut], &[0x2a, 0x01, 0x80]);
        }
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..3000 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 40 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 33) as u8);
            }
            let _ = scan_obus(&bytes);
            let _ = temporal_units(&bytes);
            let _ = metadata_of_type(&bytes, MINE);
            let _ = user_data(&bytes, MINE, &UUID);
            let _ = insert_metadata_obu(&bytes, &[0x2a, 0x01, 0x80]);
            let _ = leb128(&bytes, 0);
        }
    }
}
