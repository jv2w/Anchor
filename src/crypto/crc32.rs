//! CRC-32 (IEEE 802.3, reflected), computed bit by bit.
//!
//! It serves the zip entry check of resource packs and the ICE candidate foundation of NetherNet
//! answers. Both hash a few small inputs, once each, so a table buys nothing.

/// The IEEE 802.3 polynomial, bit-reflected.
const POLYNOMIAL: u32 = 0xedb8_8320;

/// The CRC-32 of `parts` one after another.
pub(crate) fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0_u32;
    for &byte in parts.iter().flat_map(|part| part.iter()) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_check_value_over_any_split() {
        assert_eq!(crc32(&[b"123456789"]), 0xcbf4_3926);
        assert_eq!(crc32(&[b"1234", b"", b"56789"]), 0xcbf4_3926);
        assert_eq!(crc32(&[b""]), 0);
        assert_eq!(crc32(&[]), 0);
    }
}
