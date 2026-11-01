//! Engine randomness for tick code: counter-based streams with no saved state.
//!
//! A stream is `SplitMix64` started from a mix of the world seed, the engine tick and a stream key,
//! so the same world, tick and key give the same numbers after a restart, in a replay and on any
//! pool thread. A key is a [`tag`] (one per purpose) mixed with the position, entity or session
//! it is for; one tick never opens the same key twice.

/// The `SplitMix64` increment, also the offset of its finaliser [`mix`].
const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// A random stream.
#[derive(Debug, Clone)]
pub(crate) struct Random(u64);

impl Random {
    /// The stream of `key` in the world with `seed` at engine tick `tick`.
    pub(crate) fn new(seed: i64, tick: u64, key: u64) -> Self {
        Self(mix(mix(mix(seed.cast_unsigned()) ^ tick) ^ key))
    }

    /// Draws the next 64 random bits (`SplitMix64`).
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GOLDEN);
        mix(self.0)
    }

    /// Draws a number in `0..n` (`0` when `n` is 0), by multiply-high: the bias is below n / 2^64.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "invariant: the high half of a u64 × u32 product is below 2^32"
    )]
    pub(crate) fn below(&mut self, n: u32) -> u32 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u32
    }

    /// Draws a number in `lo..=hi` (`lo` when `hi < lo`).
    pub(crate) fn range(&mut self, lo: i32, hi: i32) -> i32 {
        let span = hi.wrapping_sub(lo).cast_unsigned();
        if hi < lo {
            return lo;
        }
        if span == u32::MAX {
            let b = self.next_u64().to_le_bytes();
            return i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        lo.wrapping_add(self.below(span + 1).cast_signed())
    }

    /// Draws true with probability `numerator / denominator` (false when `denominator` is 0).
    pub(crate) fn chance(&mut self, numerator: u32, denominator: u32) -> bool {
        denominator != 0 && self.below(denominator) < numerator
    }

    /// Draws a float in `[0, 1)` with 53 random bits.
    #[expect(
        clippy::cast_precision_loss,
        reason = "invariant: a 53-bit integer is exact in f64"
    )]
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1_u64 << 53) as f64)
    }
}

/// FNV-1a 64: a hash whose output is fixed by its definition, unlike `DefaultHasher` (the
/// purpose keys of [`tag`], the state hash of the tests and the input log's commit identity).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fnv(pub(crate) u64);

impl Fnv {
    /// The state of a hash of nothing.
    pub(crate) const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    /// The multiplier of every byte.
    const PRIME: u64 = 0x0100_0000_01b3;

    /// A hash of nothing.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self(Self::OFFSET)
    }

    /// Mixes `bytes` into the hash.
    pub(crate) const fn raw(&mut self, mut bytes: &[u8]) {
        while let [b, rest @ ..] = bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(Self::PRIME);
            bytes = rest;
        }
    }

    /// Mixes the little-endian bytes of `v` into the hash.
    pub(crate) const fn u64(&mut self, v: u64) {
        self.raw(&v.to_le_bytes());
    }

    /// Mixes `v` as a `u64` into the hash.
    #[cfg(test)]
    pub(crate) fn usize(&mut self, v: usize) {
        self.u64(u64::try_from(v).unwrap_or(u64::MAX));
    }

    /// Mixes `bytes` with their length in front, so adjacent fields cannot run together.
    #[cfg(test)]
    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.usize(bytes.len());
        self.raw(bytes);
    }
}

/// The key of a purpose, from its name ([`Fnv`]), computed at compile time:
/// `const CROP_GROWTH: u64 = tag("crop-growth");`.
pub(crate) const fn tag(name: &str) -> u64 {
    let mut h = Fnv(Fnv::OFFSET);
    h.raw(name.as_bytes());
    h.0
}

/// A key made of a purpose `tag` and the values it is for (positions, IDs, sessions).
pub(crate) fn key(tag: u64, parts: &[u64]) -> u64 {
    parts.iter().fold(mix(tag), |k, &p| mix(k ^ p))
}

/// The `SplitMix64` finaliser.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(GOLDEN);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_tick_and_key_give_the_same_numbers() {
        let k = key(tag("test"), &[1, 2, 3]);
        let (mut ra, mut rb) = (Random::new(7, 9, k), Random::new(7, 9, k));
        let a: Vec<u64> = (0..8).map(|_| ra.next_u64()).collect();
        let b: Vec<u64> = (0..8).map(|_| rb.next_u64()).collect();
        assert_eq!(a, b);
        assert_ne!(Random::new(7, 10, k).next_u64(), a[0]);
        assert_ne!(
            Random::new(7, 9, key(tag("test"), &[1, 2, 4])).next_u64(),
            a[0]
        );
    }

    #[test]
    fn bounded_draws_stay_in_range() {
        let mut r = Random::new(1, 2, 3);
        for _ in 0..10_000 {
            assert!(r.below(7) < 7);
            let v = r.range(-3, 3);
            assert!((-3..=3).contains(&v));
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
        assert_eq!(r.below(0), 0);
        assert_eq!(r.range(5, 1), 5);
        assert!(!r.chance(1, 0));
        r.range(i32::MIN, i32::MAX);
    }
}
