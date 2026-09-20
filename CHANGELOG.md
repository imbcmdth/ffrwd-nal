# Changelog

## 0.1.1

- `config::framing_of_entry(kind, config)` reads the framing off a
  container's sample entry: `avc1`, `avc3`, `hvc1` and `hev1` are
  length-prefixed with the width taken from the record, `av01` is
  low-overhead OBUs, and anything else is `UnknownCodec`. A record too
  short or too damaged to declare its width is an error rather than a
  fall back to Annex B, which the sample entry has already ruled out.
- `config::framing_of` also takes `avc3` and `av01` as codec names. It
  reads them as names for the codec, as it already read `avc1`, `hvc1`
  and `hev1`; the extradata still decides the framing.

## 0.1.0

First release: the NAL, OBU and SEI byte level consolidated from
ffrwd's index and moq packages and the ffrwd sidecar, which had four
copies of it between them.

- `annexb`: start code scanning with offsets, and reframing between
  start codes and length prefixes of one to four bytes.
- `h26x`: NAL headers, slice types, access unit boundaries, keyframes
  and temporal ids for H.264 and HEVC.
- `ep`: emulation prevention, both directions.
- `sei`: SEI messages of any payload type, and finding, building and
  placing `user_data_unregistered` payloads by UUID.
- `config`: the `avcC` and `hvcC` records, the parameter sets, and the
  framing a stream is in.
- `sps`: an exp-Golomb reader and the SPS fields an `avcC` repeats.
- `codec_string`: the RFC 6381 name of an H.264 stream.
- `obu`: AV1 OBUs, temporal units, and metadata OBUs of any
  `metadata_type`.
- `feed`: the same boundaries found a chunk at a time.
