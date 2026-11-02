//! Batch compression: zlib (raw DEFLATE) and Snappy (block format).

pub(crate) mod snappy;
pub(crate) mod zlib;

use std::fmt;

use super::consts::CompressionAlgorithm;

/// The `NetworkSettings` id of raw DEFLATE.
const ALGORITHM_FLATE: u16 = CompressionAlgorithm::Flate as u16;
/// The `NetworkSettings` id of Snappy.
const ALGORITHM_SNAPPY: u16 = CompressionAlgorithm::Snappy as u16;

/// The algorithm that compresses batch bodies, announced to the client in `NetworkSettings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Compression {
    /// Raw DEFLATE without a zlib header. Network id 0. Smaller batches, more CPU.
    ///
    /// Named after the `zlib` value of the `compression` setting, not after the stream format.
    Zlib,
    /// Snappy block format. Network id 1. Less CPU, larger batches.
    ///
    /// gophertunnel notes an unconfirmed report that clients without AVX2 crash with Snappy;
    /// Anchor cannot observe official client crashes, so this is unverified.
    #[default]
    Snappy,
}

impl Compression {
    /// The algorithm for a `NetworkSettings` id, or `None` for an unknown id.
    pub const fn from_network_id(id: u16) -> Option<Self> {
        match id {
            ALGORITHM_FLATE => Some(Self::Zlib),
            ALGORITHM_SNAPPY => Some(Self::Snappy),
            _ => None,
        }
    }

    /// The id that `NetworkSettings` carries for this algorithm.
    pub const fn network_id(self) -> u16 {
        match self {
            Self::Zlib => ALGORITHM_FLATE,
            Self::Snappy => ALGORITHM_SNAPPY,
        }
    }

    /// The first byte of a batch body compressed with this algorithm: the low byte of its
    /// network id (gophertunnel `packet.Encoder`).
    pub(crate) const fn header(self) -> u8 {
        self.network_id().to_le_bytes()[0]
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
    /// The DEFLATE stream, or its zlib header or checksum, is invalid.
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

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Reusable compression state for one algorithm.
pub(crate) enum Compressor {
    Zlib(zlib::Encoder),
    Snappy(snappy::Encoder),
}

impl Compressor {
    /// A compressor for `compression`; `zlib_level` must be in 1..=9 and is ignored for Snappy.
    pub(crate) fn new(compression: Compression, zlib_level: u8) -> Self {
        match compression {
            Compression::Zlib => Self::Zlib(zlib::Encoder::new(zlib_level)),
            Compression::Snappy => Self::Snappy(snappy::Encoder::new()),
        }
    }

    /// The most bytes [`Compressor::compress`] appends for `input_len` bytes of input.
    pub(crate) fn max_compressed_len(&mut self, input_len: usize) -> usize {
        match self {
            Self::Zlib(encoder) => encoder.max_compressed_len(input_len),
            Self::Snappy(_) => snappy::max_compressed_len(input_len),
        }
    }

    /// Appends the compressed `input` to `out`.
    ///
    /// The caller keeps `input` within `u32::MAX` bytes, the most a Snappy length prefix declares.
    pub(crate) fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        match self {
            Self::Zlib(encoder) => encoder.compress(input, out),
            Self::Snappy(encoder) => encoder.compress(input, out),
        }
    }
}

/// Reusable decompression state for one algorithm.
pub(crate) enum Decompressor {
    Zlib(zlib::Decoder),
    Snappy,
}

impl Decompressor {
    /// A decompressor for `compression`.
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
        limit: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        match self {
            Self::Zlib(decoder) => decoder.decompress(input, limit, out),
            Self::Snappy => snappy::decompress(input, limit, out),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::protocol::Rng;

    /// The decompression limit of the mutation test.
    const MAX_MUTATION_BYTES: usize = 64 * 1024;
    /// The decompression limit of the round-trip tests.
    pub(crate) const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
    /// The input sizes of the round-trip tests.
    pub(crate) const ROUND_TRIP_SIZES: [usize; 6] = [0, 1, 255, 256, 64 * 1024, 1024 * 1024];

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
    fn batch_headers_are_0_for_zlib_and_1_for_snappy() {
        assert_eq!(Compression::Zlib.header(), 0x00);
        assert_eq!(Compression::Snappy.header(), 0x01);
    }

    /// The kind of bytes [`sample`] returns.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Content {
        /// Random bytes, which do not compress.
        Random,
        /// Runs and repeated phrases, which compress well.
        Repetitive,
    }

    /// `len` bytes of `content` drawn from `seed`.
    pub(crate) fn sample(len: usize, content: Content, seed: u64) -> Vec<u8> {
        let mut rng = Rng(seed);
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            let word = rng.next().to_le_bytes();
            match content {
                Content::Random => data.extend_from_slice(&word),
                Content::Repetitive => match word[0] % 3 {
                    0 => data.extend(std::iter::repeat_n(word[1] % 4, usize::from(word[2]))),
                    1 => data.extend_from_slice(b"minecraft:stone"),
                    _ => data.extend_from_slice(&word[..usize::from(word[3] % 8)]),
                },
            }
        }
        data.truncate(len);
        data
    }

    fn compress(compression: Compression, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Compressor::new(compression, 6).compress(input, &mut out);
        out
    }

    fn decompress(compression: Compression, input: &[u8], limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        Decompressor::new(compression).decompress(input, limit, &mut out)?;
        Ok(out)
    }

    #[test]
    fn both_algorithms_round_trip_random_and_repetitive_data() {
        for compression in [Compression::Zlib, Compression::Snappy] {
            for len in ROUND_TRIP_SIZES {
                for content in [Content::Random, Content::Repetitive] {
                    let input = sample(len, content, 7);
                    let compressed = compress(compression, &input);
                    let output = decompress(compression, &compressed, MAX_OUTPUT_BYTES).unwrap();
                    assert_eq!(output, input, "{compression:?} len {len} {content:?}");
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
            for (seed, len, content) in [
                (1, 70_000, Content::Random),
                (2, 300, Content::Repetitive),
                (3, 0, Content::Random),
                (4, 140_000, Content::Repetitive),
            ] {
                let input = sample(len, content, seed);
                compressed.clear();
                compressor.compress(&input, &mut compressed);
                assert_eq!(compressed, compress(compression, &input));
                decompressor
                    .decompress(&compressed, MAX_OUTPUT_BYTES, &mut output)
                    .unwrap();
                assert_eq!(output, input);
            }
        }
    }

    #[test]
    fn output_exactly_at_the_limit_decompresses() {
        for compression in [Compression::Zlib, Compression::Snappy] {
            for content in [Content::Random, Content::Repetitive] {
                let input = sample(1024 * 1024, content, 11);
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
        let mut rng = Rng(0x5eed);
        for compression in [Compression::Zlib, Compression::Snappy] {
            let mut decompressor = Decompressor::new(compression);
            let mut output = Vec::new();
            let originals: Vec<Vec<u8>> = [
                (0, Content::Random),
                (300, Content::Repetitive),
                (5000, Content::Random),
                (40_000, Content::Repetitive),
            ]
            .iter()
            .map(|&(len, content)| compress(compression, &sample(len, content, 3)))
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
                if decompressor
                    .decompress(&input, MAX_MUTATION_BYTES, &mut output)
                    .is_ok()
                {
                    assert!(output.len() <= MAX_MUTATION_BYTES);
                }
            }
        }
    }
}
