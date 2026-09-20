//! Emulation prevention, ISO/IEC 14496-10 section 7.4.1.1: the
//! `emulation_prevention_three_byte` that keeps a NAL's payload from
//! growing a start code in the middle of itself.
//!
//! A NAL's bytes on the wire are the `rbsp` with a `03` pushed in
//! wherever two zeroes are followed by a byte of 0 to 3. Reading undoes
//! it; writing does it. Escaping is what a writer here does by default,
//! and a payload that holds no `00 00 0x` sequence comes back from
//! [`insert_emulation_prevention`] unchanged, so a caller who knows its
//! payload cannot grow one has nothing to skip and no second spelling
//! to choose between.

/// Removes emulation prevention bytes: `00 00 03` becomes `00 00`.
pub fn remove_emulation_prevention(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeros = 0u32;
    for byte in bytes {
        if zeros >= 2 && *byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if *byte == 0 { zeros + 1 } else { 0 };
        out.push(*byte);
    }
    out
}

/// Inserts emulation prevention bytes, so no `00 00 00`, `00 00 01`,
/// `00 00 02` or `00 00 03` survives into the byte stream.
///
/// This is what keeps a payload full of zeroes from growing a start
/// code in the middle of an SEI and cutting the stream in half. A
/// payload with no such sequence in it is returned byte for byte.
pub fn insert_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 16 + 4);
    let mut zeros = 0u32;
    for byte in rbsp {
        if zeros >= 2 && *byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if *byte == 0 { zeros + 1 } else { 0 };
        out.push(*byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_round_trips_the_worst_payloads() {
        let patterns: [&[u8]; 8] = [
            &[0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 0, 1, 0, 0, 1, 0, 0, 1],
            &[0, 0, 3, 0, 0, 3, 0, 0, 3],
            &[0, 0, 2],
            &[1, 0, 0, 0, 1, 0, 0, 0, 1],
            &[0xff, 0, 0, 0xff],
            &[0],
            &[],
        ];
        for pattern in patterns {
            let escaped = insert_emulation_prevention(pattern);
            assert_eq!(
                remove_emulation_prevention(&escaped),
                pattern,
                "pattern {pattern:?}"
            );
            assert!(
                !escaped.windows(3).any(|w| w == [0, 0, 1]),
                "an escaped payload grew a start code: {escaped:?}"
            );
        }
    }

    #[test]
    fn escaping_a_payload_that_needs_none_returns_it_unchanged() {
        // The reason there is no second, unescaped spelling of a write.
        // A caller whose payload cannot hold `00 00 0x` - a UUID with no
        // zero byte in it and ASCII behind it, which is what the sei
        // packet filter writes - gets the same bytes either way.
        let mut payload = b"ffrwd-sei-test!!".to_vec();
        payload.extend_from_slice(b"a note, and another\nand a third");
        assert_eq!(insert_emulation_prevention(&payload), payload);

        // The rule in general: no two zeroes followed by a 0 to 3 byte.
        let clear: [&[u8]; 5] = [
            &[],
            &[0x00],
            &[0x00, 0x00, 0x04],
            &[0x00, 0x04, 0x00, 0x04],
            &[0xff; 64],
        ];
        for payload in clear {
            assert_eq!(insert_emulation_prevention(payload), payload);
        }
        // And one that does need escaping does not come back unchanged.
        assert_ne!(
            insert_emulation_prevention(&[0x00, 0x00, 0x03]),
            vec![0x00, 0x00, 0x03]
        );
    }

    #[test]
    fn random_bytes_survive_the_round_trip() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..3000 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 48 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                // A third of the bytes are zeroes, so the worst
                // sequences turn up often.
                bytes.push(match (seed >> 33) % 3 {
                    0 => 0,
                    _ => (seed >> 41) as u8,
                });
            }
            let escaped = insert_emulation_prevention(&bytes);
            assert_eq!(remove_emulation_prevention(&escaped), bytes);
            assert!(!escaped.windows(3).any(|w| w == [0, 0, 1]));
            let _ = remove_emulation_prevention(&bytes);
        }
    }
}
