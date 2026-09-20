//! The two ways a coded stream frames its NAL units: a start code
//! before each one, which is ISO/IEC 14496-10 Annex B and what an
//! elementary stream and ffmpeg's own NUT carry, and a length before
//! each one, which is ISO/IEC 14496-15 section 5.3.4.1 and what MP4 and
//! its relatives store samples in.
//!
//! Reframing touches no NAL's own bytes. Which framing a stream is in is
//! [`crate::config::framing_of`], from the codec name and the
//! out-of-band header.

use crate::{Error, Result};

/// The four byte start code a writer here puts before a NAL.
///
/// Three byte codes are read and never written: four is what the
/// encoders these crates read write, and what makes the offsets of a
/// rewritten stream easy to reason about.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One NAL unit's place in an Annex B byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalRef<'a> {
    /// Where the bytes between the NAL unit before this one and this one
    /// begin: the zero padding, if the stream has any, and then the start
    /// code. The NAL before this one ends here, so `pad` to `end` tiles
    /// the byte stream.
    pub pad: usize,
    /// Where this NAL's own start code begins, its `zero_byte` included.
    /// Something spliced in before this NAL goes here, which leaves the
    /// padding trailing the NAL it trailed before.
    pub code: usize,
    /// Where the NAL's own bytes begin.
    pub start: usize,
    /// Where they end, which is before any zero byte that pads the way
    /// to the next start code.
    pub end: usize,
    /// The NAL, start code and padding removed.
    pub bytes: &'a [u8],
}

/// The NAL units of an Annex B byte stream, with their offsets.
///
/// Both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes cut.
/// Bytes before the first start code are ignored, as ffmpeg ignores
/// them, and bytes with no start code in them at all hold no NAL units.
///
/// Zero bytes between one NAL unit and the next start code belong to
/// the byte stream and not to either NAL unit: H.264 Annex B.1.1 and
/// HEVC Annex B.2.1 spell them `trailing_zero_8bits`, and a NAL unit
/// cannot end in a zero byte, since its last byte is always the one
/// carrying `rbsp_trailing_bits` or the escape of a `cabac_zero_word`.
/// So `end` excludes every such zero, and [`NalRef::pad`] carries the
/// offset they begin at. ffmpeg's H.264 demuxer writes one of them
/// between the parameter sets of its extradata, and the SPS read out of
/// that extradata is the SPS an `avcC` carries, byte for byte.
pub fn scan_nals(annexb: &[u8]) -> Vec<NalRef<'_>> {
    // (pad, code, start) of each NAL, in order.
    let mut codes: Vec<(usize, usize, usize)> = Vec::new();
    let mut at = 0usize;
    while at + 2 < annexb.len() {
        if annexb[at] == 0 && annexb[at + 1] == 0 && annexb[at + 2] == 1 {
            let mut pad = at;
            while pad > 0 && annexb[pad - 1] == 0 {
                pad -= 1;
            }
            // One zero of the run is this code's own `zero_byte`; the
            // rest padded the NAL before it.
            let code = if at > pad { at - 1 } else { at };
            codes.push((pad, code, at + 3));
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut out = Vec::with_capacity(codes.len());
    for (index, (pad, code, start)) in codes.iter().enumerate() {
        let end = match codes.get(index + 1) {
            Some((next, _, _)) => (*next).max(*start),
            None => trimmed_end(annexb, *start),
        };
        out.push(NalRef {
            pad: *pad,
            code: *code,
            start: *start,
            end,
            bytes: &annexb[*start..end],
        });
    }
    out
}

/// Where the last NAL unit of a byte stream ends: before the zeroes
/// that pad the stream out, which belong to it no more than the ones
/// before a start code do.
fn trimmed_end(annexb: &[u8], start: usize) -> usize {
    let mut end = annexb.len();
    while end > start && annexb[end - 1] == 0 {
        end -= 1;
    }
    end
}

/// The NAL units of an Annex B byte stream, start codes removed.
pub fn split_nals(annexb: &[u8]) -> Vec<&[u8]> {
    scan_nals(annexb).into_iter().map(|nal| nal.bytes).collect()
}

/// One Annex B access unit reframed with length prefixes of
/// `length_size` bytes, the framing an `avcC` or `hvcC` declares.
///
/// Empty when the bytes hold no start code. [`Error::TooLarge`] when a
/// NAL does not fit the prefix it was given.
pub fn annexb_to_length_prefixed(annexb: &[u8], length_size: usize) -> Result<Vec<u8>> {
    check_length_size(length_size)?;
    let mut out = Vec::with_capacity(annexb.len());
    for nal in scan_nals(annexb) {
        put_length(&mut out, nal.bytes.len(), length_size, nal.start)?;
        out.extend_from_slice(nal.bytes);
    }
    Ok(out)
}

/// One length-prefixed sample back as Annex B, 4-byte start codes.
pub fn length_prefixed_to_annexb(sample: &[u8], length_size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(sample.len() + 8);
    for nal in split_length_prefixed(sample, length_size)? {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    Ok(out)
}

/// The NAL units of a length-prefixed sample.
pub fn split_length_prefixed(sample: &[u8], length_size: usize) -> Result<Vec<&[u8]>> {
    check_length_size(length_size)?;
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < sample.len() {
        let header = sample.get(at..at + length_size).ok_or(Error::Malformed {
            what: "the sample ends inside a NAL length",
            at,
        })?;
        let length = header
            .iter()
            .fold(0usize, |value, byte| (value << 8) | usize::from(*byte));
        at += length_size;
        let nal = sample.get(at..at + length).ok_or(Error::Malformed {
            what: "a NAL overruns the sample",
            at,
        })?;
        at += length;
        out.push(nal);
    }
    Ok(out)
}

/// Whether a length prefix of `length_size` bytes is one this crate
/// writes and reads: 1 to 4, which is every width the records can
/// declare.
pub(crate) fn check_length_size(length_size: usize) -> Result<()> {
    if !(1..=4).contains(&length_size) {
        return Err(Error::Malformed {
            what: "a NAL length prefix is 1 to 4 bytes",
            at: 0,
        });
    }
    Ok(())
}

/// Appends one big-endian length of `length_size` bytes.
pub(crate) fn put_length(
    out: &mut Vec<u8>,
    length: usize,
    length_size: usize,
    at: usize,
) -> Result<()> {
    if length_size < 4 && length >= 1 << (length_size * 8) {
        return Err(Error::TooLarge { at });
    }
    for shift in (0..length_size).rev() {
        out.push((length >> (shift * 8)) as u8);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_codes_of_both_lengths_cut() {
        let annexb = [
            0, 0, 0, 1, 0x67, 0xaa, // 4-byte code, SPS-ish
            0, 0, 1, 0x68, 0xbb, // 3-byte code
            0, 0, 0, 1, 0x65, 0xcc, 0xdd, // 4-byte code again
        ];
        let nals = split_nals(&annexb);
        assert_eq!(
            nals,
            vec![&[0x67, 0xaa][..], &[0x68, 0xbb], &[0x65, 0xcc, 0xdd]]
        );
        // Every NAL knows where its code began, which is where an
        // insertion before it goes.
        let refs = scan_nals(&annexb);
        assert_eq!(
            refs.iter().map(|nal| nal.code).collect::<Vec<_>>(),
            vec![0, 6, 11]
        );
        assert_eq!(
            refs.iter().map(|nal| nal.start).collect::<Vec<_>>(),
            vec![4, 9, 15]
        );
        // With no padding anywhere, the padding offset is the code.
        assert_eq!(
            refs.iter().map(|nal| nal.pad).collect::<Vec<_>>(),
            vec![0, 6, 11]
        );
        assert_eq!(refs.last().expect("a NAL").end, annexb.len());
    }

    #[test]
    fn bytes_with_no_start_code_hold_no_nals() {
        assert!(scan_nals(&[]).is_empty());
        assert!(scan_nals(&[1, 2, 3, 4]).is_empty());
        assert!(scan_nals(&[0, 0, 0, 0]).is_empty());
        assert!(split_nals(&[0x67, 0x64, 0x00, 0x1f]).is_empty());
        // And a packet of them reframes to nothing rather than refusing.
        assert!(annexb_to_length_prefixed(&[1, 2, 3, 4], 4)
            .expect("a sample")
            .is_empty());
    }

    #[test]
    fn bytes_before_the_first_start_code_are_ignored() {
        let annexb = [0xde, 0xad, 0, 0, 0, 1, 0x65, 0x88];
        assert_eq!(split_nals(&annexb), vec![&[0x65, 0x88][..]]);
        assert_eq!(scan_nals(&annexb)[0].code, 2);
    }

    #[test]
    fn the_zeroes_before_a_start_code_belong_to_neither_nal() {
        // One zero is the four byte code's own; a second pads the way
        // to it, which is what ffmpeg writes between the parameter sets
        // of its H.264 extradata. Neither is part of a NAL unit.
        let annexb = [0, 0, 0, 1, 0x67, 0xaa, 0, 0, 0, 0, 1, 0x68, 0xbb];
        assert_eq!(split_nals(&annexb), vec![&[0x67, 0xaa][..], &[0x68, 0xbb]]);
        let refs = scan_nals(&annexb);
        // The padding is where the second NAL's bytes begin, and the
        // start code is where a splice before it goes: the byte between
        // the two is the one that padded the SPS.
        assert_eq!((refs[0].start, refs[0].end), (4, 6));
        assert_eq!((refs[1].pad, refs[1].code, refs[1].start), (6, 7, 11));
        assert_eq!(refs[0].end, refs[1].pad, "the NALs tile the stream");

        // However many zeroes there are, and wherever they fall.
        let mut padded = vec![0, 0, 0, 1, 0x67, 0xaa];
        padded.extend_from_slice(&[0; 6]);
        padded.extend_from_slice(&[0, 0, 1, 0x68, 0xbb]);
        padded.extend_from_slice(&[0; 3]);
        assert_eq!(split_nals(&padded), vec![&[0x67, 0xaa][..], &[0x68, 0xbb]]);
        let refs = scan_nals(&padded);
        assert_eq!((refs[0].end, refs[1].pad, refs[1].code), (6, 6, 11));
        assert_eq!(
            refs[1].end,
            padded.len() - 3,
            "the stream's own trailing zeroes are not the NAL's"
        );
    }

    #[test]
    fn padding_survives_a_reframe_only_as_padding() {
        // The one thing reframing drops: the zeroes between NALs, which
        // no length-prefixed sample has anywhere to put. Every other
        // byte comes back.
        let mut padded = vec![0, 0, 0, 1, 0x67, 0xaa, 0];
        padded.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xbb, 0, 0]);
        let clean: Vec<u8> = [
            &START_CODE[..],
            &[0x67, 0xaa],
            &START_CODE[..],
            &[0x68, 0xbb],
        ]
        .concat();
        for length_size in 1..=4usize {
            let framed = annexb_to_length_prefixed(&padded, length_size).expect("a sample");
            assert_eq!(
                framed,
                annexb_to_length_prefixed(&clean, length_size).expect("a sample"),
                "the padding reached the sample"
            );
            let back = length_prefixed_to_annexb(&framed, length_size).expect("annex b");
            assert_eq!(back, clean);
            assert_eq!(split_nals(&back), split_nals(&padded));
        }
    }

    #[test]
    fn length_prefixing_puts_each_nals_own_length_in_front() {
        let annexb = [0, 0, 1, 0x06, 0x05, 0, 0, 0, 1, 0x65, 0x88];
        assert_eq!(
            annexb_to_length_prefixed(&annexb, 4).expect("a sample"),
            vec![0, 0, 0, 2, 0x06, 0x05, 0, 0, 0, 2, 0x65, 0x88]
        );
        let annexb: &[u8] = &[0, 0, 0, 1, 0x65, 1, 2, 3, 0, 0, 0, 1, 0x41, 9];
        let framed = annexb_to_length_prefixed(annexb, 4).expect("a sample");
        assert_eq!(framed, vec![0, 0, 0, 4, 0x65, 1, 2, 3, 0, 0, 0, 2, 0x41, 9]);
        assert_eq!(
            length_prefixed_to_annexb(&framed, 4).expect("annex b"),
            annexb
        );
    }

    #[test]
    fn the_two_framings_round_trip_at_every_width() {
        let mut stream = Vec::new();
        for nal in [
            vec![0x67u8, 0x64, 0x00, 0x1f, 0xac],
            vec![0x68, 0xeb, 0xec, 0xb2],
            vec![0x65, 0x88, 0x84, 0x00],
        ] {
            stream.extend_from_slice(&START_CODE);
            stream.extend_from_slice(&nal);
        }
        for length_size in 1..=4usize {
            let framed = annexb_to_length_prefixed(&stream, length_size).expect("framed");
            let back = length_prefixed_to_annexb(&framed, length_size).expect("annex b");
            assert_eq!(split_nals(&back), split_nals(&stream));
        }
        assert!(annexb_to_length_prefixed(&stream, 0).is_err());
        assert!(annexb_to_length_prefixed(&stream, 5).is_err());
        assert!(split_length_prefixed(&stream, 5).is_err());
    }

    #[test]
    fn a_nal_too_long_for_its_prefix_is_refused() {
        let mut annexb = vec![0, 0, 0, 1, 0x65];
        annexb.extend(std::iter::repeat_n(0x5a, 300));
        assert_eq!(
            annexb_to_length_prefixed(&annexb, 1),
            Err(Error::TooLarge { at: 4 })
        );
        assert!(annexb_to_length_prefixed(&annexb, 2).is_ok());
    }

    #[test]
    fn a_nal_running_past_the_sample_is_refused_with_its_offset() {
        assert_eq!(
            split_length_prefixed(&[0, 0, 0, 9, 1, 2], 4),
            Err(Error::Malformed {
                what: "a NAL overruns the sample",
                at: 4
            })
        );
        assert_eq!(
            split_length_prefixed(&[0, 0, 0], 4),
            Err(Error::Malformed {
                what: "the sample ends inside a NAL length",
                at: 0
            })
        );
    }

    #[test]
    fn a_truncated_sample_is_refused_not_panicked() {
        let mut stream = Vec::new();
        for nal in [vec![0x67u8, 0x64, 0x00], vec![0x65, 0x88, 0x84, 0x00]] {
            stream.extend_from_slice(&START_CODE);
            stream.extend_from_slice(&nal);
        }
        let sample = annexb_to_length_prefixed(&stream, 4).expect("a sample");
        for cut in 1..sample.len() {
            let _ = split_length_prefixed(&sample[..cut], 4);
            let _ = length_prefixed_to_annexb(&sample[..cut], 4);
        }
        assert!(split_length_prefixed(&sample[..3], 4).is_err());
    }
}
