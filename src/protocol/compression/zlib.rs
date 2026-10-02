//! Raw DEFLATE: `libdeflate` compresses, `miniz_oxide` inflates into a buffer bounded by the limit.

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};

use super::{Error, Result};

/// The first inflate buffer is this many times the input, then it doubles up to the limit.
const FIRST_RATIO: usize = 4;
const MIN_FIRST_BUFFER: usize = 4096;

/// Reusable compressor state at one level.
pub(crate) struct Encoder(libdeflater::Compressor);

impl Encoder {
    /// `level` must be in 1..=9.
    pub(crate) fn new(level: u8) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "invariant: batch settings validate 1..=9"
        )]
        let level = libdeflater::CompressionLvl::new(i32::from(level)).expect("valid zlib level");
        Self(libdeflater::Compressor::new(level))
    }

    /// Appends the raw DEFLATE encoding of `input` to `out`.
    pub(crate) fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let start = out.len();
        let bound = self.0.deflate_compress_bound(input.len());
        out.resize(start.saturating_add(bound), 0);
        #[expect(
            clippy::indexing_slicing,
            clippy::expect_used,
            reason = "invariant: `out` was just grown by libdeflate's worst-case bound"
        )]
        let written = self
            .0
            .deflate_compress(input, &mut out[start..])
            .expect("output fits the bound");
        out.truncate(start + written);
    }
}

/// Reusable inflate state.
pub(crate) struct Decoder(Box<DecompressorOxide>);

impl Decoder {
    pub(crate) fn new() -> Self {
        Self(Box::default())
    }

    /// Replaces the contents of `out` with the inflated `input`.
    ///
    /// The buffer grows only up to `limit + 1` bytes; a stream that needs more is a size error.
    /// Input left after the final block is an error.
    pub(crate) fn decompress(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<()> {
        // One byte over the limit tells "ends exactly at the limit" from "needs more".
        let cap = limit.saturating_add(1);
        let first = input
            .len()
            .saturating_mul(FIRST_RATIO)
            .max(MIN_FIRST_BUFFER)
            .min(cap);
        out.clear();
        out.resize(first, 0);
        self.0.init();
        let mut rest = input;
        let mut written = 0;
        loop {
            let flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
            let (status, read, wrote) = decompress(&mut self.0, rest, out, written, flags);
            rest = rest.get(read..).ok_or(Error::Deflate)?;
            written = written.saturating_add(wrote);
            match status {
                TINFLStatus::Done => {
                    if written > limit {
                        return Err(Error::Size { limit });
                    }
                    if !rest.is_empty() {
                        return Err(Error::TrailingInput);
                    }
                    out.truncate(written);
                    return Ok(());
                }
                TINFLStatus::HasMoreOutput => {
                    if out.len() >= cap {
                        return Err(Error::Size { limit });
                    }
                    out.resize(out.len().saturating_mul(2).min(cap), 0);
                }
                TINFLStatus::FailedCannotMakeProgress | TINFLStatus::NeedsMoreInput => {
                    return Err(Error::Truncated);
                }
                _ => return Err(Error::Deflate),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Encoder, Error};

    const LIMIT: usize = 1024 * 1024;

    fn deflate(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Encoder::new(6).compress(input, &mut out);
        out
    }

    #[test]
    fn every_level_round_trips() {
        let input = super::super::tests::sample(100_000, true, 5);
        let mut output = Vec::new();
        for level in 1..=9 {
            let mut compressed = Vec::new();
            Encoder::new(level).compress(&input, &mut compressed);
            Decoder::new()
                .decompress(&compressed, &mut output, LIMIT)
                .unwrap();
            assert_eq!(output, input, "level {level}");
        }
    }

    #[test]
    fn small_input_inflating_past_the_limit_is_a_size_error() {
        let bomb = deflate(&vec![0; 64 * LIMIT]);
        assert!(bomb.len() < 128 * 1024, "bomb is {} bytes", bomb.len());
        let mut out = Vec::new();
        let result = Decoder::new().decompress(&bomb, &mut out, LIMIT);
        assert_eq!(result, Err(Error::Size { limit: LIMIT }));
        assert!(
            out.capacity() <= 2 * (LIMIT + 1),
            "capacity {}",
            out.capacity()
        );
    }

    #[test]
    fn rejects_trailing_input() {
        let mut input = deflate(b"hello hello hello");
        input.push(0);
        let mut out = Vec::new();
        assert_eq!(
            Decoder::new().decompress(&input, &mut out, LIMIT),
            Err(Error::TrailingInput)
        );
    }

    #[test]
    fn rejects_truncated_and_invalid_streams() {
        let input = deflate(&super::super::tests::sample(10_000, false, 9));
        let mut out = Vec::new();
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.decompress(&input[..input.len() / 2], &mut out, LIMIT),
            Err(Error::Truncated)
        );
        assert_eq!(
            decoder.decompress(&[], &mut out, LIMIT),
            Err(Error::Truncated)
        );
        // Block type 3 is reserved.
        assert_eq!(
            decoder.decompress(&[0x07], &mut out, LIMIT),
            Err(Error::Deflate)
        );
    }
}
