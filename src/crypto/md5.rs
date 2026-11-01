//! MD5 (RFC 1321), only for name-based version 3 UUIDs (gophertunnel `identityFromXUID`).
//! aws-lc-rs has no MD5, and the input is a short public string, so speed and collision
//! resistance do not matter here.

/// Bytes in a message block.
const BLOCK_BYTES: usize = 64;
/// Where the message length starts in the last block: the length fills its final 8 bytes.
const LENGTH_OFFSET_BYTES: usize = 56;
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];
const SINES: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// The MD5 digest of the concatenation of `parts`.
pub(crate) fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut message = parts.concat();
    let bits = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % BLOCK_BYTES != LENGTH_OFFSET_BYTES {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_le_bytes());
    let mut state = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in message.as_chunks::<BLOCK_BYTES>().0 {
        compress(&mut state, block);
    }
    let mut digest = [0_u8; 16];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *chunk = word.to_le_bytes();
    }
    digest
}

/// Mixes one message block into the state.
fn compress(state: &mut [u32; 4], block: &[u8; BLOCK_BYTES]) {
    let mut words = [0_u32; 16];
    for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    // RFC 1321 names the registers A, B, C, D and the round function F with message index g.
    let [mut ra, mut rb, mut rc, mut rd] = *state;
    for (i, (&shift, &sine)) in SHIFTS.iter().zip(&SINES).enumerate() {
        let (mixed, index) = match i / 16 {
            0 => ((rb & rc) | (!rb & rd), i),
            1 => ((rd & rb) | (!rd & rc), (5 * i + 1) % 16),
            2 => (rb ^ rc ^ rd, (3 * i + 5) % 16),
            _ => (rc ^ (rb | !rd), (7 * i) % 16),
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: every message index is reduced modulo the 16 words"
        )]
        let word = words[index];
        let rotated = ra
            .wrapping_add(mixed)
            .wrapping_add(sine)
            .wrapping_add(word)
            .rotate_left(shift);
        (ra, rd, rc) = (rd, rc, rb);
        rb = rb.wrapping_add(rotated);
    }
    for (register, value) in state.iter_mut().zip([ra, rb, rc, rd]) {
        *register = register.wrapping_add(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::crypto::tests::hex;

    #[test]
    fn digests_match_rfc_1321_vectors() {
        let vectors = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, digest) in vectors {
            assert_eq!(hex(&md5(&[input.as_bytes()])), digest, "{input}");
        }
        assert_eq!(
            md5(&[b"mess", b"age ", b"digest"]),
            md5(&[b"message digest"])
        );
    }
}
