#!/bin/sh
# Regenerates tests/data/. Run it from the repository root with ffmpeg 9
# and ffprobe on the PATH; the files it writes are committed, so the
# tests run with nothing installed.
#
# The three elementary streams are two seconds of testsrc2 at 30 frames a
# second, one keyframe every thirty, B-frames on where the encoder has
# them. The AV1 clip is 320x176 because SVT-AV1 wants a height it can
# divide by eight and pads one that it cannot, which would leave the
# committed file at a size nobody asked for.
#
# The three records are ffmpeg's own: the mp4 muxer writes the avcC and
# the hvcC, and ffprobe hands them back as the stream's extradata, so
# nothing here parses a box. `ref-extradata.h264` is the same parameter
# sets as the h264 demuxer collects them, Annex B, which is what an
# encoder hands a filter out of band.
set -eu

data=tests/data
tmp=${TMPDIR:-/tmp}/ffrwd-nal-fixtures
rm -rf "$tmp"
mkdir -p "$tmp" "$data"

ffmpeg -hide_banner -nostdin -y -loglevel error \
  -f lavfi -i testsrc2=size=320x180:rate=30 -t 2 \
  -c:v libx264 -preset veryfast -g 30 -bf 2 -pix_fmt yuv420p \
  -f h264 "$data/ref.h264"

ffmpeg -hide_banner -nostdin -y -loglevel error \
  -f lavfi -i testsrc2=size=320x180:rate=30 -t 2 \
  -c:v libx265 -preset fast -pix_fmt yuv420p \
  -x265-params keyint=30:min-keyint=30:bframes=2:log-level=error \
  -f hevc "$data/ref.h265"

ffmpeg -hide_banner -nostdin -y -loglevel error \
  -f lavfi -i testsrc2=size=320x176:rate=30 -t 2 \
  -c:v libsvtav1 -preset 10 -crf 40 -g 30 -pix_fmt yuv420p \
  -f obu "$data/ref.obu"

# The extradata of a stream, as ffprobe dumps it: an xxd-shaped hex
# dump, cut back to its bytes.
extradata() {
  ffprobe -v error -show_entries stream=extradata -show_data \
    -of default=nw=1:nk=1 "$1" |
    sed -e 's/^[0-9a-f]*: //' -e 's/  .*$//' -e 's/ //g' |
    tr -d '\n' | xxd -r -p > "$2"
}

# The parameter sets the h264 demuxer collects, Annex B.
extradata "$data/ref.h264" "$data/ref-extradata.h264"

# The records the mp4 muxer builds out of those same parameter sets.
ffmpeg -hide_banner -nostdin -y -loglevel error \
  -i "$data/ref.h264" -c copy -f mp4 "$tmp/ref.mp4"
extradata "$tmp/ref.mp4" "$data/ref.avcc"

ffmpeg -hide_banner -nostdin -y -loglevel error \
  -i "$data/ref.h265" -c copy -f mp4 "$tmp/ref-hevc.mp4"
extradata "$tmp/ref-hevc.mp4" "$data/ref.hvcc"

rm -rf "$tmp"
ls -l "$data"
