//! The RFC 6381 name of a coded stream, which is what a catalog, a
//! WebCodecs configuration and an HLS or DASH manifest all call a codec.
//!
//! Only the H.264 spelling is here. The AAC one lives with the audio
//! configuration it reads, and the HEVC one nothing on this machine
//! writes yet.

/// The RFC 6381 codec string an `avcC` record spells: the profile, the
/// constraint flags and the level, which are its bytes 1 through 3. The
/// fallback for a stream that names no profile or level of its own.
pub fn avc_codec(avcc: &[u8]) -> String {
    match avcc.get(1..4) {
        Some(triple) => format!("avc1.{:02x}{:02x}{:02x}", triple[0], triple[1], triple[2]),
        None => "avc1".to_string(),
    }
}

/// The same codec string from the stream's own profile and level, the
/// preferred source: a stream whose extradata is empty still names them.
/// The constraint flags are the one byte they do not carry, read off the
/// `avcC` where it has one and zero otherwise.
pub fn avc_codec_from(profile: u8, level: u8, avcc: &[u8]) -> String {
    let constraints = avcc.get(2).copied().unwrap_or(0);
    format!("avc1.{profile:02x}{constraints:02x}{level:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codec_string_is_the_avcc_profile_and_level() {
        // avcC: configurationVersion, profile 0x64, compat 0x00, level 0x1f.
        assert_eq!(avc_codec(&[1, 0x64, 0x00, 0x1f, 0xff]), "avc1.64001f");
    }

    #[test]
    fn an_unreadable_avcc_still_names_the_codec() {
        assert_eq!(avc_codec(&[1, 0x64]), "avc1");
        assert_eq!(avc_codec(&[]), "avc1");
    }

    #[test]
    fn the_streams_own_profile_and_level_agree_with_the_avcc() {
        // An intact avcC and the stream's numbers spell the same string;
        // the middle byte is the avcC's, the outer two the stream's.
        let avcc = [1u8, 0x64, 0x00, 0x1f, 0xff];
        assert_eq!(avc_codec_from(0x64, 0x1f, &avcc), avc_codec(&avcc));
        assert_eq!(avc_codec_from(0x64, 0x1f, &avcc), "avc1.64001f");
        // Constrained baseline: the constraint flags come off the avcC.
        assert_eq!(
            avc_codec_from(66, 30, &[1, 0x42, 0xc0, 0x1e]),
            "avc1.42c01e"
        );
    }

    #[test]
    fn without_an_avcc_the_constraint_flags_read_zero() {
        assert_eq!(avc_codec_from(0x64, 0x28, &[]), "avc1.640028");
    }
}
