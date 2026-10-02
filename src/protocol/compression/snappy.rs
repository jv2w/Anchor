//! Snappy block format: a greedy hash-table encoder and a strict decoder of standard elements.
//!
//! The encoder follows the reference matcher (Go `snappy.Encode`): 64 KiB blocks, a hash table of
//! up to 2^14 entries, copies with offsets below 2^16. The decoder accepts every standard element,
//! copy-4 included, and rejects offset 0, offsets before the output, output past the declared
//! length, input after the declared length, and output shorter than the declared length.

use super::{Error, Result};

/// The encoder matches inside blocks of this size, so copy offsets fit in 16 bits.
const MAX_BLOCK: usize = 1 << 16;
const MAX_TABLE: usize = 1 << 14;
/// Bytes kept after the last match start so the 8-byte loads stay inside the block.
const INPUT_MARGIN: usize = 16 - 1;
/// Smaller blocks are emitted as one literal.
const MIN_NON_LITERAL_BLOCK: usize = 1 + 1 + INPUT_MARGIN;
/// A valid stream grows at most 64 bytes per 3-byte copy, under 22 times its size.
const MAX_RATIO: usize = 22;

/// Reusable encoder state: the match table.
pub(crate) struct Encoder {
    table: Box<[u16]>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self {
            table: vec![0; MAX_TABLE].into_boxed_slice(),
        }
    }

    /// Appends the Snappy encoding of `input` to `out`. The caller keeps `input` within `u32`.
    pub(crate) fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        out.reserve(
            input
                .len()
                .saturating_add(input.len() / 6)
                .saturating_add(32),
        );
        let mut len = input.len();
        while len >= 0x80 {
            out.push(len.to_le_bytes()[0] | 0x80);
            len >>= 7;
        }
        out.push(len.to_le_bytes()[0]);
        for block in input.chunks(MAX_BLOCK) {
            if block.len() < MIN_NON_LITERAL_BLOCK {
                emit_literal(out, block);
            } else {
                encode_block(&mut self.table, block, out);
            }
        }
    }
}

const fn hash(bytes: u32, shift: u32) -> usize {
    (bytes.wrapping_mul(0x1e35_a7bd) >> shift) as usize
}

#[expect(
    clippy::indexing_slicing,
    reason = "invariant: callers load inside the block"
)]
fn load32(src: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([src[at], src[at + 1], src[at + 2], src[at + 3]])
}

#[expect(
    clippy::indexing_slicing,
    reason = "invariant: callers load inside the block"
)]
fn load64(src: &[u8], at: usize) -> u64 {
    let b = &src[at..at + 8];
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Encodes one block of `MIN_NON_LITERAL_BLOCK..=MAX_BLOCK` bytes (Go `encodeBlock`).
#[expect(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "invariant: positions stay below s_limit < 2^16 and hashes below the table size; \
              the u64-to-u32 casts take the low bytes on purpose"
)]
fn encode_block(table: &mut [u16], src: &[u8], out: &mut Vec<u8>) {
    let mut shift = 32 - 8;
    let mut table_size = 1 << 8;
    while table_size < MAX_TABLE && table_size < src.len() {
        table_size *= 2;
        shift -= 1;
    }
    let table = &mut table[..table_size];
    table.fill(0);

    let s_limit = src.len() - INPUT_MARGIN;
    let mut next_emit = 0;
    let mut s = 1;
    let mut next_hash = hash(load32(src, s), shift);
    'blocks: loop {
        // Look for a 4-byte match, skipping faster the longer nothing matches.
        let mut skip = 32;
        let mut next_s = s;
        let mut candidate;
        loop {
            s = next_s;
            let step = skip >> 5;
            next_s = s + step;
            skip += step;
            if next_s > s_limit {
                break 'blocks;
            }
            candidate = usize::from(table[next_hash]);
            table[next_hash] = s as u16;
            next_hash = hash(load32(src, next_s), shift);
            if load32(src, s) == load32(src, candidate) {
                break;
            }
        }
        emit_literal(out, &src[next_emit..s]);

        // Emit copies while the next position matches too.
        loop {
            let base = s;
            s += 4;
            let mut i = candidate + 4;
            while s < src.len() && src[i] == src[s] {
                i += 1;
                s += 1;
            }
            emit_copy(out, base - candidate, s - base);
            next_emit = s;
            if s >= s_limit {
                break 'blocks;
            }
            let x = load64(src, s - 1);
            table[hash(x as u32, shift)] = (s - 1) as u16;
            let current = hash((x >> 8) as u32, shift);
            candidate = usize::from(table[current]);
            table[current] = s as u16;
            if (x >> 8) as u32 != load32(src, candidate) {
                next_hash = hash((x >> 16) as u32, shift);
                s += 1;
                break;
            }
        }
    }
    if next_emit < src.len() {
        emit_literal(out, &src[next_emit..]);
    }
}

/// Emits a literal of 1..=65536 bytes with the shortest standard tag.
fn emit_literal(out: &mut Vec<u8>, literal: &[u8]) {
    let n = literal.len() - 1;
    let [lo, hi, ..] = n.to_le_bytes();
    if n < 60 {
        out.push(lo << 2);
    } else if n < 1 << 8 {
        out.extend_from_slice(&[60 << 2, lo]);
    } else {
        out.extend_from_slice(&[61 << 2, lo, hi]);
    }
    out.extend_from_slice(literal);
}

/// Emits a copy with an offset in 1..2^16 and a length of at least 4 (Go `emitCopy`).
fn emit_copy(out: &mut Vec<u8>, offset: usize, mut len: usize) {
    let [o0, o1, ..] = offset.to_le_bytes();
    while len >= 68 {
        out.extend_from_slice(&[(63 << 2) | 2, o0, o1]);
        len -= 64;
    }
    if len > 64 {
        out.extend_from_slice(&[(59 << 2) | 2, o0, o1]);
        len -= 60;
    }
    if len >= 12 || offset >= 2048 {
        let [l, ..] = (len - 1).to_le_bytes();
        out.extend_from_slice(&[(l << 2) | 2, o0, o1]);
    } else {
        let [l, ..] = (len - 4).to_le_bytes();
        out.extend_from_slice(&[(o1 << 5) | (l << 2) | 1, o0]);
    }
}

/// Replaces the contents of `out` with the decoded `input`.
///
/// A declared length above `limit` or above 22 times the input size is a size error, checked
/// before anything is allocated.
pub(crate) fn decompress(input: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<()> {
    out.clear();
    let (declared, mut rest) = read_length(input)?;
    let limit = limit.min(input.len().saturating_mul(MAX_RATIO));
    let declared = usize::try_from(declared).map_err(|_| Error::Size { limit })?;
    if declared > limit {
        return Err(Error::Size { limit });
    }
    out.reserve(declared);
    while let Some((&tag, tail)) = rest.split_first() {
        if out.len() == declared {
            return Err(Error::TrailingInput);
        }
        rest = tail;
        let (offset, len) = match tag & 3 {
            0 => {
                let n = usize::from(tag >> 2);
                let len = if n < 60 {
                    n + 1
                } else {
                    let (bytes, tail) = rest.split_at_checked(n - 59).ok_or(Error::Truncated)?;
                    rest = tail;
                    let value = bytes
                        .iter()
                        .rev()
                        .fold(0_u64, |value, &b| (value << 8) | u64::from(b));
                    usize::try_from(value + 1).map_err(|_| Error::Overrun)?
                };
                let (literal, tail) = rest.split_at_checked(len).ok_or(Error::Truncated)?;
                if len > declared - out.len() {
                    return Err(Error::Overrun);
                }
                out.extend_from_slice(literal);
                rest = tail;
                continue;
            }
            1 => {
                let (&low, tail) = rest.split_first().ok_or(Error::Truncated)?;
                rest = tail;
                let offset = (usize::from(tag & 0xe0) << 3) | usize::from(low);
                (offset, 4 + usize::from((tag >> 2) & 7))
            }
            2 => {
                let (bytes, tail) = rest.split_first_chunk().ok_or(Error::Truncated)?;
                rest = tail;
                (
                    usize::from(u16::from_le_bytes(*bytes)),
                    1 + usize::from(tag >> 2),
                )
            }
            _ => {
                let (bytes, tail) = rest.split_first_chunk().ok_or(Error::Truncated)?;
                rest = tail;
                let offset = usize::try_from(u32::from_le_bytes(*bytes))
                    .map_err(|_| Error::OffsetOutOfRange)?;
                (offset, 1 + usize::from(tag >> 2))
            }
        };
        copy(out, offset, len, declared)?;
    }
    if out.len() == declared {
        Ok(())
    } else {
        Err(Error::Underrun)
    }
}

/// Reads the declared length: a varint of at most 5 bytes whose value fits in `u32`.
fn read_length(mut rest: &[u8]) -> Result<(u32, &[u8])> {
    let mut value = 0_u64;
    for shift in [0, 7, 14, 21, 28] {
        let (&b, tail) = rest.split_first().ok_or(Error::Truncated)?;
        rest = tail;
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return u32::try_from(value)
                .map(|value| (value, rest))
                .map_err(|_| Error::Varint);
        }
    }
    Err(Error::Varint)
}

/// Appends `len` bytes starting `offset` bytes back; overlapping copies repeat the pattern.
fn copy(out: &mut Vec<u8>, offset: usize, len: usize, declared: usize) -> Result<()> {
    if offset == 0 {
        return Err(Error::ZeroOffset);
    }
    let start = out
        .len()
        .checked_sub(offset)
        .ok_or(Error::OffsetOutOfRange)?;
    if len > declared - out.len() {
        return Err(Error::Overrun);
    }
    if offset == 1
        && let Some(&byte) = out.last()
    {
        out.resize(out.len() + len, byte);
        return Ok(());
    }
    let mut remaining = len;
    while remaining > 0 {
        // Each pass copies whole periods from `start`, so the available pattern doubles.
        let n = remaining.min(out.len() - start);
        out.extend_from_within(start..start + n);
        remaining -= n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Encoder, Error, MAX_RATIO, decompress};

    const LIMIT: usize = 16 * 1024 * 1024;

    fn decode(input: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        decompress(input, &mut out, LIMIT)?;
        Ok(out)
    }

    #[test]
    fn decodes_every_standard_element() {
        // "abcd" literal, then copy-1, copy-2 and copy-4 of it, with overlapping lengths.
        let input = [
            20,
            3 << 2,
            b'a',
            b'b',
            b'c',
            b'd', // varint 20, literal "abcd"
            0x01,
            4, // copy-1: length 4, offset 4
            (5 << 2) | 2,
            4,
            0, // copy-2: length 6, offset 4
            (5 << 2) | 3,
            2,
            0,
            0,
            0, // copy-4: length 6, offset 2
        ];
        assert_eq!(decode(&input).unwrap(), b"abcdabcdabcdabababab");
    }

    #[test]
    fn decodes_long_literal_tags() {
        for (tag, extra) in [
            (60_u8, &[0_u8][..]),
            (61, &[0, 0]),
            (62, &[0, 0, 0]),
            (63, &[0, 0, 0, 0]),
        ] {
            let mut input = vec![1, tag << 2];
            input.extend_from_slice(extra);
            input.push(b'x');
            assert_eq!(decode(&input).unwrap(), b"x", "tag {tag}");
        }
    }

    #[test]
    fn encoder_uses_minimal_standard_elements() {
        let mut out = Vec::new();
        Encoder::new().compress(&[7; 100], &mut out);
        // varint 100, a 1-byte literal, then copy-2 of 64 and copy-2 of 35 (too long for copy-1).
        assert_eq!(out, [100, 0, 7, (63 << 2) | 2, 1, 0, (34 << 2) | 2, 1, 0]);
        out.clear();
        Encoder::new().compress(b"short", &mut out);
        assert_eq!(out, [5, 4 << 2, b's', b'h', b'o', b'r', b't']);
    }

    #[test]
    fn rejects_zero_offset() {
        assert_eq!(decode(&[5, 0, b'a', 0x01, 0]), Err(Error::ZeroOffset));
        assert_eq!(
            decode(&[5, 0, b'a', (3 << 2) | 2, 0, 0]),
            Err(Error::ZeroOffset)
        );
    }

    #[test]
    fn rejects_offset_beyond_written_output() {
        assert_eq!(decode(&[5, 0, b'a', 0x01, 2]), Err(Error::OffsetOutOfRange));
        assert_eq!(
            decode(&[5, 0, b'a', 3, 9, 0, 0, 1]),
            Err(Error::OffsetOutOfRange)
        );
    }

    #[test]
    fn rejects_output_past_declared_length() {
        assert_eq!(decode(&[2, 2 << 2, b'a', b'b', b'c']), Err(Error::Overrun));
        assert_eq!(decode(&[4, 0, b'a', 0x01, 1]), Err(Error::Overrun));
    }

    #[test]
    fn rejects_trailing_input() {
        assert_eq!(decode(&[1, 0, b'a', 0, b'b']), Err(Error::TrailingInput));
        assert_eq!(decode(&[0, 0]), Err(Error::TrailingInput));
    }

    #[test]
    fn rejects_output_shorter_than_declared_length() {
        assert_eq!(decode(&[3, 0, b'a']), Err(Error::Underrun));
        assert_eq!(decode(&[1]), Err(Error::Underrun));
    }

    #[test]
    fn rejects_truncated_elements_and_invalid_length_prefixes() {
        assert_eq!(decode(&[]), Err(Error::Truncated));
        assert_eq!(decode(&[0x80]), Err(Error::Truncated));
        assert_eq!(decode(&[3, 2 << 2, b'a']), Err(Error::Truncated));
        assert_eq!(decode(&[8, 0, b'a', 2, 1]), Err(Error::Truncated));
        assert_eq!(
            decode(&[0x80, 0x80, 0x80, 0x80, 0x80, 0]),
            Err(Error::Varint)
        );
        assert_eq!(decode(&[0xff, 0xff, 0xff, 0xff, 0x10]), Err(Error::Varint));
    }

    #[test]
    fn declared_length_over_the_limit_is_a_size_error_before_allocating() {
        let mut out = Vec::new();
        // u32::MAX declared in a 5-byte prefix.
        let result = decompress(&[0xff, 0xff, 0xff, 0xff, 0x0f], &mut out, LIMIT);
        assert!(matches!(result, Err(Error::Size { .. })));
        assert_eq!(out.capacity(), 0);

        // Within the ratio but above the limit: 1 MiB + 1 declared, padded input.
        let mut input = vec![0x81, 0x80, 0x40];
        input.resize(64 * 1024, 0);
        let result = decompress(&input, &mut out, 1024 * 1024);
        assert_eq!(result, Err(Error::Size { limit: 1024 * 1024 }));
        assert_eq!(out.capacity(), 0);
    }

    #[test]
    fn declared_length_over_the_ratio_is_a_size_error() {
        let input = [0x80, 0x80, 0x01, 0, b'a'];
        let mut out = Vec::new();
        assert_eq!(
            decompress(&input, &mut out, LIMIT),
            Err(Error::Size {
                limit: input.len() * MAX_RATIO
            })
        );
        assert_eq!(out.capacity(), 0);
    }

    #[test]
    fn densest_valid_stream_is_within_the_ratio() {
        // One literal, then the longest copy-2 repeated: 64 bytes per 3 input bytes.
        let copies = 10_000;
        let len = 1 + 64 * copies;
        let mut input = Vec::new();
        Encoder::new().compress(&vec![0; len], &mut input);
        assert!(input.len() * MAX_RATIO >= len);
        assert_eq!(decode(&input).unwrap(), vec![0; len]);
    }
}
