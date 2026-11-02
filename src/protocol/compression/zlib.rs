//! Raw DEFLATE: `libdeflate` compresses, `miniz_oxide` inflates into a buffer bounded by the limit.

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};

use super::{Error, Result};

/// The first inflate buffer is this many times the input, then it doubles up to the limit.
const FIRST_RATIO: usize = 4;
/// The least size of the first inflate buffer, in bytes.
const MIN_FIRST_BUFFER: usize = 4096;

/// Reusable compressor state at one level.
pub(crate) struct Encoder(libdeflater::Compressor);

impl Encoder {
    /// An encoder at `level`, which must be in 1..=9.
    pub(crate) fn new(level: u8) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "invariant: every caller passes 1..=9: the level batch settings validated, \
                      or a constant level"
        )]
        let level =
            libdeflater::CompressionLvl::new(i32::from(level)).expect("zlib level is in 1..=9");
        Self(libdeflater::Compressor::new(level))
    }

    /// The most bytes [`Encoder::compress`] appends for `input_len` bytes of input.
    pub(crate) fn max_compressed_len(&mut self, input_len: usize) -> usize {
        self.0.deflate_compress_bound(input_len)
    }

    /// Appends the raw DEFLATE encoding of `input` to `out`.
    pub(crate) fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let start = out.len();
        let bound = self.max_compressed_len(input.len());
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
    /// A decoder with fresh inflate state.
    pub(crate) fn new() -> Self {
        Self(Box::default())
    }

    /// Replaces the contents of `out` with the inflated `input`.
    ///
    /// The buffer grows only up to `limit + 1` bytes; a stream that needs more is a size error.
    /// Input left after the final block is ignored: only official clients show whether their
    /// encoder leaves any in a batch, so Anchor accepts every case.
    pub(crate) fn decompress(
        &mut self,
        input: &[u8],
        limit: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.inflate(input, limit, 0, out).map(|_| ())
    }

    /// Replaces the contents of `out` with the inflated `input`, which must be one whole stream
    /// with nothing after it.
    ///
    /// The buffer grows as in [`Self::decompress`]. With [`Framing::Zlib`] the stream's RFC 1950
    /// header and Adler-32 checksum are checked too.
    pub(crate) fn decompress_whole(
        &mut self,
        input: &[u8],
        limit: usize,
        framing: Framing,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let flags = match framing {
            Framing::Zlib => inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER,
            Framing::Raw => 0,
        };
        if self.inflate(input, limit, flags, out)? == 0 {
            Ok(())
        } else {
            Err(Error::TrailingInput)
        }
    }

    /// Inflates `input` into `out` with the extra `flags`; returns how many input bytes are left
    /// after the stream.
    fn inflate(
        &mut self,
        input: &[u8],
        limit: usize,
        flags: u32,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
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
        let flags = flags | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let mut rest = input;
        let mut written = 0;
        loop {
            let (status, read, wrote) = decompress(&mut self.0, rest, out, written, flags);
            rest = rest.get(read..).ok_or(Error::Deflate)?;
            written = written.saturating_add(wrote);
            match status {
                TINFLStatus::Done => {
                    if written > limit {
                        return Err(Error::Size { limit });
                    }
                    out.truncate(written);
                    return Ok(rest.len());
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

/// How a DEFLATE stream is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// An RFC 1950 zlib header and Adler-32 trailer around the stream.
    Zlib,
    /// The bare stream.
    Raw,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::compression::tests::{Content, sample};

    const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

    #[test]
    fn every_level_round_trips() {
        let input = sample(100_000, Content::Repetitive, 5);
        let mut output = Vec::new();
        for level in 1..=9 {
            let mut compressed = Vec::new();
            Encoder::new(level).compress(&input, &mut compressed);
            Decoder::new()
                .decompress(&compressed, MAX_OUTPUT_BYTES, &mut output)
                .unwrap();
            assert_eq!(output, input, "level {level}");
        }
    }

    fn deflate(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Encoder::new(6).compress(input, &mut out);
        out
    }

    #[test]
    fn small_input_inflating_past_the_limit_is_a_size_error() {
        let bomb = deflate(&vec![0; 64 * MAX_OUTPUT_BYTES]);
        assert!(bomb.len() < 128 * 1024, "bomb is {} bytes", bomb.len());
        let mut out = Vec::new();
        let result = Decoder::new().decompress(&bomb, MAX_OUTPUT_BYTES, &mut out);
        assert_eq!(
            result,
            Err(Error::Size {
                limit: MAX_OUTPUT_BYTES
            })
        );
        assert!(
            out.capacity() <= 2 * (MAX_OUTPUT_BYTES + 1),
            "capacity {}",
            out.capacity()
        );
    }

    #[test]
    fn ignores_input_after_the_final_block() {
        let mut input = deflate(b"hello hello hello");
        input.extend_from_slice(&[0, 0, 0xff, 0xff]);
        let mut out = Vec::new();
        Decoder::new()
            .decompress(&input, MAX_OUTPUT_BYTES, &mut out)
            .unwrap();
        assert_eq!(out, b"hello hello hello");
    }

    #[test]
    fn rejects_truncated_and_invalid_streams() {
        let input = deflate(&sample(10_000, Content::Random, 9));
        let mut out = Vec::new();
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.decompress(&input[..input.len() / 2], MAX_OUTPUT_BYTES, &mut out),
            Err(Error::Truncated)
        );
        assert_eq!(
            decoder.decompress(&[], MAX_OUTPUT_BYTES, &mut out),
            Err(Error::Truncated)
        );
        // Block type 3 is reserved.
        assert_eq!(
            decoder.decompress(&[0x07], MAX_OUTPUT_BYTES, &mut out),
            Err(Error::Deflate)
        );
    }

    fn zlib(input: &[u8]) -> Vec<u8> {
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::default());
        let mut out = vec![0; c.zlib_compress_bound(input.len())];
        let n = c.zlib_compress(input, &mut out).unwrap();
        out.truncate(n);
        out
    }

    #[test]
    fn whole_streams_decode_with_and_without_the_zlib_frame() {
        let input = sample(50_000, Content::Repetitive, 4);
        let mut out = Vec::new();
        let mut decoder = Decoder::new();
        decoder
            .decompress_whole(&zlib(&input), MAX_OUTPUT_BYTES, Framing::Zlib, &mut out)
            .unwrap();
        assert_eq!(out, input);
        decoder
            .decompress_whole(&deflate(&input), MAX_OUTPUT_BYTES, Framing::Raw, &mut out)
            .unwrap();
        assert_eq!(out, input);
        // Each framing refuses the other.
        assert!(
            decoder
                .decompress_whole(&deflate(&input), MAX_OUTPUT_BYTES, Framing::Zlib, &mut out)
                .is_err()
        );
        assert!(
            decoder
                .decompress_whole(&zlib(&input), MAX_OUTPUT_BYTES, Framing::Raw, &mut out)
                .is_err()
        );
    }

    #[test]
    fn whole_streams_refuse_trailing_input_and_bad_checksums() {
        let mut out = Vec::new();
        let mut decoder = Decoder::new();
        for (framing, mut stream) in [
            (Framing::Zlib, zlib(b"hello")),
            (Framing::Raw, deflate(b"hello")),
        ] {
            stream.push(0);
            assert_eq!(
                decoder.decompress_whole(&stream, MAX_OUTPUT_BYTES, framing, &mut out),
                Err(Error::TrailingInput)
            );
        }
        let mut stream = zlib(b"hello hello");
        let last = stream.len() - 1;
        stream[last] ^= 1;
        assert_eq!(
            decoder.decompress_whole(&stream, MAX_OUTPUT_BYTES, Framing::Zlib, &mut out),
            Err(Error::Deflate)
        );
        let bomb = zlib(&vec![0; 4 * MAX_OUTPUT_BYTES]);
        assert_eq!(
            decoder.decompress_whole(&bomb, MAX_OUTPUT_BYTES, Framing::Zlib, &mut out),
            Err(Error::Size {
                limit: MAX_OUTPUT_BYTES
            })
        );
    }
}
