//! The byte level of H.264, HEVC and AV1: where the NAL units and OBUs
//! of a coded stream begin and end, which of them open a picture, and
//! how to read a payload out of one or put a payload into one without
//! moving anything else.
//!
//! Four ffrwd crates were carrying their own copy of this and the copies
//! had drifted. What is here is those copies reconciled: one start code
//! scanner, one emulation prevention pair, one SEI reader and writer,
//! one OBU reader and writer, one `avcC` builder, one exp-Golomb reader.
//! Nothing above the byte level is here. There are no boxes, no
//! containers, no timestamps and no index format: a caller brings its
//! own UUID or `metadata_type` and this crate finds and places the bytes
//! that carry it.
//!
//! The crate has no dependencies, forbids unsafe code, opens no file and
//! starts no thread, so a wasm module compiles it in and its tests run
//! on the host. Every decoder returns [`Error`] rather than panicking,
//! and nothing allocates from a length that untrusted bytes supplied
//! before that length has been checked against the bytes in hand.
//!
//! The modules follow the standards:
//!
//! - [`annexb`]: start codes and length prefixes, ISO/IEC 14496-10
//!   Annex B and 14496-15 section 5.
//! - [`h26x`]: NAL headers, slice types and access unit boundaries,
//!   14496-10 section 7 and 23008-2 section 7.
//! - [`ep`]: emulation prevention, 14496-10 section 7.4.1.1.
//! - [`sei`]: SEI messages, 14496-10 Annex D.
//! - [`config`]: the `avcC` and `hvcC` records and the framing they
//!   declare, 14496-15 sections 5.3.3 and 8.3.3.
//! - [`sps`]: exp-Golomb and the sequence parameter set fields an
//!   `avcC` repeats, 14496-10 section 7.3.2.1.
//! - [`codec_string`]: the RFC 6381 name of a coded stream.
//! - [`obu`]: AV1 open bitstream units, AV1 specification section 5.
//! - [`feed`]: the same boundaries found a chunk at a time, for a
//!   stream that is still being written.

#![forbid(unsafe_code)]

pub mod annexb;
pub mod codec_string;
pub mod config;
pub mod ep;
pub mod feed;
pub mod h26x;
pub mod obu;
pub mod sei;
pub mod sps;

pub use h26x::Codec;

/// What went wrong reading or building bytes of a coded stream.
///
/// One enum for the crate, and no allocation on the error path: a reader
/// that hits any of these drops what it was in the middle of and carries
/// on, which is what a reader of a socket has to do, and it can say
/// where the trouble was without anyone having formatted a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes ran out before the value did. `at` is where the value
    /// began.
    Truncated { at: usize },
    /// The bytes are not the shape the standard requires. `at` is where
    /// the offending value began.
    Malformed { what: &'static str, at: usize },
    /// A value too large for the field it has to go in: a NAL longer
    /// than its length prefix can spell, or an OBU wider than memory.
    /// `at` is the offset the value belongs at, in the input when a
    /// reader found it and in the output when a writer did.
    TooLarge { at: usize },
    /// A codec name this crate has no framing for.
    UnknownCodec,
}

impl Error {
    /// Where the trouble was, when the bytes say.
    pub fn at(self) -> Option<usize> {
        match self {
            Error::Truncated { at } | Error::Malformed { at, .. } | Error::TooLarge { at } => {
                Some(at)
            }
            Error::UnknownCodec => None,
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Truncated { at } => write!(f, "the bytes end inside a value at byte {at}"),
            Error::Malformed { what, at } => write!(f, "{what}, at byte {at}"),
            Error::TooLarge { at } => {
                write!(f, "a value at byte {at} too large for the field it goes in")
            }
            Error::UnknownCodec => write!(f, "a codec with no NAL or OBU framing here"),
        }
    }
}

impl std::error::Error for Error {}

/// The crate's result.
pub type Result<T> = core::result::Result<T, Error>;

/// Which of a carrier's payloads are the caller's own.
///
/// Every format that rides inside a coded stream has to tell its own
/// payloads from the encoder's, and both spellings do it the same way:
/// an H.264 or HEVC payload is a `user_data_unregistered` SEI message
/// opened by a UUID, and an AV1 payload is a metadata OBU of a chosen
/// `metadata_type` whose body opens by the same UUID. Nothing else in
/// the stream is looked at, let alone changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Select {
    /// The 16 byte `uuid_iso_iec_11578` a payload opens with.
    pub uuid: [u8; 16],
    /// The AV1 `metadata_type` the payload's OBU carries. The AV1
    /// specification leaves 6 to 31 unregistered for private use.
    pub metadata_type: u64,
}

impl Select {
    /// The payloads opened by `uuid`, carried in AV1 metadata OBUs of
    /// `metadata_type`.
    pub const fn new(uuid: [u8; 16], metadata_type: u64) -> Self {
        Self {
            uuid,
            metadata_type,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_says_where_it_was() {
        assert_eq!(Error::Truncated { at: 9 }.at(), Some(9));
        assert_eq!(
            Error::Malformed {
                what: "a NAL overruns the sample",
                at: 12
            }
            .at(),
            Some(12)
        );
        assert_eq!(Error::UnknownCodec.at(), None);
        assert_eq!(
            Error::Malformed {
                what: "a NAL overruns the sample",
                at: 12
            }
            .to_string(),
            "a NAL overruns the sample, at byte 12"
        );
    }
}
