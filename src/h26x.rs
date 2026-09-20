//! What a NAL unit's header says and where an access unit begins:
//! ISO/IEC 14496-10 sections 7.3.1 and 7.4.1.2 for H.264, ISO/IEC
//! 23008-2 sections 7.3.1.2 and 7.4.2.4 for HEVC.
//!
//! The two codecs differ in the width of the header, the numbering of
//! the types and where the temporal id lives, and in nothing else that
//! matters at this level, so one [`Codec`] value carries the difference
//! and the rest of the crate takes it as an argument.

use crate::annexb::scan_nals;

/// Which of the two codecs a NAL belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// The H.264 NAL unit type of a sequence parameter set.
pub const H264_SPS: u8 = 7;
/// The H.264 NAL unit type of a picture parameter set.
pub const H264_PPS: u8 = 8;
/// The H.264 NAL unit type of an SEI.
pub const H264_SEI: u8 = 6;
/// The HEVC NAL unit type of a prefix SEI.
pub const H265_PREFIX_SEI: u8 = 39;
/// The HEVC NAL unit type of a suffix SEI, which nothing here writes
/// and nothing here reads a payload out of.
pub const H265_SUFFIX_SEI: u8 = 40;

impl Codec {
    /// How many bytes the NAL header takes: one for H.264's
    /// `nal_unit_header`, two for HEVC's.
    pub fn header_len(self) -> usize {
        match self {
            Codec::H264 => 1,
            Codec::H265 => 2,
        }
    }

    /// A NAL unit's type, or `None` when the NAL is too short to have
    /// one.
    pub fn nal_type(self, nal: &[u8]) -> Option<u8> {
        match self {
            Codec::H264 => nal.first().map(|byte| byte & 0x1f),
            Codec::H265 => {
                if nal.len() < 2 {
                    None
                } else {
                    Some(nal[0] >> 1 & 0x3f)
                }
            }
        }
    }

    /// Whether a type is a coded slice: the NALs an access unit is
    /// built around.
    pub fn is_vcl_type(self, kind: u8) -> bool {
        match self {
            // 1 to 5 are the slices. 19 and 20 are the auxiliary and
            // extension slices, which no encoder here writes and which
            // never open an access unit on their own.
            Codec::H264 => (1..=5).contains(&kind),
            Codec::H265 => kind <= 31,
        }
    }

    /// Whether a NAL is a coded slice.
    pub fn is_vcl(self, nal: &[u8]) -> bool {
        self.nal_type(nal)
            .is_some_and(|kind| self.is_vcl_type(kind))
    }

    /// Whether a slice type starts a random access point: the frames a
    /// file's sync samples are cut at.
    pub fn is_keyframe_type(self, kind: u8) -> bool {
        match self {
            Codec::H264 => kind == 5,
            // BLA, IDR and CRA: the IRAP range.
            Codec::H265 => (16..=23).contains(&kind),
        }
    }

    /// Whether a non-slice type opens a new access unit when it turns
    /// up after one.
    ///
    /// The types left out are the ones that belong to the access unit
    /// they follow: filler data, and the end of sequence and end of
    /// stream markers.
    pub fn starts_access_unit(self, kind: u8) -> bool {
        match self {
            Codec::H264 => matches!(kind, 6..=9 | 13..=18),
            Codec::H265 => matches!(kind, 32..=35 | 39..=47),
        }
    }

    /// The NAL header of a prefix SEI, for an access unit at
    /// `temporal_id_plus1`.
    ///
    /// HEVC requires a prefix SEI to carry the temporal id of the
    /// access unit it sits in, so the header is not a constant: a
    /// stream with temporal sub-layers would be non-conforming with one
    /// that always said zero. H.264 keeps its temporal id somewhere
    /// else entirely and its header is the same byte every time.
    pub fn sei_header(self, temporal_id_plus1: u8) -> Vec<u8> {
        match self {
            // nal_ref_idc 0, type 6.
            Codec::H264 => vec![H264_SEI],
            // type 39, layer 0, and the access unit's temporal id.
            Codec::H265 => vec![H265_PREFIX_SEI << 1, temporal_id_plus1.max(1) & 0x07],
        }
    }

    /// The `nuh_temporal_id_plus1` of a NAL, which is 1 for H.264,
    /// where the field does not exist.
    pub fn temporal_id_plus1(self, nal: &[u8]) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::H265 => nal.get(1).map_or(1, |byte| (byte & 0x07).max(1)),
        }
    }

    /// Whether a NAL is a prefix SEI, which is the one an SEI message
    /// of a writer's own may sit in.
    pub fn is_prefix_sei(self, nal: &[u8]) -> bool {
        match self.nal_type(nal) {
            Some(kind) => match self {
                Codec::H264 => kind == H264_SEI,
                Codec::H265 => kind == H265_PREFIX_SEI,
            },
            None => false,
        }
    }
}

/// One access unit of an Annex B stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    /// Where the access unit starts: at the first byte the picture's
    /// own NALs need, which is any zero padding before its first start
    /// code and then that start code.
    pub start: usize,
    /// Where it ends, at the same point in the next access unit, so the
    /// units tile the stream with nothing left over.
    pub end: usize,
    /// Where an SEI NAL goes: at the start code of the first coded
    /// slice, which is after the delimiter, the parameter sets, any SEI
    /// already there, and any padding, so a splice here leaves every
    /// byte of the stream where it was.
    pub insert_at: usize,
    /// Whether the access unit is a random access point.
    pub keyframe: bool,
    /// The `nuh_temporal_id_plus1` of its first coded slice, which an
    /// SEI put in this access unit has to repeat.
    pub temporal_id_plus1: u8,
}

/// The access units of an Annex B stream.
///
/// The boundary rule is the one a writer needs and no more: a new
/// access unit begins at a slice that says it is the first of a
/// picture, and at any parameter set, delimiter or SEI that turns up
/// after a slice. That is enough to place an SEI correctly in every
/// stream an encoder writes, which is what section 7.4.1.2 asks for.
pub fn access_units(annexb: &[u8], codec: Codec) -> Vec<AccessUnit> {
    let nals = scan_nals(annexb);
    let mut out: Vec<AccessUnit> = Vec::new();
    let mut seen_slice = false;
    for nal in &nals {
        let Some(kind) = codec.nal_type(nal.bytes) else {
            continue;
        };
        let slice = codec.is_vcl_type(kind);
        let boundary = if slice {
            seen_slice && first_slice_of_picture(codec, nal.bytes)
        } else {
            seen_slice && codec.starts_access_unit(kind)
        };
        if out.is_empty() || boundary {
            if let Some(last) = out.last_mut() {
                last.end = nal.pad;
            }
            out.push(AccessUnit {
                start: nal.pad,
                end: annexb.len(),
                insert_at: usize::MAX,
                keyframe: false,
                temporal_id_plus1: 1,
            });
            seen_slice = false;
        }
        if let Some(unit) = out.last_mut() {
            if slice {
                if unit.insert_at == usize::MAX {
                    unit.insert_at = nal.code;
                    unit.temporal_id_plus1 = codec.temporal_id_plus1(nal.bytes);
                }
                unit.keyframe |= codec.is_keyframe_type(kind);
                seen_slice = true;
            }
        }
    }
    // An access unit with no slice at all takes its own end as the
    // place to insert, which is where a slice would have gone.
    for unit in out.iter_mut() {
        if unit.insert_at == usize::MAX {
            unit.insert_at = unit.end;
        }
    }
    out
}

/// Whether a slice NAL says it opens a picture.
///
/// `first_mb_in_slice` in H.264 and `first_slice_segment_in_pic_flag`
/// in HEVC are both the first bit after the header, and both are one
/// for the first slice: an exp-Golomb zero is a single set bit. No
/// emulation prevention byte can land that early, because the header
/// byte before it is never zero.
pub fn first_slice_of_picture(codec: Codec, nal: &[u8]) -> bool {
    match nal.get(codec.header_len()) {
        Some(byte) => byte & 0x80 != 0,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::{split_nals, START_CODE};

    /// A short Annex B stream shaped like what x264 writes: parameter
    /// sets, an encoder SEI, an IDR, then two more pictures.
    fn a_stream() -> Vec<u8> {
        let mut out = Vec::new();
        let mut nal = |payload: &[u8], four: bool| {
            out.extend_from_slice(if four { &START_CODE[..] } else { &[0, 0, 1] });
            out.extend_from_slice(payload);
        };
        nal(&[0x67, 0x64, 0x00, 0x1f, 0xac], true); // SPS
        nal(&[0x68, 0xeb, 0xec, 0xb2], true); // PPS
        nal(&[0x06, 0x05, 0x02, 0xaa, 0xbb, 0x80], false); // an SEI
        nal(&[0x65, 0x88, 0x84, 0x00], false); // IDR slice
        nal(&[0x41, 0x9a, 0x01], false); // a P slice
        nal(&[0x41, 0x9a, 0x02], false); // another picture
        out
    }

    #[test]
    fn a_header_says_the_type_in_either_spelling() {
        assert_eq!(Codec::H264.nal_type(&[0x65]), Some(5));
        assert_eq!(Codec::H264.nal_type(&[]), None);
        assert_eq!(Codec::H265.nal_type(&[0x26, 0x01]), Some(19));
        assert_eq!(Codec::H265.nal_type(&[0x26]), None, "the header is 2 bytes");
        assert!(Codec::H264.is_vcl(&[0x65]));
        assert!(!Codec::H264.is_vcl(&[0x67]));
        assert!(Codec::H264.is_keyframe_type(5));
        assert!(Codec::H265.is_keyframe_type(19));
        assert!(!Codec::H265.is_keyframe_type(1));
        assert_eq!(Codec::H264.temporal_id_plus1(&[0x65, 0x88]), 1);
        assert_eq!(Codec::H265.temporal_id_plus1(&[0x02, 0x03]), 3);
        assert_eq!(Codec::H265.temporal_id_plus1(&[0x02]), 1);
        assert!(Codec::H264.is_prefix_sei(&[0x06]));
        assert!(Codec::H265.is_prefix_sei(&[H265_PREFIX_SEI << 1, 1]));
        assert!(!Codec::H265.is_prefix_sei(&[H265_SUFFIX_SEI << 1, 1]));
    }

    #[test]
    fn access_units_cut_where_the_pictures_do() {
        let stream = a_stream();
        let units = access_units(&stream, Codec::H264);
        assert_eq!(units.len(), 3, "three pictures");
        assert!(units[0].keyframe, "the first is an IDR");
        assert!(!units[1].keyframe);
        assert_eq!(units[0].start, 0);
        assert_eq!(units.last().expect("a unit").end, stream.len());
        for pair in units.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "the units tile the stream");
        }
        // The first access unit inserts before its IDR slice, which is
        // after the parameter sets and after the encoder's own SEI.
        let nals = crate::annexb::scan_nals(&stream);
        let idr = nals
            .iter()
            .find(|nal| Codec::H264.nal_type(nal.bytes) == Some(5))
            .expect("an IDR");
        assert_eq!(units[0].insert_at, idr.code);
    }

    #[test]
    fn a_slice_that_does_not_open_a_picture_stays_in_its_access_unit() {
        // Two slices of one picture: the second says first_mb_in_slice
        // is not zero, so its leading bit is clear.
        let mut stream = Vec::new();
        stream.extend_from_slice(&START_CODE);
        stream.extend_from_slice(&[0x65, 0x88, 0x84, 0x00]);
        stream.extend_from_slice(&START_CODE);
        stream.extend_from_slice(&[0x65, 0x11, 0x84, 0x00]);
        assert_eq!(access_units(&stream, Codec::H264).len(), 1);
        assert!(first_slice_of_picture(Codec::H264, &[0x65, 0x88]));
        assert!(!first_slice_of_picture(Codec::H264, &[0x65, 0x11]));
        assert!(!first_slice_of_picture(Codec::H264, &[0x65]));
    }

    #[test]
    fn an_hevc_access_unit_carries_its_temporal_id() {
        let mut stream = Vec::new();
        for (kind, temporal_id_plus1) in [(19u8, 1u8), (1, 3)] {
            stream.extend_from_slice(&START_CODE);
            stream.extend_from_slice(&[kind << 1, temporal_id_plus1, 0x80, 0x00]);
        }
        let units = access_units(&stream, Codec::H265);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].temporal_id_plus1, 1);
        assert_eq!(units[1].temporal_id_plus1, 3);
        assert!(units[0].keyframe, "type 19 is an IDR");
        assert_eq!(
            Codec::H265.sei_header(3),
            vec![H265_PREFIX_SEI << 1, 3],
            "an SEI repeats the sub-layer"
        );
        assert_eq!(Codec::H265.sei_header(0), vec![H265_PREFIX_SEI << 1, 1]);
        assert_eq!(Codec::H264.sei_header(3), vec![H264_SEI]);
    }

    #[test]
    fn a_stream_of_no_slices_is_still_one_access_unit() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&START_CODE);
        stream.extend_from_slice(&[0x67, 0x64, 0x00]);
        let units = access_units(&stream, Codec::H264);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].insert_at, units[0].end);
        assert!(access_units(&[], Codec::H264).is_empty());
        assert!(split_nals(&[]).is_empty());
    }
}
