//! The bits inside a sequence parameter set: the exp-Golomb reader of
//! ISO/IEC 14496-10 section 9.1, and the few fields an `avcC` record
//! repeats out of the SPS, section 7.3.2.1.
//!
//! Only those fields are read. Resolution, the VUI, the HRD and the AV1
//! sequence header are not parsed here, because nothing on this machine
//! needs them yet: the geometry a muxer writes comes from the stream's
//! own header out of band, which is where ffmpeg already put it.

use crate::ep::remove_emulation_prevention;
use crate::h26x::{Codec, H264_SPS};

/// A most-significant-bit-first reader over a byte slice, which is how
/// every field of a parameter set is spelled.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader at the first bit of `data`, which is an RBSP: the
    /// escapes are expected to be off already.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The next bit, or `None` at the end of the bytes.
    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    /// Steps `n` bits on, or `None` when there are not that many left.
    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// One unsigned exp-Golomb value, `ue(v)`: the zeroes say how many
    /// bits follow the set bit.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut value = 0u32;
        for _ in 0..zeros {
            value = (value << 1) | self.bit()?;
        }
        Some((1u32 << zeros) - 1 + value)
    }
}

/// The formats an `avcC` extension carries, read out of an SPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Formats {
    pub chroma_format_idc: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
}

/// Reads chroma format and bit depths from an H.264 SPS NAL, header
/// byte and escapes included. `None` when the SPS runs out before they
/// are read.
pub fn formats(nal: &[u8]) -> Option<Formats> {
    let rbsp = remove_emulation_prevention(nal.get(1..)?);
    let mut bits = BitReader::new(&rbsp);
    bits.skip(24)?; // profile_idc, constraint flags, level_idc
    bits.ue()?; // seq_parameter_set_id
    let chroma_format_idc = bits.ue()?;
    if chroma_format_idc == 3 {
        bits.skip(1)?; // separate_colour_plane_flag
    }
    let bit_depth_luma_minus8 = bits.ue()?;
    let bit_depth_chroma_minus8 = bits.ue()?;
    Some(Formats {
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth_luma_minus8: bit_depth_luma_minus8 as u8,
        bit_depth_chroma_minus8: bit_depth_chroma_minus8 as u8,
    })
}

/// The H.264 profile and level of a stream, from its out-of-band header
/// in either spelling.
///
/// They are bytes 1 and 3 of the SPS payload, the same two an `avcC`
/// copies into its own header. Annex B extradata carries the SPS behind
/// a start code; a record carries them at the same offsets behind its
/// configuration version of 1, which no Annex B stream begins with.
pub fn profile_level(extradata: &[u8]) -> Option<(u8, u8)> {
    if extradata.first() == Some(&1) {
        return match extradata {
            [_, profile, _, level, ..] => Some((*profile, *level)),
            _ => None,
        };
    }
    crate::annexb::scan_nals(extradata)
        .iter()
        .find(|nal| Codec::H264.nal_type(nal.bytes) == Some(H264_SPS))
        .and_then(|nal| match nal.bytes {
            [_, profile, _, level, ..] => Some((*profile, *level)),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A High profile SPS, as x264 writes one.
    const HIGH_SPS: [u8; 14] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x6a, 0x02, 0x02,
    ];

    #[test]
    // The digits are grouped as the codes are, not in fours.
    #[allow(clippy::unusual_byte_groupings)]
    fn exp_golomb_reads_the_first_values() {
        // 1 -> 0; 010 -> 1; 011 -> 2; 00100 -> 3.
        let mut bits = BitReader::new(&[0b1_010_011_0, 0b0100_0000]);
        assert_eq!(bits.ue(), Some(0));
        assert_eq!(bits.ue(), Some(1));
        assert_eq!(bits.ue(), Some(2));
        assert_eq!(bits.ue(), Some(3));
    }

    #[test]
    fn a_reader_at_the_end_of_its_bytes_says_so() {
        let mut bits = BitReader::new(&[0x80]);
        assert_eq!(bits.bit(), Some(1));
        assert_eq!(bits.skip(7), Some(()));
        assert_eq!(bits.bit(), None);
        assert_eq!(bits.skip(1), None);
        // Thirty-two zero bits are not an exp-Golomb value.
        assert_eq!(BitReader::new(&[0; 8]).ue(), None);
        assert_eq!(BitReader::new(&[]).ue(), None);
    }

    #[test]
    fn a_high_profile_sps_gives_its_chroma_format_and_bit_depths() {
        assert_eq!(
            formats(&HIGH_SPS),
            Some(Formats {
                chroma_format_idc: 1,
                bit_depth_luma_minus8: 0,
                bit_depth_chroma_minus8: 0,
            })
        );
    }

    #[test]
    fn an_sps_that_runs_out_reads_nothing_and_panics_at_nothing() {
        for cut in 0..HIGH_SPS.len() {
            let _ = formats(&HIGH_SPS[..cut]);
        }
        assert_eq!(formats(&[]), None);
        assert_eq!(formats(&[0x67]), None);
        assert_eq!(formats(&[0x67, 0x64, 0x00]), None);
    }

    #[test]
    fn the_profile_and_level_read_from_either_spelling() {
        // SPS behind a 4-byte start code: profile 0x64 (High), level
        // 0x1f, then the PPS the extradata also carries.
        let extradata = [
            0u8, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xab, // SPS
            0, 0, 0, 1, 0x68, 0xee, // PPS
        ];
        assert_eq!(profile_level(&extradata), Some((0x64, 0x1f)));
        // A PPS first, behind a 3-byte code, does not confuse the scan.
        let pps_first = [0u8, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e];
        assert_eq!(profile_level(&pps_first), Some((0x42, 0x1e)));
        assert_eq!(profile_level(&HIGH_SPS), None, "an SPS with no start code");

        // An avcC says the same two bytes at the same two offsets.
        assert_eq!(
            profile_level(&[1, 0x64, 0x00, 0x1f, 0xff, 0xe1]),
            Some((0x64, 0x1f))
        );
        assert_eq!(profile_level(&[1, 0x64]), None);

        // And bytes that carry no SPS say nothing.
        assert_eq!(profile_level(&[]), None);
        assert_eq!(profile_level(&[0, 0, 0, 1, 0x68, 0xee]), None);
        assert_eq!(profile_level(&[0xab, 0xcd, 0xef]), None);
    }
}
