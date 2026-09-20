# ffrwd-nal

The byte level of H.264, HEVC and AV1: where the NAL units and OBUs of
a coded stream begin and end, which of them open a picture, and how to
read a payload out of one or put a payload into one without moving
anything else. No dependencies, no unsafe code, no I/O, so a wasm
module compiles it in and its tests run on the host.

The code was consolidated from ffrwd's index and moq packages and the
ffrwd sidecar, which had four copies of it between them, drifted apart.
Where the copies disagreed, the decision and the reason are written
down under [the rules not to simplify](#the-rules-not-to-simplify), and
[MIGRATION.md](MIGRATION.md) says where each old function went.

```rust
use ffrwd_nal::config::framing_of;
use ffrwd_nal::Select;

// The UUID a payload opens with, and the AV1 metadata_type it rides in.
const MINE: Select = Select::new([0x04, 0x1f, /* ... */ 0x66], 25);

let framing = framing_of("h264", &stream.extradata)?;
let packet = framing.insert(&packet, &my_payload, MINE)?;
// and on the way back
for payload in framing.payloads(&packet, MINE) { /* ... */ }
```

## Using it

```toml
[dependencies]
ffrwd-nal = { git = "https://github.com/imbcmdth/ffrwd-nal", tag = "v0.1.1" }
```

[CHANGELOG.md](CHANGELOG.md) says what each tag changed.

## What it does

Cutting a stream into its pieces. `annexb::scan_nals` gives every NAL
unit with the offset of its start code, `h26x::access_units` groups
them into pictures and says which are random access points,
`obu::scan_obus` and `obu::temporal_units` do the same for AV1, and
`feed::Feed` finds the same boundaries a chunk at a time for a stream
that is still being written. `annexb` reframes between start codes and
length prefixes of one to four bytes, which is the difference between
an elementary stream and an MP4 sample.

Carrying a payload of your own. A format that rides inside a coded
stream declares a UUID and, for AV1, a `metadata_type`, which is what
`Select` holds. `sei` finds, builds and places `user_data_unregistered`
SEI messages of any payload type; `obu` finds, builds and places
metadata OBUs of any `metadata_type`. Nothing else in the packet is
read and nothing else is moved: an encoder's own settings SEI, captions
and HDR metadata travel on untouched, byte for byte, which the tests
check against real x264 output.

Reading the two records a NAL reader needs. `config` takes the length
prefix width and the parameter sets out of an `avcC` or `hvcC`, builds
an `avcC` back from parameter sets the way ffmpeg's own writer does,
and answers which framing a stream is in: `framing_of` from a pipeline
pad's codec name and extradata, `framing_of_entry` from a container's
sample entry, which knows more and is held to it. `sps` reads the few
SPS fields that record repeats, and `codec_string` spells the RFC 6381
name.

## What it leaves to the caller

Everything above the byte level. There are no boxes here and no
containers: a record is a byte slice, and which box carries it belongs
to a container crate. There are no timestamps, no index format, no UUID
of its own, and no opinion about what a payload means. The caller
brings the UUID and gets its own bytes back.

There is no dependency on `h264-parser`, `scuffle-h265`, `scuffle-av1`
or `mp4-atom`, which is what upstream `moq-mux` uses for the same job.
Those crates were read for reference. They are not used here because
this code compiles into wasm modules and into a packet filter that runs
inside a pipeline, where a parser crate's own dependency tree, its
`bytes` and `thiserror` and `tokio` neighbours, is a cost the callers
cannot spend, and because what those callers were already using fits in
1800 lines.

## The rules not to simplify

Each of these was a disagreement between the copies. Each has a test.

**One error type, and no allocation on the error path.** `Error` is a
`Copy` enum of four variants. The index copy carried
`Malformed(&'static str)` and the moq copy formatted messages like "a
NAL of 9 bytes overruns the sample at byte 12". The offsets were worth
keeping and the formatting was not, so `Truncated`, `Malformed` and
`TooLarge` all carry an `at` field and a reader that hits one can say
where without anyone having built a `String`.

**Zero bytes between NAL units belong to neither of them.** H.264 Annex
B.1.1 and HEVC Annex B.2.1 call them `trailing_zero_8bits`, and a NAL
unit cannot end in a zero byte, because its last byte always carries
`rbsp_trailing_bits` or the escape of a `cabac_zero_word`. So a
`NalRef`'s `end` excludes every zero before the next start code, and
its `pad` field is where those zeroes begin, which is also where the
NAL before it ended: the spans tile the stream. `code` stays at the
NAL's own start code, `zero_byte` included, and that is where a splice
goes, so an inserted NAL leaves the padding trailing whatever it
trailed and every original byte stays in its original order. One of the
old copies gave the padding to the NAL it followed, which made the
`avcC` built from ffmpeg's own Annex B extradata one byte longer than
the `avcC` ffmpeg builds from the same stream. Now the two records are
equal, and `tests/reference.rs` asserts it against the real ones.

**Start codes are three and four bytes, and the scan carries offsets.**
`split_nals` is the slice iterator on top of `scan_nals`. Bytes before
the first start code are ignored, as ffmpeg ignores them, and bytes
with no start code hold no NAL units rather than holding one.

**Length prefixes are one to four bytes everywhere.** The moq copy's
`annexb_to_avcc` was hard-wired to four with an unchecked `as u32`;
`annexb::annexb_to_length_prefixed` takes the size and answers
`TooLarge` when a NAL does not fit the prefix it was given. Anything
outside one to four is refused rather than guessed at.

**Escaping is the default on write, and there is no second spelling.**
The sidecar's SEI packet filter writes its NAL unescaped on purpose:
its UUID has no zero byte in it and its notes are ASCII, so the payload
cannot grow a start code. It does not need a special path, because
escaping a payload with no `00 00 0x` sequence in it returns that
payload unchanged. `escaping_a_payload_that_needs_none_returns_it_unchanged`
proves the rule and
`an_escape_free_payload_is_written_exactly_as_it_would_be_by_hand`
builds that filter's own NAL by hand and shows `sei::write_user_data`
gives the same bytes.

**A finder hands back the whole payload, UUID and all.** That is the
unit a carried format encodes and decodes, so the wrappers in the
calling crates stay one line each. `SeiMessage::user_data_body` is
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
ends before the field, which is what the writers these crates read
mean. `framing_of` is stricter about what counts as a record at all: 7
bytes for an `avcC` and 23 for an `hvcC`, both with a configuration
version of 1, and anything shorter is read as Annex B.

**The SPS reader reads what it read before.** Profile, level, chroma
format and the two bit depths, which are the fields an `avcC` extension
repeats. Nothing more.

## Not here yet

Nothing calling this crate needs these, and it was written by
consolidating code that existed rather than by writing new parsers.
Each is a day's work when something does need it:

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
