//! The carriage pinned against real encoders, a real muxer and a real
//! remuxer.
//!
//! `tests/data/` holds three short clips and two configuration records
//! made once with ffmpeg 9 and committed, so the byte-level tests below
//! run with nothing installed; `scripts/fixtures.sh` regenerates all of
//! them and says exactly how. The clips are two seconds of testsrc2 at
//! 30 frames a second with a keyframe every thirty, one from libx264,
//! one from libx265 and one from libsvtav1. `ref.avcc` and `ref.hvcc`
//! are what ffmpeg's own MP4 muxer writes for the first two, and
//! `ref-extradata.h264` is the parameter sets of the first as its
//! demuxer hands them out of band.
//!
//! Everything asserted about the fixtures is read out of them, never
//! assumed: the access units, the keyframes and x264's own SEI are
//! found by this file's own hand-written scanner as well as by the
//! crate, and the two must agree. The tests that need ffmpeg skip
//! themselves with a message when it is not on the PATH, and say so
//! rather than passing quietly.

use std::path::{Path, PathBuf};
use std::process::Command;

use ffrwd_nal::annexb::{
    annexb_to_length_prefixed, length_prefixed_to_annexb, scan_nals, split_nals, START_CODE,
};
use ffrwd_nal::config::{
    avcc_length_size, avcc_to_annexb_extradata, build_avcc, framing_of, hvcc_length_size,
    parse_parameter_sets, Framing,
};
use ffrwd_nal::feed::{Feed, StreamKind};
use ffrwd_nal::h26x::{access_units, Codec};
use ffrwd_nal::sei::{parse_sei, user_data_annexb, write_user_data_at, SeiMessage};
use ffrwd_nal::sps::profile_level;
use ffrwd_nal::{codec_string, obu, Select};

/// A UUID of this test's own: the version 5 UUID of
/// `https://ffrwd.video/nal/test`, standing in for a carried format's.
const UUID: [u8; 16] = [
    0x04, 0x1f, 0x74, 0xa3, 0x80, 0x90, 0x5e, 0x08, 0xbc, 0xfc, 0x76, 0x4d, 0xf2, 0xdc, 0xd4, 0x66,
];

/// A `metadata_type` from the AV1 unregistered private range.
const METADATA_TYPE: u64 = 25;

fn select() -> Select {
    Select::new(UUID, METADATA_TYPE)
}

/// The payload carrier `index` gets: the UUID and something to tell it
/// from the others, with zeroes in it so the escaping is exercised.
fn payload(index: usize) -> Vec<u8> {
    let mut payload = UUID.to_vec();
    payload.extend_from_slice(&(index as u32).to_be_bytes());
    payload.extend_from_slice(&[0, 0, 0, 1, 0, 0, 3, 0xff]);
    payload.extend_from_slice(format!("carrier {index}").as_bytes());
    payload
}

/// Which fixture, and how its carriage works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    H264,
    H265,
    Av1,
}

impl Stream {
    fn every() -> [Stream; 3] {
        [Stream::H264, Stream::H265, Stream::Av1]
    }

    fn name(self) -> &'static str {
        match self {
            Stream::H264 => "h264",
            Stream::H265 => "h265",
            Stream::Av1 => "av1",
        }
    }

    /// The extension ffmpeg's demuxer for this elementary stream wants.
    fn extension(self) -> &'static str {
        match self {
            Stream::H264 => "h264",
            Stream::H265 => "h265",
            Stream::Av1 => "obu",
        }
    }

    /// The format name for ffmpeg's `-f`.
    fn format(self) -> &'static str {
        match self {
            Stream::H264 => "h264",
            Stream::H265 => "hevc",
            Stream::Av1 => "obu",
        }
    }

    /// The bitstream filter that unpacks this codec out of MP4 and
    /// Matroska, where NALs carry lengths instead of start codes.
    fn to_annexb(self) -> Option<&'static str> {
        match self {
            Stream::H264 => Some("h264_mp4toannexb"),
            Stream::H265 => Some("hevc_mp4toannexb"),
            Stream::Av1 => None,
        }
    }

    fn codec(self) -> Option<Codec> {
        match self {
            Stream::H264 => Some(Codec::H264),
            Stream::H265 => Some(Codec::H265),
            Stream::Av1 => None,
        }
    }

    fn bytes(self) -> Vec<u8> {
        data(&format!("ref.{}", self.extension()))
    }

    /// The carriers of a stream: where each one starts and where a
    /// payload goes in it.
    fn carriers(self, bytes: &[u8]) -> Vec<Spot> {
        match self.codec() {
            Some(codec) => access_units(bytes, codec)
                .into_iter()
                .map(|unit| Spot {
                    start: unit.start,
                    end: unit.end,
                    insert_at: unit.insert_at,
                    keyframe: unit.keyframe,
                    temporal_id_plus1: unit.temporal_id_plus1,
                })
                .collect(),
            None => obu::temporal_units(bytes)
                .expect("the AV1 fixture reads")
                .into_iter()
                .map(|unit| Spot {
                    start: unit.start,
                    end: unit.end,
                    insert_at: unit.insert_at,
                    // An AV1 writer repeats the sequence header before
                    // each key frame, which is as close to a sync
                    // sample as a reader gets without decoding.
                    keyframe: unit.has_sequence_header,
                    temporal_id_plus1: 1,
                })
                .collect(),
        }
    }

    /// The stream with one payload spliced into every carrier.
    fn splice(self, bytes: &[u8]) -> Vec<u8> {
        let spots = self.carriers(bytes);
        let mut out = Vec::with_capacity(bytes.len() + 4096);
        let mut at = 0usize;
        for (index, spot) in spots.iter().enumerate() {
            out.extend_from_slice(&bytes[at..spot.insert_at]);
            match self.codec() {
                Some(codec) => {
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(&write_user_data_at(
                        &payload(index),
                        codec,
                        spot.temporal_id_plus1,
                    ));
                }
                None => out.extend_from_slice(&obu::write_metadata(METADATA_TYPE, &payload(index))),
            }
            at = spot.insert_at;
        }
        out.extend_from_slice(&bytes[at..]);
        out
    }

    /// The payloads in each carrier of a stream.
    fn payloads(self, bytes: &[u8]) -> Vec<Vec<Vec<u8>>> {
        self.carriers(bytes)
            .into_iter()
            .map(|spot| self.payloads_in(&bytes[spot.start..spot.end]))
            .collect()
    }

    /// The payloads in some bytes of a stream.
    fn payloads_in(self, bytes: &[u8]) -> Vec<Vec<u8>> {
        match self.codec() {
            Some(codec) => user_data_annexb(bytes, codec, &UUID),
            None => obu::user_data(bytes, METADATA_TYPE, &UUID),
        }
    }
}

/// One carrier: where it is, and what an SEI put in it has to repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Spot {
    start: usize,
    end: usize,
    insert_at: usize,
    keyframe: bool,
    temporal_id_plus1: u8,
}

fn data(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    std::fs::read(&path).expect("the committed fixture")
}

// ---------------------------------------------------------------- //
// An independent reader, written without the crate.
// ---------------------------------------------------------------- //

/// The payloads of an Annex B stream, found by hand.
///
/// Deliberately naive and deliberately not the crate's code: find start
/// codes, take SEI NALs, undo the escapes, walk the messages, keep the
/// payloads of type 5 that open with the UUID.
fn nal_payloads_by_hand(bytes: &[u8], header_len: usize, sei_type: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut starts = Vec::new();
    for at in 0..bytes.len().saturating_sub(2) {
        if bytes[at] == 0 && bytes[at + 1] == 0 && bytes[at + 2] == 1 {
            starts.push(at + 3);
        }
    }
    for (index, start) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).copied().unwrap_or(bytes.len() + 3) - 3;
        if end > *start && bytes[end - 1] == 0 {
            end -= 1;
        }
        let nal = &bytes[*start..end.min(bytes.len())];
        let kind = if header_len == 1 {
            nal.first().map(|byte| byte & 0x1f)
        } else {
            nal.first().map(|byte| byte >> 1 & 0x3f)
        };
        if kind != Some(sei_type) || nal.len() <= header_len {
            continue;
        }
        let mut rbsp = Vec::new();
        let mut zeros = 0;
        for byte in &nal[header_len..] {
            if zeros == 2 && *byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if *byte == 0 { zeros + 1 } else { 0 };
            rbsp.push(*byte);
        }
        let mut at = 0usize;
        while at + 2 < rbsp.len() {
            let mut kind = 0usize;
            while rbsp.get(at) == Some(&0xff) {
                kind += 255;
                at += 1;
            }
            kind += rbsp[at] as usize;
            at += 1;
            let mut size = 0usize;
            while rbsp.get(at) == Some(&0xff) {
                size += 255;
                at += 1;
            }
            if at >= rbsp.len() {
                break;
            }
            size += rbsp[at] as usize;
            at += 1;
            if at + size > rbsp.len() {
                break;
            }
            let payload = &rbsp[at..at + size];
            at += size;
            if kind == 5 && payload.starts_with(&UUID) {
                out.push(payload.to_vec());
            }
        }
    }
    out
}

/// The payloads of a low-overhead AV1 stream, found by hand.
fn av1_payloads_by_hand(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let header = bytes[at];
        let kind = header >> 3 & 0x0f;
        let extension = header & 0x04 != 0;
        let sized = header & 0x02 != 0;
        at += 1 + usize::from(extension);
        let mut size = 0u64;
        if sized {
            let mut shift = 0;
            loop {
                let Some(byte) = bytes.get(at) else {
                    return out;
                };
                at += 1;
                size |= u64::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
        } else {
            size = (bytes.len() - at) as u64;
        }
        let end = at + size as usize;
        if end > bytes.len() {
            return out;
        }
        let body = &bytes[at..end];
        at = end;
        // metadata_type 25 is one leb128 byte.
        if kind == 5 && body.first() == Some(&(METADATA_TYPE as u8)) && body[1..].starts_with(&UUID)
        {
            let mut payload = &body[1..];
            while payload.last() == Some(&0) {
                payload = &payload[..payload.len() - 1];
            }
            if payload.last() == Some(&0x80) {
                payload = &payload[..payload.len() - 1];
            }
            out.push(payload.to_vec());
        }
    }
    out
}

/// Every payload of a woven stream, by hand.
fn by_hand(stream: Stream, bytes: &[u8]) -> Vec<Vec<u8>> {
    match stream {
        Stream::H264 => nal_payloads_by_hand(bytes, 1, 6),
        Stream::H265 => nal_payloads_by_hand(bytes, 2, 39),
        Stream::Av1 => av1_payloads_by_hand(bytes),
    }
}

// ---------------------------------------------------------------- //
// ffmpeg, when it is here.
// ---------------------------------------------------------------- //

/// Whether ffmpeg is on the PATH.
fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Says a test is skipping, so a run without ffmpeg does not look like
/// a run that proved something.
fn skipping(what: &str) {
    println!("skipping {what}: ffmpeg is not on the PATH");
}

/// A directory of this test's own, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ffrwd-nal-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Runs ffmpeg, and fails the test with its own words if it refuses.
fn ffmpeg(args: &[&str]) {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg runs");
    assert!(
        output.status.success(),
        "ffmpeg {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The per-frame checksums of a file, which is what "the picture is
/// untouched" means in bytes.
fn framemd5(path: &Path) -> String {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        output.status.success(),
        "ffmpeg could not decode {}:\n{}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- //
// The tests.
// ---------------------------------------------------------------- //

#[test]
fn the_fixtures_are_what_the_tests_assume() {
    for stream in Stream::every() {
        let bytes = stream.bytes();
        let spots = stream.carriers(&bytes);
        assert_eq!(spots.len(), 60, "{}: two seconds at 30fps", stream.name());
        let keyframes = spots.iter().filter(|spot| spot.keyframe).count();
        assert_eq!(
            keyframes,
            2,
            "{}: a keyframe every 30 frames",
            stream.name()
        );
        assert!(spots[0].keyframe, "{}: the first frame", stream.name());
        // The carriers tile the stream with nothing left over.
        assert_eq!(spots[0].start, 0);
        assert_eq!(spots.last().expect("a carrier").end, bytes.len());
        for pair in spots.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "{}", stream.name());
            assert!(pair[0].insert_at >= pair[0].start && pair[0].insert_at <= pair[0].end);
        }
        // Nothing of ours is in there yet, by either reader.
        assert!(by_hand(stream, &bytes).is_empty());
        assert!(stream.payloads(&bytes).iter().all(|found| found.is_empty()));
    }

    // x264 writes its own user_data_unregistered SEI, and it is not
    // ours: a reader must walk straight past it.
    let h264 = Stream::H264.bytes();
    let sei: Vec<SeiMessage> = scan_nals(&h264)
        .iter()
        .filter(|nal| Codec::H264.is_prefix_sei(nal.bytes))
        .flat_map(|nal| parse_sei(nal.bytes, Codec::H264).expect("SEI messages"))
        .collect();
    assert!(
        sei.iter().any(|message| message.payload_type == 5),
        "the fixture has no x264 settings SEI"
    );
    assert!(
        sei.iter().any(|message| {
            message.payload_type == 5 && String::from_utf8_lossy(&message.payload).contains("x264")
        }),
        "the x264 SEI does not name x264"
    );
    assert!(
        sei.iter()
            .all(|message| message.user_data_body(&UUID).is_none()),
        "an SEI of ours is already in the fixture"
    );
}

#[test]
fn payloads_woven_into_a_stream_read_back_byte_for_byte() {
    for stream in Stream::every() {
        let original = stream.bytes();
        let woven = stream.splice(&original);
        let want: Vec<Vec<Vec<u8>>> = (0..60).map(|index| vec![payload(index)]).collect();
        assert_eq!(stream.payloads(&woven), want, "{}", stream.name());
        // The second, independent reader finds the same payloads in the
        // same order.
        let theirs = by_hand(stream, &woven);
        let ours = stream.payloads_in(&woven);
        assert_eq!(theirs, ours, "{}", stream.name());
        assert_eq!(ours.len(), 60, "{}", stream.name());
    }
}

#[test]
fn weaving_adds_nothing_but_the_payloads() {
    for stream in Stream::every() {
        let original = stream.bytes();
        let woven = stream.splice(&original);
        match stream.codec() {
            Some(codec) => {
                let before: Vec<&[u8]> = split_nals(&original);
                let after: Vec<&[u8]> = split_nals(&woven)
                    .into_iter()
                    .filter(|nal| ffrwd_nal::sei::user_data_in_nal(nal, codec, &UUID).is_empty())
                    .collect();
                assert_eq!(after, before, "{}: a NAL changed", stream.name());
            }
            None => {
                let before: Vec<Vec<u8>> = obu::scan_obus(&original)
                    .expect("obus")
                    .iter()
                    .map(|unit| original[unit.start..unit.end].to_vec())
                    .collect();
                let after: Vec<Vec<u8>> = obu::scan_obus(&woven)
                    .expect("obus")
                    .iter()
                    .filter(|unit| obu::metadata_payload(unit, METADATA_TYPE).is_none())
                    .map(|unit| woven[unit.start..unit.end].to_vec())
                    .collect();
                assert_eq!(after, before, "{}: an OBU changed", stream.name());
            }
        }
        // And the carriers are still the carriers.
        let spots = stream.carriers(&woven);
        assert_eq!(spots.len(), 60, "{}", stream.name());
        assert_eq!(
            spots.iter().filter(|spot| spot.keyframe).count(),
            2,
            "{}",
            stream.name()
        );
    }
}

#[test]
fn x264s_own_sei_is_still_there_and_untouched() {
    let original = Stream::H264.bytes();
    let woven = Stream::H264.splice(&original);
    let theirs = |bytes: &[u8]| -> Vec<Vec<u8>> {
        scan_nals(bytes)
            .iter()
            .filter(|nal| Codec::H264.is_prefix_sei(nal.bytes))
            .flat_map(|nal| parse_sei(nal.bytes, Codec::H264).expect("SEI messages"))
            .filter(|message| !message.payload.starts_with(&UUID))
            .map(|message| message.payload)
            .collect()
    };
    let before = theirs(&original);
    assert!(!before.is_empty(), "the fixture has no foreign SEI");
    assert_eq!(theirs(&woven), before, "a foreign SEI changed");
}

#[test]
fn a_real_access_unit_survives_both_framings() {
    for stream in [Stream::H264, Stream::H265] {
        let codec = stream.codec().expect("a NAL codec");
        let bytes = stream.bytes();
        let woven = stream.splice(&bytes);
        for spot in stream.carriers(&woven) {
            let au = &woven[spot.start..spot.end];
            for length_size in 2..=4usize {
                let framing = Framing::LengthPrefixed { codec, length_size };
                let sample = framing.reframe(au).expect("a sample");
                assert_eq!(
                    framing.payloads(&sample, select()),
                    user_data_annexb(au, codec, &UUID),
                    "{} at {length_size} bytes a length",
                    stream.name()
                );
                let back = length_prefixed_to_annexb(&sample, length_size).expect("annex b");
                assert_eq!(split_nals(&back), split_nals(au), "{}", stream.name());
            }
        }
        // And a payload put into a length-prefixed sample of the real
        // stream comes back out of it.
        let framing = Framing::LengthPrefixed {
            codec,
            length_size: 4,
        };
        let first = stream.carriers(&bytes)[0];
        let sample = annexb_to_length_prefixed(&bytes[first.start..first.end], 4).expect("sample");
        let inserted = framing
            .insert(&sample, &payload(7), select())
            .expect("woven");
        assert_eq!(framing.payloads(&inserted, select()), vec![payload(7)]);
    }
}

#[test]
fn a_feed_finds_the_carriers_a_whole_stream_does() {
    for stream in Stream::every() {
        let woven = stream.splice(&stream.bytes());
        let kind = match stream.codec() {
            Some(codec) => StreamKind::Nal(codec),
            None => StreamKind::Av1,
        };
        let whole: Vec<(bool, Vec<Vec<u8>>)> = stream
            .carriers(&woven)
            .iter()
            .zip(stream.payloads(&woven))
            .map(|(spot, payloads)| (spot.keyframe, payloads))
            .collect();
        for chunk_size in [1usize, 97, 4096] {
            let mut feed = Feed::new(kind, select());
            let mut carried = Vec::new();
            for chunk in woven.chunks(chunk_size) {
                carried.extend(feed.push(chunk).expect("a push"));
            }
            carried.extend(feed.finish().expect("the last carrier"));
            let found: Vec<(bool, Vec<Vec<u8>>)> = carried
                .iter()
                .map(|one| (one.keyframe, one.payloads.clone()))
                .collect();
            assert_eq!(
                found,
                whole,
                "{} fed {chunk_size} bytes at a time",
                stream.name()
            );
            assert_eq!(feed.dropped(), 0, "{}", stream.name());
        }
    }
}

#[test]
fn a_truncated_fixture_is_refused_not_panicked() {
    // Every parser entry point over prefixes of the real streams, at
    // every length near a boundary and then some.
    for stream in Stream::every() {
        let woven = stream.splice(&stream.bytes());
        let mut cuts: Vec<usize> = stream
            .carriers(&woven)
            .iter()
            .flat_map(|spot| [spot.start, spot.insert_at, spot.end])
            .flat_map(|at| [at.saturating_sub(1), at, at + 1])
            .filter(|at| *at <= woven.len())
            .collect();
        cuts.extend((0..woven.len()).step_by(997));
        for cut in cuts {
            let prefix = &woven[..cut];
            let _ = stream.payloads_in(prefix);
            match stream.codec() {
                Some(codec) => {
                    let _ = access_units(prefix, codec);
                    let _ = scan_nals(prefix);
                    for length_size in 1..=4 {
                        let _ = annexb_to_length_prefixed(prefix, length_size);
                        let _ = length_prefixed_to_annexb(prefix, length_size);
                        let framing = Framing::LengthPrefixed { codec, length_size };
                        let _ = framing.payloads(prefix, select());
                        let _ = framing.insert(prefix, &payload(0), select());
                    }
                    let _ = Framing::AnnexB(codec).insert(prefix, &payload(0), select());
                }
                None => {
                    let _ = obu::scan_obus(prefix);
                    let _ = obu::temporal_units(prefix);
                    let _ = Framing::Av1.insert(prefix, &payload(0), select());
                }
            }
        }
    }
}

#[test]
fn the_record_ffmpeg_writes_is_the_record_this_builds() {
    // The round trip moq's own reference test made, against ffmpeg 9's
    // avcC for the h264 fixture: the sets come out of the record as
    // Annex B, go back in, and the record built from them is ffmpeg's
    // byte for byte, extension bytes and all.
    let avcc = data("ref.avcc");
    let extradata = avcc_to_annexb_extradata(&avcc).expect("the sets");
    let (sps, pps) = parse_parameter_sets(&extradata);
    assert_eq!((sps.len(), pps.len()), (1, 1));
    assert_eq!(build_avcc(&sps, &pps).expect("an avcC"), avcc);
    assert_eq!(avcc_length_size(&avcc), 4);
    assert_eq!(codec_string::avc_codec(&avcc), "avc1.64000d");
    assert_eq!(profile_level(&avcc), Some((0x64, 0x0d)));
    assert_eq!(
        codec_string::avc_codec_from(0x64, 0x0d, &avcc),
        codec_string::avc_codec(&avcc)
    );

    // The hvcC is read for one thing only, and that one thing is right.
    let hvcc = data("ref.hvcc");
    assert_eq!(hvcc_length_size(&hvcc), 4);
    assert_eq!(
        framing_of("hevc", &hvcc).expect("a framing"),
        Framing::LengthPrefixed {
            codec: Codec::H265,
            length_size: 4
        }
    );
    assert_eq!(
        framing_of("h264", &avcc).expect("a framing"),
        Framing::LengthPrefixed {
            codec: Codec::H264,
            length_size: 4
        }
    );
}

#[test]
fn the_record_built_from_ffmpegs_own_extradata_is_ffmpegs_own_record() {
    // What a stream really hands a filter out of band, and the reason
    // the scanner counts padding the way Annex B does. ffmpeg's H.264
    // demuxer writes a `trailing_zero_8bits` after the SPS of its
    // extradata and its MP4 muxer does not carry it into the avcC. The
    // byte belongs to the byte stream and to neither NAL unit, so the
    // sets read out of the wire extradata are the sets inside the
    // record, and the record built from them is ffmpeg's own.
    let extradata = data("ref-extradata.h264");
    let avcc = data("ref.avcc");
    let (wire, pps) = parse_parameter_sets(&extradata);
    let (record, record_pps) =
        parse_parameter_sets(&avcc_to_annexb_extradata(&avcc).expect("the sets"));
    assert_eq!((wire.len(), pps.len()), (1, 1));
    assert_eq!(wire, record, "the SPS on the wire is the SPS in the record");
    assert_eq!(pps, record_pps);
    assert_eq!(build_avcc(&wire, &pps).expect("an avcC"), avcc);
    assert_eq!(profile_level(&extradata), Some((0x64, 0x0d)));

    // The padding is the one byte the record's own Annex B spelling
    // does not have, and the only thing a reframe drops.
    let spelled = avcc_to_annexb_extradata(&avcc).expect("the sets");
    assert_eq!(spelled.len() + 1, extradata.len());
    assert_eq!(split_nals(&spelled), split_nals(&extradata));
    for length_size in 2..=4usize {
        let framed = annexb_to_length_prefixed(&extradata, length_size).expect("a sample");
        assert_eq!(
            framed,
            annexb_to_length_prefixed(&spelled, length_size).expect("a sample")
        );
        assert_eq!(
            length_prefixed_to_annexb(&framed, length_size).expect("annex b"),
            spelled
        );
    }
}

#[test]
fn the_picture_decodes_to_the_same_frames() {
    if !have_ffmpeg() {
        skipping("the framemd5 comparison");
        return;
    }
    for stream in Stream::every() {
        let dir = scratch(&format!("framemd5-{}", stream.name()));
        let original = stream.bytes();
        let woven = stream.splice(&original);
        let before = dir.join(format!("before.{}", stream.extension()));
        let after = dir.join(format!("after.{}", stream.extension()));
        std::fs::write(&before, &original).expect("the original");
        std::fs::write(&after, &woven).expect("the woven stream");
        assert_eq!(
            framemd5(&before),
            framemd5(&after),
            "{}: the woven stream decodes to different frames",
            stream.name()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn ffmpeg_and_ffprobe_say_nothing_new_about_the_woven_stream() {
    if !have_ffmpeg() {
        skipping("the decoder's own opinion");
        return;
    }
    for stream in Stream::every() {
        let dir = scratch(&format!("quiet-{}", stream.name()));
        let original = stream.bytes();
        let woven = stream.splice(&original);
        let before = dir.join(format!("before.{}", stream.extension()));
        let after = dir.join(format!("after.{}", stream.extension()));
        std::fs::write(&before, &original).expect("the original");
        std::fs::write(&after, &woven).expect("the woven stream");

        let complaints = |path: &Path| -> Vec<String> {
            let output = Command::new("ffmpeg")
                .args(["-hide_banner", "-nostdin", "-v", "warning", "-i"])
                .arg(path)
                .args(["-f", "null", "-"])
                .output()
                .expect("ffmpeg runs");
            assert!(output.status.success(), "ffmpeg refused {}", path.display());
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .map(|line| line.replace(&path.display().to_string(), "<file>"))
                .filter(|line| !line.trim().is_empty())
                .collect()
        };
        assert_eq!(
            complaints(&after),
            complaints(&before),
            "{}: the decoder has something new to say",
            stream.name()
        );

        let probe = Command::new("ffprobe")
            .args(["-v", "warning", "-show_entries", "stream=codec_name"])
            .args(["-of", "csv=p=0"])
            .arg(&after)
            .output()
            .expect("ffprobe runs");
        assert!(probe.status.success(), "{}: ffprobe refused", stream.name());
        assert!(
            String::from_utf8_lossy(&probe.stderr).trim().is_empty(),
            "{}: ffprobe warned: {}",
            stream.name(),
            String::from_utf8_lossy(&probe.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn remuxing_through_the_containers_keeps_the_payloads() {
    if !have_ffmpeg() {
        skipping("the remux round trips");
        return;
    }
    for stream in Stream::every() {
        let dir = scratch(&format!("remux-{}", stream.name()));
        let woven = stream.splice(&stream.bytes());
        let source = dir.join(format!("woven.{}", stream.extension()));
        std::fs::write(&source, &woven).expect("the woven stream");
        let want = stream.payloads_in(&woven);
        assert_eq!(want.len(), 60);

        // An elementary stream carries no timestamps, and the Matroska
        // and MPEG-TS muxers will not take packets without them, so the
        // MP4 made first is what the other two are made from. That is
        // what a real pipeline does as well.
        let mp4 = dir.join("remux.mp4");
        ffmpeg(&[
            "-loglevel",
            "error",
            "-i",
            source.to_str().expect("a path"),
            "-c",
            "copy",
            "-f",
            "mp4",
            mp4.to_str().expect("a path"),
        ]);

        let mut containers = vec![("mp4", mp4.clone())];
        for (name, format) in [("mkv", "matroska"), ("ts", "mpegts")] {
            if stream == Stream::Av1 && name == "ts" {
                continue; // MPEG-TS has no place to put AV1 here.
            }
            let path = dir.join(format!("remux.{name}"));
            ffmpeg(&[
                "-loglevel",
                "error",
                "-i",
                mp4.to_str().expect("a path"),
                "-c",
                "copy",
                "-f",
                format,
                path.to_str().expect("a path"),
            ]);
            containers.push((name, path));
        }

        for (name, path) in containers {
            let back = dir.join(format!("back-{name}.{}", stream.extension()));
            let mut args = vec![
                "-loglevel",
                "error",
                "-i",
                path.to_str().expect("a path"),
                "-c",
                "copy",
            ];
            // MPEG-TS already carries start codes; MP4 and Matroska do
            // not, and the bitstream filter is what puts them back.
            if let (Some(filter), false) = (stream.to_annexb(), name == "ts") {
                args.extend(["-bsf:v", filter]);
            }
            args.extend(["-f", stream.format(), back.to_str().expect("a path")]);
            ffmpeg(&args);

            let bytes = std::fs::read(&back).expect("the stream back out");
            assert_eq!(
                stream.payloads_in(&bytes),
                want,
                "{} lost payloads through {name}",
                stream.name()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn trace_headers_sees_a_user_data_unregistered_sei_of_ours() {
    if !have_ffmpeg() {
        skipping("the trace_headers reading");
        return;
    }
    // One payload in one access unit, and small enough that ffmpeg
    // prints the size in a single byte.
    let dir = scratch("trace");
    let original = Stream::H264.bytes();
    let mut body = UUID.to_vec();
    body.extend_from_slice(b"one payload, under 255 bytes of it");
    assert!(body.len() < 255);
    let spots = Stream::H264.carriers(&original);
    let sei = write_user_data_at(&body, Codec::H264, spots[0].temporal_id_plus1);
    let mut woven = original[..spots[0].insert_at].to_vec();
    woven.extend_from_slice(&START_CODE);
    woven.extend_from_slice(&sei);
    woven.extend_from_slice(&original[spots[0].insert_at..]);
    let path = dir.join("traced.h264");
    std::fs::write(&path, &woven).expect("the woven stream");

    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "trace", "-i"])
        .arg(&path)
        .args(["-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(output.status.success(), "ffmpeg refused the woven stream");
    let trace = String::from_utf8_lossy(&output.stderr);

    // ffmpeg's own reading of the SEI: the payload type, the size, and
    // the sixteen bytes of the UUID, in order.
    let values = |needle: &str| -> Vec<u8> {
        trace
            .lines()
            .filter(|line| line.contains(needle))
            .filter_map(|line| line.rsplit('=').next())
            .filter_map(|value| value.trim().parse::<u32>().ok())
            .map(|value| value as u8)
            .collect()
    };
    let types = values("last_payload_type_byte");
    assert!(
        types.contains(&5),
        "no user_data_unregistered SEI in the trace"
    );
    let uuids = values("uuid_iso_iec_11578[");
    assert!(
        uuids.windows(16).any(|window| window == UUID),
        "the trace does not show this UUID"
    );
    // The SEI whose UUID is ours is the one whose size is our payload's.
    let sizes = values("last_payload_size_byte");
    assert!(
        sizes.contains(&(body.len() as u8)),
        "no SEI of {} bytes in the trace: sizes were {sizes:?}",
        body.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
