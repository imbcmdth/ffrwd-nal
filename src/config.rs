//! The decoder configuration records, ISO/IEC 14496-15 sections 5.3.3
//! (`AVCDecoderConfigurationRecord`, the `avcC`) and 8.3.3 (the
//! `hvcC`), and the framing they declare.
//!
//! A record is a byte string here, not a box: which box carries it and
//! where that box sits is the container's business. What this module
//! does is read the two things a NAL reader needs out of a record - the
//! width of the length prefix and the parameter sets - and build the
//! H.264 record back out of parameter sets the way ffmpeg's own writer
//! does.
//!
//! [`Framing`] is the question a packet filter asks first: are these
//! packets start-coded or length-prefixed, and how wide is the length.

use crate::annexb::{annexb_to_length_prefixed, split_length_prefixed, split_nals, START_CODE};
use crate::h26x::{access_units, Codec, H264_PPS, H264_SPS};
use crate::sei::{
    insert_sei_annexb, insert_sei_length_prefixed, user_data_annexb, user_data_in_nal,
    user_data_length_prefixed, write_user_data_at,
};
use crate::sps::formats;
use crate::{obu, Error, Result, Select};

/// The NAL length prefix an `avcC` declares, in bytes: its
/// `lengthSizeMinusOne` plus one. Four for everything ffmpeg writes; a
/// record too short to say is read as four.
pub fn avcc_length_size(avcc: &[u8]) -> usize {
    match avcc.get(4) {
        Some(byte) => usize::from(byte & 0x3) + 1,
        None => 4,
    }
}

/// The NAL length prefix an `hvcC` declares, in bytes. The field sits
/// at byte 21, past the profile, tier and level the record opens with;
/// a record too short to say is read as four.
pub fn hvcc_length_size(hvcc: &[u8]) -> usize {
    match hvcc.get(21) {
        Some(byte) => usize::from(byte & 0x3) + 1,
        None => 4,
    }
}

/// The SPS and PPS NAL units of an Annex B extradata blob, in order.
pub fn parse_parameter_sets(annexb: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for nal in split_nals(annexb) {
        match nal.first().map(|byte| byte & 0x1f) {
            Some(H264_SPS) => sps.push(nal.to_vec()),
            Some(H264_PPS) => pps.push(nal.to_vec()),
            _ => {}
        }
    }
    (sps, pps)
}

/// Builds an `avcC` record from SPS and PPS NAL units, 4-byte lengths
/// declared.
///
/// For the high profiles the record carries chroma format and bit
/// depths read out of the first SPS, the way ffmpeg's own writer does;
/// the baseline family leaves them off.
pub fn build_avcc(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Result<Vec<u8>> {
    let first = sps
        .first()
        .filter(|set| set.len() >= 4)
        .ok_or(Error::Malformed {
            what: "the extradata carries no SPS",
            at: 0,
        })?;
    if pps.is_empty() {
        return Err(Error::Malformed {
            what: "the extradata carries no PPS",
            at: 0,
        });
    }
    if sps.len() > 31 {
        return Err(Error::TooLarge { at: 0 });
    }
    if pps.len() > 255 {
        return Err(Error::TooLarge { at: 0 });
    }
    if let Some(set) = sps.iter().chain(pps).find(|set| set.len() > 0xffff) {
        return Err(Error::TooLarge { at: set.len() });
    }

    let mut avcc = Vec::new();
    avcc.push(1); // configurationVersion
    avcc.extend_from_slice(&first[1..4]); // profile, compat, level
    avcc.push(0xfc | 3); // lengthSizeMinusOne = 3
    avcc.push(0xe0 | sps.len() as u8);
    for set in sps {
        avcc.extend_from_slice(&(set.len() as u16).to_be_bytes());
        avcc.extend_from_slice(set);
    }
    avcc.push(pps.len() as u8);
    for set in pps {
        avcc.extend_from_slice(&(set.len() as u16).to_be_bytes());
        avcc.extend_from_slice(set);
    }

    // The extension bytes, for profiles outside the baseline family.
    let profile = first[1];
    if !matches!(profile, 66 | 77 | 88) {
        let parsed = formats(first).ok_or(Error::Malformed {
            what: "an SPS of a high profile too short to read its formats",
            at: 0,
        })?;
        avcc.push(0xfc | (parsed.chroma_format_idc & 0x3));
        avcc.push(0xf8 | (parsed.bit_depth_luma_minus8 & 0x7));
        avcc.push(0xf8 | (parsed.bit_depth_chroma_minus8 & 0x7));
        avcc.push(0); // numOfSequenceParameterSetExt
    }
    Ok(avcc)
}

/// The SPS and PPS an `avcC` record carries, back as the Annex B
/// extradata a coded stream declares: each NAL behind a 4-byte start
/// code, the sets in the order the record listed them.
///
/// The inverse of [`build_avcc`]; what a source reading an MP4 init
/// segment hands an encoder's edge that expects Annex B.
pub fn avcc_to_annexb_extradata(avcc: &[u8]) -> Result<Vec<u8>> {
    if avcc.len() < 6 {
        return Err(Error::Malformed {
            what: "an avcC too short to name a parameter set",
            at: 0,
        });
    }
    let mut out = Vec::with_capacity(avcc.len());
    let mut at = 6usize;
    let mut counts = [usize::from(avcc[5] & 0x1f), 0];
    for set in 0..2 {
        if set == 1 {
            let count = *avcc.get(at).ok_or(Error::Truncated { at })?;
            counts[1] = usize::from(count);
            at += 1;
        }
        for _ in 0..counts[set] {
            let length = avcc
                .get(at..at + 2)
                .map(|pair| usize::from(u16::from_be_bytes([pair[0], pair[1]])))
                .ok_or(Error::Truncated { at })?;
            at += 2;
            let nal = avcc.get(at..at + length).ok_or(Error::Truncated { at })?;
            at += length;
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
    }
    if out.is_empty() {
        return Err(Error::Malformed {
            what: "an avcC that carries no parameter set",
            at: 0,
        });
    }
    Ok(out)
}

/// How one stream's packets are framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Start codes, which is what an elementary stream and ffmpeg's own
    /// NUT carry.
    AnnexB(Codec),
    /// A length before every NAL, as an `avcC` or `hvcC` declares.
    LengthPrefixed { codec: Codec, length_size: usize },
    /// Low-overhead OBUs: no lengths outside the OBUs' own.
    Av1,
}

/// The codecs this crate frames, ffmpeg's names for them, most
/// preferred first.
pub const CODECS: &[&str] = &["h264", "hevc", "av1"];

/// What a stream carries, from the codec name and the out-of-band
/// header.
///
/// An `avcC` or `hvcC` opens with a configuration version of 1; an
/// Annex B header opens with a start code, whose first byte is zero,
/// and a stream that carries no header at all is Annex B too. That is
/// the same test ffmpeg makes of the same bytes. A name this crate has
/// no framing for is [`Error::UnknownCodec`], and the caller says so in
/// its own words.
pub fn framing_of(codec: &str, extradata: &[u8]) -> Result<Framing> {
    match codec {
        "h264" | "avc1" => Ok(match configuration_record(extradata, 7) {
            true => Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: avcc_length_size(extradata),
            },
            false => Framing::AnnexB(Codec::H264),
        }),
        "hevc" | "h265" | "hvc1" | "hev1" => Ok(match configuration_record(extradata, 23) {
            true => Framing::LengthPrefixed {
                codec: Codec::H265,
                length_size: hvcc_length_size(extradata),
            },
            false => Framing::AnnexB(Codec::H265),
        }),
        "av1" => Ok(Framing::Av1),
        _ => Err(Error::UnknownCodec),
    }
}

fn configuration_record(extradata: &[u8], least: usize) -> bool {
    extradata.len() >= least && extradata[0] == 1
}

impl Framing {
    /// One packet with `payload` put where the standards say it goes:
    /// before the first coded slice for H.264 and HEVC, before the
    /// frame header for AV1. Everything else in the packet is copied
    /// through exactly as the encoder wrote it.
    ///
    /// The payload opens with the writer's own UUID; `select` says
    /// which `metadata_type` the AV1 spelling uses, and its UUID is not
    /// read here because the payload already carries it.
    pub fn insert(self, packet: &[u8], payload: &[u8], select: Select) -> Result<Vec<u8>> {
        match self {
            Framing::AnnexB(codec) => {
                let temporal_id_plus1 = access_units(packet, codec)
                    .first()
                    .map_or(1, |unit| unit.temporal_id_plus1);
                let sei = write_user_data_at(payload, codec, temporal_id_plus1);
                insert_sei_annexb(packet, &sei, codec)
            }
            Framing::LengthPrefixed { codec, length_size } => {
                let nals = split_length_prefixed(packet, length_size)?;
                let temporal_id_plus1 = nals
                    .iter()
                    .find(|nal| codec.is_vcl(nal))
                    .map_or(1, |nal| codec.temporal_id_plus1(nal));
                let sei = write_user_data_at(payload, codec, temporal_id_plus1);
                insert_sei_length_prefixed(packet, &sei, length_size, codec)
            }
            Framing::Av1 => {
                let metadata = obu::write_metadata(select.metadata_type, payload);
                obu::insert_metadata_obu(packet, &metadata)
            }
        }
    }

    /// Every payload of `select` in one packet, in the order it carries
    /// them.
    ///
    /// A packet this reader cannot parse answers none rather than
    /// stopping the read: one broken access unit in a stream is not a
    /// reason to lose the rest.
    pub fn payloads(self, packet: &[u8], select: Select) -> Vec<Vec<u8>> {
        match self {
            Framing::AnnexB(codec) => user_data_annexb(packet, codec, &select.uuid),
            Framing::LengthPrefixed { codec, length_size } => {
                user_data_length_prefixed(packet, length_size, codec, &select.uuid)
                    .unwrap_or_default()
            }
            Framing::Av1 => obu::user_data(packet, select.metadata_type, &select.uuid),
        }
    }

    /// The same for bytes already cut to one NAL, which is what a
    /// container scanner holds when it has read a sample's first NAL
    /// and no more. AV1 reads the whole bytes, having no such cut.
    pub fn payloads_in_nal(self, nal: &[u8], select: Select) -> Vec<Vec<u8>> {
        match self {
            Framing::AnnexB(codec) | Framing::LengthPrefixed { codec, .. } => {
                user_data_in_nal(nal, codec, &select.uuid)
            }
            Framing::Av1 => obu::user_data(nal, select.metadata_type, &select.uuid),
        }
    }

    /// One Annex B access unit reframed the way this framing spells a
    /// packet. Annex B and AV1 hand the bytes back as they are.
    pub fn reframe(self, annexb: &[u8]) -> Result<Vec<u8>> {
        match self {
            Framing::LengthPrefixed { length_size, .. } => {
                annexb_to_length_prefixed(annexb, length_size)
            }
            _ => Ok(annexb.to_vec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: [u8; 16] = [
        0x04, 0x1f, 0x74, 0xa3, 0x80, 0x90, 0x5e, 0x08, 0xbc, 0xfc, 0x76, 0x4d, 0xf2, 0xdc, 0xd4,
        0x66,
    ];

    fn select() -> Select {
        Select::new(UUID, 25)
    }

    fn payload() -> Vec<u8> {
        let mut payload = UUID.to_vec();
        payload.extend_from_slice(b"a record of somebody's own");
        payload
    }

    /// A one-slice H.264 access unit in Annex B: an SPS, a PPS and an
    /// IDR slice whose first bit says it opens the picture.
    fn annexb_h264() -> Vec<u8> {
        let mut out = Vec::new();
        for nal in [
            vec![0x67u8, 0x42, 0x00, 0x0a, 0x96],
            vec![0x68, 0xce, 0x3c, 0x80],
            vec![0x65, 0x88, 0x84, 0x00, 0x21],
        ] {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(&nal);
        }
        out
    }

    #[test]
    fn extradata_round_trips_through_the_avcc() {
        // The parameter sets go in Annex B, come back out Annex B, and
        // the record between them declares 4-byte NAL lengths.
        let annexb: &[u8] = &[
            0, 0, 0, 1, 0x67, 66, 0xc0, 30, 0xab, 0xcd, // SPS
            0, 0, 0, 1, 0x68, 0xee, 0x06, 0xf2, // PPS
        ];
        let (sps, pps) = parse_parameter_sets(annexb);
        let avcc = build_avcc(&sps, &pps).expect("an avcC");
        assert_eq!(avcc_length_size(&avcc), 4);
        assert_eq!(avcc_to_annexb_extradata(&avcc).expect("extradata"), annexb);
    }

    #[test]
    fn a_high_profile_record_keeps_its_sets_past_the_extension_bytes() {
        // A High-profile avcC carries three extension bytes after the
        // PPS; the sets read back from before them either way.
        let annexb: &[u8] = &[
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0, 0, 0, 1,
            0x68, 0xeb, 0xec, 0xb2, 0x2c,
        ];
        let (sps, pps) = parse_parameter_sets(annexb);
        let avcc = build_avcc(&sps, &pps).expect("an avcC");
        assert_eq!(avcc_to_annexb_extradata(&avcc).expect("extradata"), annexb);
        assert_eq!(&avcc[avcc.len() - 4..], &[0xfd, 0xf8, 0xf8, 0x00]);
    }

    #[test]
    fn a_record_that_names_no_set_is_refused() {
        assert_eq!(
            avcc_to_annexb_extradata(&[1, 0x64, 0, 0x1f]),
            Err(Error::Malformed {
                what: "an avcC too short to name a parameter set",
                at: 0
            })
        );
        assert_eq!(
            avcc_to_annexb_extradata(&[1, 0x64, 0, 0x1f, 0xff, 0xe0, 0x00]),
            Err(Error::Malformed {
                what: "an avcC that carries no parameter set",
                at: 0
            })
        );
        assert!(build_avcc(&[], &[vec![0x68, 0xee]]).is_err());
        assert!(build_avcc(&[vec![0x67, 66, 0xc0, 30]], &[]).is_err());
        assert!(build_avcc(&[vec![0x67, 66]], &[vec![0x68]]).is_err());
        // More sets than the record's counts can spell.
        assert_eq!(
            build_avcc(&vec![vec![0x67, 66, 0xc0, 30]; 32], &[vec![0x68]]),
            Err(Error::TooLarge { at: 0 })
        );
        assert_eq!(
            build_avcc(&[vec![0x67, 66, 0xc0, 30]], &vec![vec![0x68]; 256]),
            Err(Error::TooLarge { at: 0 })
        );
        // And a set longer than its own length field.
        let huge = vec![0x67; 0x1_0000];
        assert_eq!(
            build_avcc(&[huge], &[vec![0x68]]),
            Err(Error::TooLarge { at: 0x1_0000 })
        );
    }

    #[test]
    fn a_truncated_record_is_refused_not_panicked() {
        let annexb: &[u8] = &[
            0, 0, 0, 1, 0x67, 66, 0xc0, 30, 0xab, 0xcd, 0, 0, 0, 1, 0x68, 0xee, 0x06, 0xf2,
        ];
        let (sps, pps) = parse_parameter_sets(annexb);
        let avcc = build_avcc(&sps, &pps).expect("an avcC");
        for cut in 0..avcc.len() {
            let _ = avcc_to_annexb_extradata(&avcc[..cut]);
            let _ = avcc_length_size(&avcc[..cut]);
            let _ = hvcc_length_size(&avcc[..cut]);
        }
        let mut seed = 0x51ed_2c93_7f10_ab44u64;
        for _ in 0..3000 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 48 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 33) as u8);
            }
            let _ = avcc_to_annexb_extradata(&bytes);
            let (sps, pps) = parse_parameter_sets(&bytes);
            let _ = build_avcc(&sps, &pps);
            let _ = framing_of("h264", &bytes);
            let _ = framing_of("hevc", &bytes);
        }
    }

    #[test]
    fn extradata_tells_the_framing_and_the_length_width() {
        assert_eq!(
            framing_of("h264", &[]).expect("a framing"),
            Framing::AnnexB(Codec::H264)
        );
        assert_eq!(
            framing_of("h264", &[0, 0, 0, 1, 0x67, 0x42, 0x00]).expect("a framing"),
            Framing::AnnexB(Codec::H264)
        );
        // An avcC: version, profile, compatibility, level, then the
        // length size less one in the low two bits.
        let avcc = [1u8, 0x42, 0x00, 0x0a, 0xff, 0xe1, 0x00];
        assert_eq!(
            framing_of("h264", &avcc).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 4
            }
        );
        let two_byte = [1u8, 0x42, 0x00, 0x0a, 0xfd, 0xe1, 0x00];
        assert_eq!(
            framing_of("h264", &two_byte).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 2
            }
        );
        // An hvcC is longer, and its length size sits at byte 21.
        let mut hvcc = vec![1u8; 23];
        hvcc[21] = 0xf3;
        assert_eq!(
            framing_of("hevc", &hvcc).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H265,
                length_size: 4
            }
        );
        assert_eq!(
            framing_of("av1", &[0x81, 0x05]).expect("a framing"),
            Framing::Av1
        );
        assert_eq!(framing_of("vp9", &[]), Err(Error::UnknownCodec));
        // A record too short to be one is read as Annex B, which is
        // what a stream with no header at all is.
        assert_eq!(
            framing_of("h264", &[1, 0x42, 0x00]).expect("a framing"),
            Framing::AnnexB(Codec::H264)
        );
    }

    #[test]
    fn the_defaults_on_a_short_record_are_four_bytes() {
        assert_eq!(avcc_length_size(&[1, 0x64, 0, 0x1f, 0xff]), 4);
        assert_eq!(avcc_length_size(&[]), 4);
        assert_eq!(avcc_length_size(&[1, 0x64, 0, 0x1f, 0xfd]), 2);
        assert_eq!(hvcc_length_size(&[0; 22]), 1);
        assert_eq!(hvcc_length_size(&[]), 4);
    }

    #[test]
    fn what_goes_in_comes_back_out_of_every_framing() {
        let packet = annexb_h264();
        let annexb = Framing::AnnexB(Codec::H264);
        let woven = annexb.insert(&packet, &payload(), select()).expect("woven");
        assert_eq!(annexb.payloads(&woven, select()), vec![payload()]);
        assert!(
            annexb.payloads(&packet, select()).is_empty(),
            "a payload came from nowhere"
        );

        let prefixed = Framing::LengthPrefixed {
            codec: Codec::H264,
            length_size: 4,
        };
        let sample = prefixed.reframe(&packet).expect("a sample");
        let woven = prefixed
            .insert(&sample, &payload(), select())
            .expect("woven");
        assert_eq!(prefixed.payloads(&woven, select()), vec![payload()]);
        let nals = split_length_prefixed(&woven, 4).expect("the NALs");
        assert_eq!(nals.len(), 4, "three NALs and the SEI");
        assert_eq!(nals[2][0] & 0x1f, 6, "the SEI is before the slice");
        assert_eq!(
            prefixed.payloads_in_nal(nals[2], select()),
            vec![payload()],
            "and one NAL of it is enough to read"
        );

        // A temporal delimiter, a sequence header and a frame.
        let obus = vec![0x12u8, 0x00, 0x0a, 0x01, 0x00, 0x32, 0x02, 0x00, 0x00];
        let woven = Framing::Av1
            .insert(&obus, &payload(), select())
            .expect("woven");
        assert!(woven.len() > obus.len());
        assert_eq!(Framing::Av1.payloads(&woven, select()), vec![payload()]);

        // Bytes that are not the framing they were said to be answer
        // nothing rather than stopping a read.
        assert!(prefixed
            .payloads(&[0xff, 0xff, 0xff, 0xff, 1], select())
            .is_empty());
    }

    #[test]
    fn a_payload_goes_in_before_the_first_slice_and_moves_nothing() {
        let packet = annexb_h264();
        let framing = Framing::AnnexB(Codec::H264);
        let woven = framing
            .insert(&packet, &payload(), select())
            .expect("woven");
        assert!(woven.len() > packet.len());
        // The parameter sets are still first, the slice still last, and
        // the bytes of both are the bytes that arrived.
        assert!(woven.starts_with(&packet[..packet.len() - 9]));
        assert!(woven.ends_with(&packet[packet.len() - 9..]));
        let read = framing.payloads(&woven, select());
        assert_eq!(read, vec![payload()], "one payload, and it is ours");
    }
}
