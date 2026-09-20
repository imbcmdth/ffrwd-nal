//! Supplemental enhancement information, ISO/IEC 14496-10 Annex D and
//! ISO/IEC 23008-2 Annex D: the messages an SEI NAL unit carries, and
//! putting one where the standard says it goes.
//!
//! An SEI NAL unit holds one or more messages, each a `payloadType`, a
//! `payloadSize` and that many bytes, both numbers spelled as `ff`
//! bytes and a remainder. A prefix SEI goes before the first coded
//! slice of its access unit, which puts it after the access unit
//! delimiter, the parameter sets and any SEI that was already there.
//!
//! Two rules a reader here follows. It touches nothing that is not the
//! message it came for, so an encoder's settings SEI, captions and HDR
//! metadata travel on untouched. And it gives up quietly on a message
//! whose `payloadSize` overruns the NAL: what parsed before it stands,
//! because a reader looking for its own message has no reason to drop
//! the ones it already read over a trailing byte it cannot use.

use crate::annexb::{put_length, scan_nals, split_length_prefixed, START_CODE};
use crate::ep::{insert_emulation_prevention, remove_emulation_prevention};
use crate::h26x::{access_units, Codec};
use crate::{Error, Result};

/// `user_data_unregistered`, the payload type a format of one's own
/// travels in: 16 bytes of `uuid_iso_iec_11578` and then whatever the
/// writer of that UUID says.
pub const USER_DATA_UNREGISTERED: u32 = 5;

/// One SEI message: its type and its payload, the payload already free
/// of emulation prevention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeiMessage {
    pub payload_type: u32,
    pub payload: Vec<u8>,
}

impl SeiMessage {
    /// A `user_data_unregistered` message carrying `payload`, which
    /// opens with the writer's own 16 byte UUID.
    pub fn user_data(payload: &[u8]) -> Self {
        Self {
            payload_type: USER_DATA_UNREGISTERED,
            payload: payload.to_vec(),
        }
    }

    /// Whether this is a `user_data_unregistered` message opened by
    /// `uuid`.
    pub fn is_user_data(&self, uuid: &[u8; 16]) -> bool {
        self.payload_type == USER_DATA_UNREGISTERED && self.payload.starts_with(uuid)
    }

    /// The bytes after `uuid`, when this is a `user_data_unregistered`
    /// message opened by it.
    ///
    /// The finders below hand back the whole payload, UUID and all,
    /// because that is the unit a carried format encodes and decodes;
    /// this is for a caller that wants only what follows.
    pub fn user_data_body(&self, uuid: &[u8; 16]) -> Option<&[u8]> {
        self.is_user_data(uuid).then(|| &self.payload[16..])
    }
}

/// The SEI messages of an SEI NAL unit.
///
/// Anything the NAL holds past what parses is left behind rather than
/// refused.
pub fn parse_sei(nal: &[u8], codec: Codec) -> Result<Vec<SeiMessage>> {
    if !codec.is_prefix_sei(nal) {
        return Err(Error::Malformed {
            what: "the NAL unit is not a prefix SEI",
            at: 0,
        });
    }
    let rbsp = remove_emulation_prevention(&nal[codec.header_len()..]);
    Ok(parse_sei_rbsp(&rbsp))
}

/// The SEI messages of an SEI RBSP, header and escapes already gone.
pub fn parse_sei_rbsp(rbsp: &[u8]) -> Vec<SeiMessage> {
    // `more_rbsp_data`: the payload ends at the stop bit, which is the
    // last byte that is not a trailing zero, and that byte is 0x80
    // because every SEI payload is a whole number of bytes.
    let mut end = rbsp.len();
    while end > 0 && rbsp[end - 1] == 0 {
        end -= 1;
    }
    if end > 0 && rbsp[end - 1] == 0x80 {
        end -= 1;
    }
    let data = &rbsp[..end];

    let mut out = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let Some((payload_type, next)) = read_ff(data, at) else {
            break;
        };
        let Some((payload_size, next)) = read_ff(data, next) else {
            break;
        };
        let size = payload_size as usize;
        if next + size > data.len() {
            break;
        }
        out.push(SeiMessage {
            payload_type,
            payload: data[next..next + size].to_vec(),
        });
        at = next + size;
    }
    out
}

/// One `ff`-extended value, and where it ended.
fn read_ff(data: &[u8], mut at: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    loop {
        let byte = *data.get(at)?;
        at += 1;
        value = value.checked_add(u32::from(byte))?;
        if byte != 0xff {
            return Some((value, at));
        }
    }
}

/// Appends one `ff`-extended value.
fn put_ff(out: &mut Vec<u8>, mut value: u32) {
    while value >= 255 {
        out.push(0xff);
        value -= 255;
    }
    out.push(value as u8);
}

/// An SEI NAL unit carrying `messages`, escapes and trailing bits and
/// all, for an access unit at `temporal_id_plus1`.
pub fn write_sei(messages: &[SeiMessage], codec: Codec, temporal_id_plus1: u8) -> Vec<u8> {
    let mut rbsp = Vec::new();
    for message in messages {
        put_ff(&mut rbsp, message.payload_type);
        put_ff(&mut rbsp, message.payload.len() as u32);
        rbsp.extend_from_slice(&message.payload);
    }
    // rbsp_trailing_bits: a one bit, then zeroes to the byte.
    rbsp.push(0x80);
    let mut nal = codec.sei_header(temporal_id_plus1);
    nal.extend_from_slice(&insert_emulation_prevention(&rbsp));
    nal
}

/// A `user_data_unregistered` payload as an SEI NAL unit, ready to
/// splice into an access unit at `temporal_id_plus1`, which
/// [`crate::h26x::AccessUnit`] carries.
pub fn write_user_data_at(payload: &[u8], codec: Codec, temporal_id_plus1: u8) -> Vec<u8> {
    write_sei(&[SeiMessage::user_data(payload)], codec, temporal_id_plus1)
}

/// The same for an access unit of the base temporal layer, which is
/// every access unit of a stream without sub-layers.
pub fn write_user_data(payload: &[u8], codec: Codec) -> Vec<u8> {
    write_user_data_at(payload, codec, 1)
}

/// The payloads in one SEI NAL unit that are opened by `uuid`, UUID and
/// all. A NAL that is not a prefix SEI carries none.
pub fn user_data_in_nal(nal: &[u8], codec: Codec, uuid: &[u8; 16]) -> Vec<Vec<u8>> {
    let Ok(messages) = parse_sei(nal, codec) else {
        return Vec::new();
    };
    messages
        .into_iter()
        .filter(|message| message.is_user_data(uuid))
        .map(|message| message.payload)
        .collect()
}

/// Every payload opened by `uuid` in an Annex B stream, or in one
/// access unit of one.
pub fn user_data_annexb(annexb: &[u8], codec: Codec, uuid: &[u8; 16]) -> Vec<Vec<u8>> {
    scan_nals(annexb)
        .into_iter()
        .flat_map(|nal| user_data_in_nal(nal.bytes, codec, uuid))
        .collect()
}

/// Every payload opened by `uuid` in a length-prefixed sample.
pub fn user_data_length_prefixed(
    sample: &[u8],
    length_size: usize,
    codec: Codec,
    uuid: &[u8; 16],
) -> Result<Vec<Vec<u8>>> {
    Ok(split_length_prefixed(sample, length_size)?
        .into_iter()
        .flat_map(|nal| user_data_in_nal(nal, codec, uuid))
        .collect())
}

/// An Annex B access unit with `sei` spliced in before its first coded
/// slice.
///
/// The bytes on either side are copied through untouched, so a stream
/// written this way differs from the original only by the NALs added.
pub fn insert_sei_annexb(au: &[u8], sei: &[u8], codec: Codec) -> Result<Vec<u8>> {
    let units = access_units(au, codec);
    let at = match units.first() {
        Some(unit) => unit.insert_at,
        None => {
            return Err(Error::Malformed {
                what: "the bytes hold no access unit",
                at: 0,
            })
        }
    };
    let mut out = Vec::with_capacity(au.len() + sei.len() + 4);
    out.extend_from_slice(&au[..at]);
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(sei);
    out.extend_from_slice(&au[at..]);
    Ok(out)
}

/// A length-prefixed sample with `sei` spliced in before its first
/// coded slice.
pub fn insert_sei_length_prefixed(
    sample: &[u8],
    sei: &[u8],
    length_size: usize,
    codec: Codec,
) -> Result<Vec<u8>> {
    let nals = split_length_prefixed(sample, length_size)?;
    let at = nals
        .iter()
        .position(|nal| codec.is_vcl(nal))
        .unwrap_or(nals.len());
    let mut out = Vec::with_capacity(sample.len() + sei.len() + length_size);
    for (index, nal) in nals.iter().enumerate() {
        if index == at {
            let here = out.len();
            put_length(&mut out, sei.len(), length_size, here)?;
            out.extend_from_slice(sei);
        }
        let here = out.len();
        put_length(&mut out, nal.len(), length_size, here)?;
        out.extend_from_slice(nal);
    }
    if at == nals.len() {
        let here = out.len();
        put_length(&mut out, sei.len(), length_size, here)?;
        out.extend_from_slice(sei);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::{annexb_to_length_prefixed, length_prefixed_to_annexb, split_nals};
    use crate::h26x::{H265_PREFIX_SEI, H265_SUFFIX_SEI};

    /// A UUID of this test's own, standing in for a carried format's.
    const UUID: [u8; 16] = [
        0x04, 0x1f, 0x74, 0xa3, 0x80, 0x90, 0x5e, 0x08, 0xbc, 0xfc, 0x76, 0x4d, 0xf2, 0xdc, 0xd4,
        0x66,
    ];

    /// A short Annex B stream shaped like what x264 writes: parameter
    /// sets, the encoder's own SEI, an IDR, then two more pictures.
    fn a_stream() -> Vec<u8> {
        let mut out = Vec::new();
        let mut nal = |payload: &[u8], four: bool| {
            out.extend_from_slice(if four { &START_CODE[..] } else { &[0, 0, 1] });
            out.extend_from_slice(payload);
        };
        nal(&[0x67, 0x64, 0x00, 0x1f, 0xac], true); // SPS
        nal(&[0x68, 0xeb, 0xec, 0xb2], true); // PPS
        nal(&x264_sei(), false); // the encoder's settings
        nal(&[0x65, 0x88, 0x84, 0x00], false); // IDR slice
        nal(&[0x41, 0x9a, 0x01], false); // a P slice
        nal(&[0x41, 0x9a, 0x02], false); // another picture
        out
    }

    /// An SEI of payload type 5 with somebody else's UUID, which is
    /// what an x264 stream opens with.
    fn x264_sei() -> Vec<u8> {
        let mut payload = vec![0xdcu8, 0x45, 0xe9, 0xbd, 0xe6, 0xd9, 0x48, 0xb7];
        payload.extend_from_slice(&[0x96, 0x2c, 0xd8, 0x20, 0xd9, 0x23, 0xee, 0xef]);
        payload.extend_from_slice(b"x264 - core 164 - options: cabac=1");
        write_sei(&[SeiMessage::user_data(&payload)], Codec::H264, 1)
    }

    fn a_payload() -> Vec<u8> {
        let mut payload = UUID.to_vec();
        payload.push(1);
        payload.extend_from_slice(&[0x02, 0x04, 0x01, 0x00, 0x00, 0x00]);
        payload
    }

    #[test]
    fn a_payload_full_of_start_codes_survives_the_wrap() {
        let mut payload = UUID.to_vec();
        payload.push(1);
        for _ in 0..40 {
            payload.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 3, 0, 0, 2]);
        }
        let nal = write_user_data(&payload, Codec::H264);
        assert!(
            !nal.windows(3).any(|w| w == [0, 0, 1]),
            "the SEI NAL holds a start code"
        );
        assert_eq!(user_data_in_nal(&nal, Codec::H264, &UUID), vec![payload]);
    }

    #[test]
    fn a_payload_of_every_size_around_the_ff_boundary_round_trips() {
        for size in [0usize, 1, 254, 255, 256, 509, 510, 511, 1000] {
            let mut payload = UUID.to_vec();
            payload.push(1);
            payload.resize(17 + size, 0x5a);
            for codec in [Codec::H264, Codec::H265] {
                let nal = write_user_data(&payload, codec);
                assert_eq!(
                    user_data_in_nal(&nal, codec, &UUID),
                    vec![payload.clone()],
                    "{size}"
                );
                let messages = parse_sei(&nal, codec).expect("messages");
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].payload.len(), payload.len());
                assert_eq!(
                    messages[0].user_data_body(&UUID),
                    Some(&payload[16..]),
                    "{size}"
                );
            }
        }
    }

    #[test]
    fn an_sei_that_is_not_ours_is_left_alone() {
        let nal = x264_sei();
        assert!(user_data_in_nal(&nal, Codec::H264, &UUID).is_empty());
        let messages = parse_sei(&nal, Codec::H264).expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload_type, USER_DATA_UNREGISTERED);
        assert!(messages[0].payload.starts_with(&[0xdc, 0x45]));
        assert_eq!(messages[0].user_data_body(&UUID), None);
    }

    #[test]
    fn other_sei_types_in_the_same_nal_are_kept_and_ours_is_found() {
        let nal = write_sei(
            &[
                SeiMessage {
                    payload_type: 1,
                    payload: vec![0x11, 0x22],
                },
                SeiMessage::user_data(&a_payload()),
                SeiMessage {
                    payload_type: 137,
                    payload: vec![0x33],
                },
            ],
            Codec::H264,
            1,
        );
        let messages = parse_sei(&nal, Codec::H264).expect("messages");
        assert_eq!(
            messages.iter().map(|m| m.payload_type).collect::<Vec<_>>(),
            vec![1, 5, 137]
        );
        assert_eq!(
            user_data_in_nal(&nal, Codec::H264, &UUID),
            vec![a_payload()]
        );
    }

    #[test]
    fn the_hevc_header_is_the_two_bytes_of_a_prefix_sei() {
        let nal = write_user_data(&a_payload(), Codec::H265);
        assert_eq!(&nal[..2], &[0x4e, 0x01]);
        assert_eq!(Codec::H265.nal_type(&nal), Some(H265_PREFIX_SEI));
        assert!(Codec::H265.is_prefix_sei(&nal));
        // The same bytes read as H.264 are a different type entirely,
        // so a reader that has the codec wrong finds nothing.
        assert!(user_data_in_nal(&nal, Codec::H264, &UUID).is_empty());
    }

    #[test]
    fn an_hevc_sei_repeats_its_access_units_temporal_id() {
        let sei = write_user_data_at(&a_payload(), Codec::H265, 3);
        assert_eq!(sei[0] >> 1 & 0x3f, H265_PREFIX_SEI);
        assert_eq!(sei[1] & 0x07, 3, "the SEI is on the same sub-layer");
        assert_eq!(
            user_data_in_nal(&sei, Codec::H265, &UUID),
            vec![a_payload()]
        );
        // The base layer is what a stream without sub-layers gets.
        assert_eq!(write_user_data(&a_payload(), Codec::H265)[1], 1);
    }

    #[test]
    fn a_suffix_sei_is_not_where_a_payload_lives() {
        let mut nal = write_user_data(&a_payload(), Codec::H265);
        nal[0] = H265_SUFFIX_SEI << 1;
        assert!(user_data_in_nal(&nal, Codec::H265, &UUID).is_empty());
        assert!(parse_sei(&nal, Codec::H265).is_err());
    }

    #[test]
    fn a_written_stream_differs_only_by_the_sei() {
        let stream = a_stream();
        let sei = write_user_data(&a_payload(), Codec::H264);
        let units = access_units(&stream, Codec::H264);
        let mut woven = Vec::new();
        let mut at = 0usize;
        for unit in &units {
            woven.extend_from_slice(&stream[at..unit.insert_at]);
            woven.extend_from_slice(&START_CODE);
            woven.extend_from_slice(&sei);
            at = unit.insert_at;
        }
        woven.extend_from_slice(&stream[at..]);

        assert_eq!(
            user_data_annexb(&woven, Codec::H264, &UUID),
            vec![a_payload(); 3]
        );
        // Every NAL of the original is still there, in order.
        let original: Vec<&[u8]> = split_nals(&stream);
        let after: Vec<&[u8]> = split_nals(&woven);
        let kept: Vec<&[u8]> = after
            .into_iter()
            .filter(|nal| user_data_in_nal(nal, Codec::H264, &UUID).is_empty())
            .collect();
        assert_eq!(kept, original);
        assert_eq!(access_units(&woven, Codec::H264).len(), units.len());
    }

    #[test]
    fn an_sei_goes_in_before_the_first_slice_either_framing() {
        let stream = a_stream();
        let au = &stream[..access_units(&stream, Codec::H264)[0].end];
        let sei = write_user_data(&a_payload(), Codec::H264);

        let woven = insert_sei_annexb(au, &sei, Codec::H264).expect("a woven access unit");
        let kinds: Vec<Option<u8>> = split_nals(&woven)
            .iter()
            .map(|nal| Codec::H264.nal_type(nal))
            .collect();
        assert_eq!(
            kinds,
            vec![Some(7), Some(8), Some(6), Some(6), Some(5)],
            "ours goes last of the non-slices"
        );
        assert_eq!(
            user_data_annexb(&woven, Codec::H264, &UUID),
            vec![a_payload()]
        );

        for length_size in 1..=4usize {
            let sample = annexb_to_length_prefixed(au, length_size).expect("a sample");
            let woven =
                insert_sei_length_prefixed(&sample, &sei, length_size, Codec::H264).expect("woven");
            assert_eq!(
                user_data_length_prefixed(&woven, length_size, Codec::H264, &UUID).expect("found"),
                vec![a_payload()]
            );
            let back = length_prefixed_to_annexb(&woven, length_size).expect("annex b");
            assert_eq!(
                user_data_annexb(&back, Codec::H264, &UUID),
                vec![a_payload()]
            );
            let kinds: Vec<Option<u8>> = split_nals(&back)
                .iter()
                .map(|nal| Codec::H264.nal_type(nal))
                .collect();
            assert_eq!(kinds, vec![Some(7), Some(8), Some(6), Some(6), Some(5)]);
        }
    }

    #[test]
    fn a_sample_of_no_slices_takes_the_sei_at_the_end() {
        let sei = write_user_data(&a_payload(), Codec::H264);
        let sample = annexb_to_length_prefixed(&[&START_CODE[..], &[0x67, 0x64, 0x00]].concat(), 4)
            .expect("a sample");
        let woven = insert_sei_length_prefixed(&sample, &sei, 4, Codec::H264).expect("woven");
        assert_eq!(
            user_data_length_prefixed(&woven, 4, Codec::H264, &UUID).expect("found"),
            vec![a_payload()]
        );
        assert!(woven.starts_with(&sample), "the SPS is still first");
    }

    #[test]
    fn a_stream_with_no_slices_still_takes_an_sei() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&START_CODE);
        stream.extend_from_slice(&[0x67, 0x64, 0x00]);
        let sei = write_user_data(&a_payload(), Codec::H264);
        let woven = insert_sei_annexb(&stream, &sei, Codec::H264).expect("woven");
        assert_eq!(
            user_data_annexb(&woven, Codec::H264, &UUID),
            vec![a_payload()]
        );
        assert_eq!(
            insert_sei_annexb(&[], &sei, Codec::H264),
            Err(Error::Malformed {
                what: "the bytes hold no access unit",
                at: 0
            })
        );
    }

    #[test]
    fn an_sei_whose_size_overruns_the_nal_gives_up_quietly() {
        // A payload size of 200 with ten bytes behind it. What parsed
        // before it would stand; here nothing did.
        let mut rbsp = vec![5u8, 200];
        rbsp.extend_from_slice(&[0xaa; 10]);
        rbsp.push(0x80);
        let mut nal = vec![0x06u8];
        nal.extend_from_slice(&rbsp);
        assert!(parse_sei(&nal, Codec::H264).expect("messages").is_empty());
        assert!(user_data_in_nal(&nal, Codec::H264, &UUID).is_empty());

        // And with a whole message in front of the broken one, that one
        // is kept.
        let mut rbsp = vec![1u8, 2, 0x11, 0x22, 5, 200];
        rbsp.extend_from_slice(&[0xaa; 10]);
        rbsp.push(0x80);
        let mut nal = vec![0x06u8];
        nal.extend_from_slice(&rbsp);
        let messages = parse_sei(&nal, Codec::H264).expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload_type, 1);
    }

    #[test]
    fn an_escape_free_payload_is_written_exactly_as_it_would_be_by_hand() {
        // The sei packet filter writes its NAL without escaping it,
        // because its UUID has no zero byte and its notes are ASCII.
        // This is that NAL, built by hand the way that filter builds
        // it, and the escaping writer here gives the same bytes: the
        // reason there is no unescaped spelling of a write.
        const THEIRS: [u8; 16] = [
            0x66, 0x66, 0x72, 0x77, 0x64, 0x2d, 0x73, 0x65, 0x69, 0x2d, 0x74, 0x65, 0x73, 0x74,
            0x21, 0x21,
        ];
        for text in ["a note", &"long note ".repeat(40)] {
            let mut by_hand = vec![0x06u8, 0x05];
            let mut left = THEIRS.len() + text.len();
            while left >= 255 {
                by_hand.push(0xff);
                left -= 255;
            }
            by_hand.push(left as u8);
            by_hand.extend_from_slice(&THEIRS);
            by_hand.extend_from_slice(text.as_bytes());
            by_hand.push(0x80);

            let mut payload = THEIRS.to_vec();
            payload.extend_from_slice(text.as_bytes());
            assert_eq!(write_user_data(&payload, Codec::H264), by_hand);
            assert_eq!(
                user_data_in_nal(&by_hand, Codec::H264, &THEIRS),
                vec![payload]
            );
        }
    }

    #[test]
    fn truncated_and_random_nals_never_panic() {
        let nal = write_user_data(&a_payload(), Codec::H264);
        for cut in 0..nal.len() {
            for codec in [Codec::H264, Codec::H265] {
                let _ = user_data_in_nal(&nal[..cut], codec, &UUID);
                let _ = parse_sei(&nal[..cut], codec);
                let _ = access_units(&nal[..cut], codec);
            }
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..3000 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 48 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 33) as u8);
            }
            for codec in [Codec::H264, Codec::H265] {
                let _ = user_data_in_nal(&bytes, codec, &UUID);
                let _ = user_data_annexb(&bytes, codec, &UUID);
                let _ = access_units(&bytes, codec);
                let _ = insert_sei_annexb(&bytes, &nal, codec);
                for length_size in 1..=4 {
                    let _ = user_data_length_prefixed(&bytes, length_size, codec, &UUID);
                    let _ = insert_sei_length_prefixed(&bytes, &nal, length_size, codec);
                }
            }
            let _ = parse_sei_rbsp(&bytes);
        }
    }
}
