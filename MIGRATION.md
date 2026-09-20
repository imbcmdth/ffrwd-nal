# Moving to ffrwd-nal

Where each function in the ffrwd packages that this crate was
consolidated from now lives, and what changes when it moves.

## ffrwd-index

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
variants the rest of the format needs and gains a
`From<ffrwd_nal::Error>` that maps `Truncated { .. }` to `Truncated`,
`TooLarge { .. }` to `TooLarge`, `Malformed { what, .. }` to
`Malformed(what)` and `UnknownCodec` to the message `framing_of` used
to format. And `container::scan`'s `lead_end_nals` and `lead_end_obus`
stay where they are: finding where a half-read sample's leading NALs
end is the container reader's own problem, not this crate's.

The call sites are `core/src/{avc,obu,live}.rs`, `rows/src/stream.rs`,
`container/src/scan.rs` and `tool/src/main.rs`, plus the tests in
`rows/src/read.rs`, `container/tests/containers.rs` and `tool/tests/`.
All of them go through the functions above, so wrapping `avc` and `obu`
is enough to move the package.

A `NalRef` now carries a `pad` offset as well as a `code` offset, and
`AccessUnit::start` and `end` are the padding offsets rather than the
start code offsets, so the units still tile a stream that has padding
in it. `insert_at` is still the start code, so every splice lands where
it landed before.

## ffrwd-moq

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

Behaviour to expect:

- **The `avcC` the muxer builds gets one byte shorter, and it is now
  ffmpeg's own record.** ffmpeg's H.264 demuxer pads the SPS in its
  Annex B extradata with a `trailing_zero_8bits`, and the old scanner
  gave that byte to the SPS, so every `avcC` moq has written carries a
  27 byte SPS where ffmpeg's MP4 muxer writes 26. The byte is padding
  and no decoder reads it, but the record was not ffmpeg's. The scanner
  here follows Annex B.1.1 and the record built from the same extradata
  is now equal to ffmpeg's, which `tests/reference.rs` asserts against
  the real thing. Any golden file holding the old record needs updating
  by that one byte and the length in front of it.
- Every function that returned `Result<_, String>` now returns
  `Result<_, ffrwd_nal::Error>`, so call sites that end in `?` inside a
  `String`-error function need a `.map_err(|e| e.to_string())` or a
  `From` impl.
- `annexb_to_avcc` returned an empty `Vec` for a packet with no start
  code and still does, but it returns it inside an `Ok`.
- `build_avcc` refuses more than 31 SPS, more than 255 PPS and a
  parameter set longer than 65535 bytes, each of which the old code
  wrote as a truncated count or length.
- `avc_codec_from` takes `u8`, so a caller holding ffmpeg's `i32` casts
  at the call site, which is where the old code cast anyway.

## ffrwd-cli

| was | is |
| --- | --- |
| `ffrwd-wasm`'s `h264_profile_level(extradata)` | `sps::profile_level(extradata)`, returning `Option<(u8, u8)>` |
| `packet-sei`'s `Framing::read(extradata)` | `config::framing_of("h264", extradata)?` |
| `packet-sei`'s `Framing::frame` and `sei_nal`, then prepending | `framing.insert(packet, payload, SELECT)?` |

Behaviour to expect:

- `profile_level` hands back the two bytes rather than two `i32`, so
  the NUT bridge writes `i32::from` at the one call site that wants
  ffmpeg's width.
- `framing_of` wants 7 bytes of `avcC` where `Framing::read` wanted 5,
  which no real record fails.
- `Framing::insert` puts the SEI before the first coded slice rather
  than at the very front of the packet, which is where a prefix SEI
  belongs and what the filter's own doc comment already claims it does.
  The bytes of the NAL itself are identical, since that payload needs
  no escaping.
