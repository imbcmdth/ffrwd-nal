# ffrwd-nal

The byte level of H.264, HEVC and AV1: where the NAL units and OBUs of
a coded stream begin and end, which of them open a picture, and how to
read a payload out of one or put a payload into one without moving
anything else. No dependencies, no unsafe code, no I/O, so a wasm
module compiles it in and its tests run on the host.

```rust
use ffrwd_nal::config::framing_of;
use ffrwd_nal::Select;

const MINE: Select = Select { uuid: [0x04, 0x1f, /* ... */ 0x66], metadata_type: 25 };

let framing = framing_of("h264", &stream.extradata)?;
let packet = framing.insert(&packet, &my_payload, MINE)?;
// and on the way back
for payload in framing.payloads(&packet, MINE) { /* ... */ }
```

Four crates on this machine were carrying their own copy of this and
the copies had drifted: the index package had the scanner, the access
unit rule, emulation prevention both ways and the OBU reader; the moq
package had a second scanner, the `avcC` builder and an exp-Golomb
reader; the sidecar had a third scanner in its NUT bridge and a fourth
in a packet filter. This is those copies reconciled into one, with each
disagreement decided and the decision written down below.

## What it does, and what it leaves to the caller

It cuts. `annexb::scan_nals` gives every NAL unit with the offset of
its start code, `h26x::access_units` groups them into pictures and says
which are random access points, `obu::scan_obus` and
`obu::temporal_units` do the same for AV1, and `feed::Feed` finds the
same boundaries a chunk at a time for a stream that is still being
written. `annexb` reframes between start codes and length prefixes of
one to four bytes, which is the difference between an elementary stream
and an MP4 sample.

It carries. A format that rides inside a coded stream declares a UUID
and, for AV1, a `metadata_type`, which is what `Select` holds. `sei`
finds, builds and places `user_data_unregistered` SEI messages of any
payload type; `obu` finds, builds and places metadata OBUs of any
`metadata_type`. Nothing else in the packet is read and nothing else is
moved: an encoder's own settings SEI, captions and HDR metadata travel
on untouched, byte for byte, which the tests check against real x264
output.

It reads the two records a NAL reader needs. `config` takes the length
prefix width and the parameter sets out of an `avcC` or `hvcC`, builds
an `avcC` back from parameter sets the way ffmpeg's own writer does,
and answers which framing a stream is in. `sps` reads the few SPS
fields that record repeats, and `codec_string` spells the RFC 6381 name.

What it leaves to the caller is everything above the byte level. There
are no boxes here and no containers: a record is a byte slice, and
which box carries it belongs to `ffrwd-bmff`. There are no timestamps,
no index format, no UUID of its own, and no opinion about what a
payload means. The caller brings the UUID and gets its own bytes back.

There is no dependency on `h264-parser`, `scuffle-h265`, `scuffle-av1`
or `mp4-atom`, which is what upstream `moq-mux` uses for the same job
in `rs/moq-mux/src/codec/`. Those crates are fine and were read for
reference. They are not used here because this code compiles into wasm
modules and into a packet filter that runs inside a pipeline, where a
parser crate's own dependency tree, its `bytes` and `thiserror` and
`tokio` neighbours, is a cost nobody asked for, and because the whole
of what four ffrwd crates were already using fits in 1800 lines.

## The rules not to simplify

Each of these was a disagreement between the copies. Each has a test.

**One error type, and no allocation on the error path.** `Error` is a
`Copy` enum of four variants. The index copy carried
`Malformed(&'static str)` and the moq copy formatted messages like "a
NAL of 9 bytes overruns the sample at byte 12". The offsets were worth
keeping and the formatting was not, so `Truncated`, `Malformed` and
`TooLarge` all carry an `at` field and a reader that hits one can say
where without anyone having built a `String`.

**Start codes are three and four bytes, and the scan carries offsets.**
`NalRef` has `code`, `start` and `end`: `code` is where a splice goes,
`start` is where the NAL's own bytes begin. `split_nals` is the slice
iterator on top. Bytes before the first start code are ignored, as
ffmpeg ignores them, and bytes with no start code hold no NAL units
rather than holding one. One zero directly before a three byte code
belongs to the code and no more: a NAL padded with further
`trailing_zero_8bits` keeps them, which is what ffmpeg's own H.264
demuxer writes between the parameter sets of its extradata and what
`ffmpegs_annex_b_extradata_pads_its_sps_and_the_record_does_not` pins
against the real bytes. The one byte difference that leaves between the
wire extradata and the same SPS inside an `avcC` is padding, and no
decoder reads it, but a builder that copies NAL bytes verbatim carries
it through and its `avcC` is one byte longer than ffmpeg's. That is the
one place the record this crate builds can differ from the record
ffmpeg builds from the same stream.

**Length prefixes are one to four bytes everywhere.** The moq copy's
`annexb_to_avcc` was hard-wired to four with an unchecked `as u32`;
`annexb::annexb_to_length_prefixed` takes the size and answers
`TooLarge` when a NAL does not fit the prefix it was given. Anything
outside one to four is refused rather than guessed at.

**Escaping is the default on write, and there is no second spelling.**
The sei packet filter writes its SEI NAL unescaped on purpose: its UUID
has no zero byte in it and its notes are ASCII, so the payload cannot
grow a start code. It does not need a special path, because escaping a
payload with no `00 00 0x` sequence in it returns that payload
unchanged. `escaping_a_payload_that_needs_none_returns_it_unchanged`
proves the rule and
`an_escape_free_payload_is_written_exactly_as_it_would_be_by_hand`
builds that filter's own NAL by hand and shows `sei::write_user_data`
gives the same bytes.

**A finder hands back the whole payload, UUID and all.** That is the
unit a carried format encodes and decodes, so the wrappers in the
consuming crates stay one line each. `SeiMessage::user_data_body` is
there for a caller that wants only what follows the UUID.

**An SEI whose `payloadSize` overruns its NAL gives up quietly.** The
messages that parsed before it stand and the rest is dropped, with no
error: a reader looking for its own message has no reason to lose what
it already read over a trailing byte it cannot use. That was the index
copy's behaviour and what came before stands.

**`first_slice_of_picture` is one bit.** `first_mb_in_slice` in H.264
and `first_slice_segment_in_pic_flag` in HEVC are both the first bit
after the header, and both are one for the first slice, because an
exp-Golomb zero is a single set bit. No emulation prevention byte can
land that early, because the header byte before it is never zero. The
comment saying so is in the code and stays there.

**A record too short to declare its length size is read as four.**
`avcc_length_size` and `hvcc_length_size` answer 4 for a record that
ends before the field, which is what every writer on this machine
means. `framing_of` is stricter about what counts as a record at all: 7
bytes for an `avcC` and 23 for an `hvcC`, both with a configuration
version of 1, and anything shorter is read as Annex B.

**The SPS reader reads what it read before.** Profile, level, chroma
format and the two bit depths, which are the fields an `avcC` extension
repeats. Nothing more.

## Not here yet

Nothing on this machine needs these, and the rule is extraction, not
invention. Each is a day's work when something does:

- resolution out of the SPS, and the VUI with it: aspect ratio, colour
  primaries, transfer and matrix. ffmpeg already hands all of these
  over out of band, which is where they come from today.
- the AV1 sequence header, so a key frame could be found without
  leaning on the sequence header being repeated before one.
- closed captions: the `cc_data` of an `ATSC1_data` SEI, and the
  ITU-T T.35 metadata OBU that spells the same thing for AV1.
- HEVC parameter sets out of an `hvcC`, and building one. Only the
  length size is read today.
- the `av1C` record, which nothing here parses or builds.
- the length-delimited AV1 Annex B framing, which is a different
  framing around the same OBUs.

## Where each old function went

Phase 2 is the consuming crates. These are the mappings, and the only
places where behaviour changes are called out.

### ffrwd-index

`ffrwd_index_core::avc` and `::obu` keep their own module names and
become thin wrappers. The format's own constants, `UUID`,
`METADATA_TYPE` and `Unit`, stay where they are.

| was | is |
| --- | --- |
| `avc::Codec`, `H264_SEI`, `H265_PREFIX_SEI`, `H265_SUFFIX_SEI` | `h26x::` the same |
| `avc::PAYLOAD_TYPE_USER_DATA_UNREGISTERED` | `sei::USER_DATA_UNREGISTERED` |
| `avc::NalRef`, `scan_nals`, `split_nals` | `annexb::` the same |
| `avc::AccessUnit`, `access_units` | `h26x::` the same |
| `avc::remove_emulation_prevention`, `insert_emulation_prevention` | `ep::` the same |
| `avc::SeiMessage`, `parse_sei`, `parse_sei_rbsp`, `write_sei` | `sei::` the same |
| `avc::wrap_unit_at(unit, codec, tid)` | `sei::write_user_data_at(unit, codec, tid)` |
| `avc::wrap_unit(unit, codec)` | `sei::write_user_data(unit, codec)` |
| `avc::units_in_nal(nal, codec)` | `sei::user_data_in_nal(nal, codec, &UUID)` |
| `avc::units_annexb(bytes, codec)` | `sei::user_data_annexb(bytes, codec, &UUID)` |
| `avc::units_length_prefixed(s, n, codec)` | `sei::user_data_length_prefixed(s, n, codec, &UUID)` |
| `avc::insert_sei_annexb`, `insert_sei_length_prefixed` | `sei::` the same |
| `avc::avcc_length_size`, `hvcc_length_size` | `config::` the same |
| `avc::annexb_to_length_prefixed`, `length_prefixed_to_annexb`, `split_length_prefixed` | `annexb::` the same |
| `obu::OBU_*`, `ObuRef`, `scan_obus`, `leb128`, `put_leb128`, `TemporalUnit`, `temporal_units`, `insert_metadata_obu` | `obu::` the same |
| `obu::write_metadata_obu(unit)` | `obu::write_metadata(METADATA_TYPE, unit)` |
| `obu::unit_in_obu(o)` | `obu::metadata_payload(o, METADATA_TYPE)`, then the caller's own `Unit::is_ours` |
| `obu::units_obu(bytes)` | `obu::user_data(bytes, METADATA_TYPE, &UUID)` |
| `live::StreamKind`, `Carried`, `Feed`, `DEFAULT_LIMIT` | `feed::` the same |
| `live::Feed::new(kind)` | `feed::Feed::new(kind, Select::new(UUID, METADATA_TYPE))` |
| `live::Carried::units` | `feed::Carried::payloads` |
| `rows::stream::Framing`, `framing_of`, `CODECS` | `config::` the same |
| `rows::stream::insert(framing, packet, unit)` | `framing.insert(packet, unit, SELECT)` |
| `rows::stream::units_in(framing, packet)` | `framing.payloads(packet, SELECT)` |
| `rows::stream::temporal_id_annexb` | gone, inside `Framing::insert` |
| `container::scan::units_in` | `Framing::payloads` and `Framing::payloads_in_nal` |

Two things to change in the index crate itself. Its `Error` keeps the
variants the rest of the format needs, and gains a `From<ffrwd_nal::Error>`
that maps `Truncated { .. }` to `Truncated`, `TooLarge { .. }` to
`TooLarge`, `Malformed { what, .. }` to `Malformed(what)` and
`UnknownCodec` to the message `framing_of` used to format. And
`container::scan`'s `lead_end_nals` and `lead_end_obus` stay where they
are: finding where a half-read sample's leading NALs end is the
container reader's own problem, not this crate's.

### ffrwd-moq

| was | is |
| --- | --- |
| `avc::split_nals` | `annexb::split_nals` |
| `avc::annexb_to_avcc(bytes)` | `annexb::annexb_to_length_prefixed(bytes, 4)?` |
| `avc::avcc_to_annexb(sample, n)` | `annexb::length_prefixed_to_annexb(sample, n)?` |
| `avc::parse_parameter_sets`, `build_avcc`, `avcc_to_annexb_extradata`, `avcc_length_size` | `config::` the same |
| `avc::BitReader` | `sps::BitReader`, now public |
| `avc::parse_sps_formats(nal)` | `sps::formats(nal)`, returning `sps::Formats` |
| `catalog::avc_codec` | `codec_string::avc_codec` |
| `catalog::avc_codec_from(profile: i32, level: i32, avcc)` | `codec_string::avc_codec_from(profile: u8, level: u8, avcc)` |
| `catalog::aac_codec` | stays in the moq package: it reads an audio config, not a NAL |

Behaviour to expect. Every function that returned `Result<_, String>`
now returns `Result<_, ffrwd_nal::Error>`, so the call sites that end
in `?` inside a `String`-error function need a `.map_err(|e|
e.to_string())` or a `From` impl. `annexb_to_avcc` used to return an
empty `Vec` for a packet with no start code and it still does, but it
returns it inside an `Ok`. `build_avcc` now refuses more than 31 SPS,
more than 255 PPS and a parameter set longer than 65535 bytes, each of
which the old code wrote as a truncated count or length. `avc_codec_from`
takes `u8`, so a caller holding ffmpeg's `i32` casts at the call site,
which is where the old code cast anyway.

### ffrwd-cli

| was | is |
| --- | --- |
| `ffrwd-wasm`'s `h264_profile_level(extradata)` | `sps::profile_level(extradata)`, returning `Option<(u8, u8)>` |
| `packet-sei`'s `Framing::read(extradata)` | `config::framing_of("h264", extradata)?` |
| `packet-sei`'s `Framing::frame` and `sei_nal`, then prepending | `framing.insert(packet, payload, SELECT)?` |

Behaviour to expect. `profile_level` hands back the two bytes rather
than two `i32`, so the NUT bridge writes `i32::from` at the one call
site that wants ffmpeg's width. `framing_of` wants 7 bytes of `avcC`
where `Framing::read` wanted 5, which no real record fails. And
`Framing::insert` puts the SEI before the first coded slice rather than
at the very front of the packet, which is where a prefix SEI belongs
and what the filter's own doc comment already claims it does; the bytes
of the NAL itself are identical, since that payload needs no escaping.

## Building and testing

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32-wasip2
```

`src/` carries the unit tests, including the escape round trips and a
truncation and random byte loop over every parser. `tests/reference.rs`
runs against real encoders: `tests/data/` holds two seconds of testsrc2
from libx264, libx265 and libsvtav1, ffmpeg's own `avcC` and `hvcC` for
the first two, and the Annex B extradata its H.264 demuxer hands out of
band. Everything asserted about those files is read out of them, and
every payload the crate finds is found a second time by a hand-written
scanner in the test file that shares no code with the crate.

The tests that need ffmpeg say they are skipping when it is not on the
PATH rather than passing quietly. With it there, they check that a
woven stream decodes to the same frames by `framemd5`, that ffmpeg and
ffprobe have nothing new to say about it, that the payloads survive a
remux through MP4, Matroska and MPEG-TS and back, and that ffmpeg's own
`trace_headers` sees the SEI and reads the UUID out of it.

`scripts/fixtures.sh` regenerates `tests/data/` and is the record of
exactly which ffmpeg commands made it.

## License

Apache-2.0.
