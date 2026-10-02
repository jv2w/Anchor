//! Batch compression: zlib (raw DEFLATE) and Snappy (block format).

mod snappy;
mod zlib;

use std::fmt;

/// The algorithm that compresses batch bodies, announced to the client in `NetworkSettings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Compression {
    /// Raw DEFLATE without a zlib header. Network id 0. Smaller batches, more CPU.
    Zlib,
    /// Snappy block format. Network id 1. Less CPU, larger batches.
    ///
    /// gophertunnel notes an unconfirmed report that clients without AVX2 crash with Snappy;
    /// Anchor cannot observe official client crashes, so this is unverified.
    #[default]
    Snappy,
}

impl Compression {
    /// Returns the id that `NetworkSettings` carries for this algorithm.
    pub const fn network_id(self) -> u16 {
        match self {
            Self::Zlib => 0,
            Self::Snappy => 1,
        }
    }

    /// Returns the algorithm for a `NetworkSettings` id, or `None` for an unknown id.
    pub const fn from_network_id(id: u16) -> Option<Self> {
        match id {
            0 => Some(Self::Zlib),
            1 => Some(Self::Snappy),
            _ => None,
        }
    }

    /// Returns the first byte of a batch body compressed with this algorithm.
    pub(crate) const fn header(self) -> u8 {
        match self {
            Self::Zlib => 0x00,
            Self::Snappy => 0x01,
        }
    }
}

/// A compression or decompression failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The data, or its declared or inflated size, is larger than `limit` bytes.
    Size {
        /// The largest size accepted for this input.
        limit: usize,
    },
    /// The input ends inside an element.
    Truncated,
    /// The Snappy length prefix is longer than 5 bytes or larger than `u32::MAX`.
    Varint,
    /// A Snappy copy has offset 0.
    ZeroOffset,
    /// A Snappy copy reaches before the start of the output.
    OffsetOutOfRange,
    /// The Snappy output grows past its declared length.
    Overrun,
    /// The Snappy output ends shorter than its declared length.
    Underrun,
    /// Input remains after the stream ends.
    TrailingInput,
    /// The DEFLATE stream is invalid.
    Deflate,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Size { limit } => write!(f, "size exceeds the limit of {limit} bytes"),
            Self::Truncated => f.write_str("input ends inside an element"),
            Self::Varint => f.write_str("invalid snappy length prefix"),
            Self::ZeroOffset => f.write_str("snappy copy offset is zero"),
            Self::OffsetOutOfRange => f.write_str("snappy copy offset reaches before the output"),
            Self::Overrun => f.write_str("snappy output exceeds the declared length"),
            Self::Underrun => f.write_str("snappy output is shorter than the declared length"),
            Self::TrailingInput => f.write_str("input continues after the stream ends"),
            Self::Deflate => f.write_str("invalid deflate stream"),
        }
    }
}

impl std::error::Error for Error {}

/// The result of a compression or decompression.
pub type Result<T> = std::result::Result<T, Error>;

/// Reusable compression state for one algorithm.
pub(crate) enum Compressor {
    Zlib(zlib::Encoder),
    Snappy(snappy::Encoder),
}

impl Compressor {
    /// `zlib_level` must be in 1..=9; it is ignored for Snappy.
    pub(crate) fn new(compression: Compression, zlib_level: u8) -> Self {
        match compression {
            Compression::Zlib => Self::Zlib(zlib::Encoder::new(zlib_level)),
            Compression::Snappy => Self::Snappy(snappy::Encoder::new()),
        }
    }

    /// Appends the compressed `input` to `out`. Input longer than `limit` is a size error.
    pub(crate) fn compress(&mut self, input: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<()> {
        if input.len() > limit {
            return Err(Error::Size { limit });
        }
        match self {
            Self::Zlib(encoder) => encoder.compress(input, out),
            Self::Snappy(encoder) => encoder.compress(input, out),
        }
        Ok(())
    }
}

/// Reusable decompression state for one algorithm.
pub(crate) enum Decompressor {
    Zlib(zlib::Decoder),
    Snappy,
}

impl Decompressor {
    pub(crate) fn new(compression: Compression) -> Self {
        match compression {
            Compression::Zlib => Self::Zlib(zlib::Decoder::new()),
            Compression::Snappy => Self::Snappy,
        }
    }

    /// Replaces the contents of `out` with the decompressed `input`, at most `limit` bytes.
    ///
    /// On error the contents of `out` are unspecified.
    pub(crate) fn decompress(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<()> {
        match self {
            Self::Zlib(decoder) => decoder.decompress(input, out, limit),
            Self::Snappy => snappy::decompress(input, out, limit),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Compression, Compressor, Decompressor, Error};
    use crate::protocol::io::tests::Rng;

    pub(crate) const ROUND_TRIP_SIZES: [usize; 6] = [0, 1, 255, 256, 64 * 1024, 1024 * 1024];
    const LIMIT: usize = 16 * 1024 * 1024;

    /// Random bytes when `repetitive` is false; otherwise runs and repeated phrases.
    pub(crate) fn sample(len: usize, repetitive: bool, seed: u64) -> Vec<u8> {
        let mut rng = Rng(seed);
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            let word = rng.next().to_le_bytes();
            if repetitive {
                match word[0] % 3 {
                    0 => data.extend(std::iter::repeat_n(word[1] % 4, usize::from(word[2]))),
                    1 => data.extend_from_slice(b"minecraft:stone"),
                    _ => data.extend_from_slice(&word[..usize::from(word[3] % 8)]),
                }
            } else {
                data.extend_from_slice(&word);
            }
        }
        data.truncate(len);
        data
    }

    fn compress(compression: Compression, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Compressor::new(compression, 6)
            .compress(input, &mut out, LIMIT)
            .unwrap();
        out
    }

    fn decompress(compression: Compression, input: &[u8], limit: usize) -> super::Result<Vec<u8>> {
        let mut out = Vec::new();
        Decompressor::new(compression).decompress(input, &mut out, limit)?;
        Ok(out)
    }

    #[test]
    fn network_ids_round_trip() {
        for compression in [Compression::Zlib, Compression::Snappy] {
            assert_eq!(
                Compression::from_network_id(compression.network_id()),
                Some(compression)
            );
        }
        assert_eq!(Compression::Zlib.network_id(), 0);
        assert_eq!(Compression::Snappy.network_id(), 1);
        assert_eq!(Compression::from_network_id(2), None);
        assert_eq!(Compression::default(), Compression::Snappy);
    }

    #[test]
    fn both_algorithms_round_trip_random_and_repetitive_data() {
        for compression in [Compression::Zlib, Compression::Snappy] {
            for len in ROUND_TRIP_SIZES {
                for repetitive in [false, true] {
                    let input = sample(len, repetitive, 7);
                    let compressed = compress(compression, &input);
                    let output = decompress(compression, &compressed, LIMIT).unwrap();
                    assert!(
                        output == input,
                        "{compression:?} len {len} repetitive {repetitive}"
                    );
                }
            }
        }
    }

    #[test]
    fn reused_state_round_trips_like_fresh_state() {
        for compression in [Compression::Zlib, Compression::Snappy] {
            let mut compressor = Compressor::new(compression, 6);
            let mut decompressor = Decompressor::new(compression);
            let (mut compressed, mut output) = (Vec::new(), Vec::new());
            for (seed, len) in [(1, 70_000), (2, 300), (3, 0), (4, 140_000)] {
                let input = sample(len, seed % 2 == 0, seed);
                compressed.clear();
                compressor.compress(&input, &mut compressed, LIMIT).unwrap();
                assert_eq!(compressed, compress(compression, &input));
                decompressor
                    .decompress(&compressed, &mut output, LIMIT)
                    .unwrap();
                assert_eq!(output, input);
            }
        }
    }

    #[test]
    fn input_over_the_limit_is_a_size_error_when_compressing() {
        let mut out = Vec::new();
        for compression in [Compression::Zlib, Compression::Snappy] {
            let result = Compressor::new(compression, 6).compress(&[0; 11], &mut out, 10);
            assert_eq!(result, Err(Error::Size { limit: 10 }));
        }
    }

    #[test]
    fn output_exactly_at_the_limit_decompresses() {
        for compression in [Compression::Zlib, Compression::Snappy] {
            for repetitive in [false, true] {
                let input = sample(1024 * 1024, repetitive, 11);
                let compressed = compress(compression, &input);
                assert_eq!(
                    decompress(compression, &compressed, input.len()).unwrap(),
                    input
                );
                let result = decompress(compression, &compressed, input.len() - 1);
                assert!(matches!(result, Err(Error::Size { .. })), "{compression:?}");
            }
        }
    }

    #[test]
    fn mutated_inputs_never_panic_or_exceed_the_limit() {
        const LIMIT: usize = 64 * 1024;
        let mut rng = Rng(0x5eed);
        for compression in [Compression::Zlib, Compression::Snappy] {
            let mut decompressor = Decompressor::new(compression);
            let mut output = Vec::new();
            let originals: Vec<Vec<u8>> = [(0, false), (300, true), (5000, false), (40_000, true)]
                .iter()
                .map(|&(len, repetitive)| compress(compression, &sample(len, repetitive, 3)))
                .collect();
            for round in 0..4000 {
                let mut input = originals[round % originals.len()].clone();
                for _ in 0..=rng.below(4) {
                    let at = rng.below(input.len() + 1);
                    match rng.below(4) {
                        0 if at < input.len() => input[at] ^= 1 << rng.below(8),
                        1 => input.insert(at, rng.next().to_le_bytes()[0]),
                        2 if at < input.len() => {
                            input.remove(at);
                        }
                        _ => input.truncate(at),
                    }
                }
                if decompressor.decompress(&input, &mut output, LIMIT).is_ok() {
                    assert!(output.len() <= LIMIT);
                }
            }
        }
    }
}
