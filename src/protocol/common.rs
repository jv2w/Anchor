//! Fixed-shape wire values: vectors, positions, rotations, UUIDs, colors and bitsets.

use std::fmt;
use std::mem::size_of;
use std::str::FromStr;

use super::io::{Codec, Error, Reader, Result, Writer};

/// The error for a bit index past a bitset's end.
const BITSET_INDEX_PAST_END: Error = Error::InvalidValue {
    field: "bitset",
    reason: "bit index past its size",
};
/// The error for a bitset with more bits than its size.
const BITSET_TOO_MANY_BITS: Error = Error::InvalidValue {
    field: "bitset",
    reason: "more bits than its size",
};
/// One `ByteFloat` step in degrees.
const BYTE_FLOAT_STEP: f32 = 360.0 / 256.0;
/// The exclusive bound of the floats that convert to an int32 in Go: 2^31.
const FLOAT_TO_INT32_LIMIT: f32 = 2_147_483_648.0;
/// The exclusive bound of the magnitudes that convert to an int64 in Go: 2^63.
const FLOAT_TO_INT64_LIMIT: f32 = 9_223_372_036_854_775_808.0;
/// The largest value of an 8-bit color channel, as a float.
const RGB_CHANNEL_MAX: f32 = 255.0;
/// The `SoundPos` units in one block.
const SOUND_POS_UNITS_PER_BLOCK: f32 = 8.0;

/// A block position: x, y, z as varint32s on the wire (gophertunnel `BlockPos`).
pub type BlockPos = [i32; 3];
/// A chunk position: x, z as varint32s on the wire (gophertunnel `ChunkPos`).
pub type ChunkPos = [i32; 2];
/// A sub-chunk position: x, y, z as little-endian int32s on the wire (gophertunnel `SubChunkPos`).
pub type SubChunkPos = [i32; 3];

/// A UUID in RFC 4122 byte order.
///
/// The wire holds each 8-byte half reversed (gophertunnel `UUID`). The text form is lowercase and
/// hyphenated; reading it accepts the forms Go `uuid.Parse` accepts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Uuid([u8; 16]);

impl Uuid {
    /// The length of the braced text form.
    const BRACED_LEN: usize = Self::HYPHENATED_LEN + 2;
    /// The length of the plain text form: 32 hex digits.
    const DIGITS: usize = 32;
    /// The hex digits between the hyphens of the hyphenated text form.
    const GROUP_DIGITS: [usize; 5] = [8, 4, 4, 4, 12];
    /// The length of the hyphenated text form.
    const HYPHENATED_LEN: usize = 36;
    /// The error for text that is no UUID form.
    const INVALID_TEXT: Error = Error::InvalidValue {
        field: "uuid",
        reason: "not a form Go uuid.Parse accepts",
    };
    /// The length of the URN text form.
    const URN_LEN: usize = Self::HYPHENATED_LEN + Self::URN_PREFIX.len();
    /// The prefix of the URN text form.
    const URN_PREFIX: &str = "urn:uuid:";

    /// The UUID with these bytes in RFC 4122 order.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The bytes in RFC 4122 order.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut bytes = self.0.iter();
        for (i, digits) in Self::GROUP_DIGITS.into_iter().enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            for byte in bytes.by_ref().take(digits / 2) {
                write!(f, "{byte:02x}")?;
            }
        }
        Ok(())
    }
}

impl Codec for Uuid {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.uuid()
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.uuid(*self);
        Ok(())
    }
}

impl FromStr for Uuid {
    type Err = Error;

    /// Reads the text forms Go `uuid.Parse` accepts.
    ///
    /// These are 36 characters with hyphens, the same in braces or after `urn:uuid:`, or 32 hex
    /// digits.
    fn from_str(text: &str) -> Result<Self> {
        let unwrapped = match text.len() {
            Self::BRACED_LEN => text
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}')),
            Self::URN_LEN => {
                text.split_at_checked(Self::URN_PREFIX.len())
                    .and_then(|(prefix, rest)| {
                        prefix
                            .eq_ignore_ascii_case(Self::URN_PREFIX)
                            .then_some(rest)
                    })
            }
            _ => Some(text),
        }
        .ok_or(Self::INVALID_TEXT)?;
        let hyphenated = unwrapped.split('-').map(str::len).eq(Self::GROUP_DIGITS);
        if !hyphenated && unwrapped.len() != Self::DIGITS {
            return Err(Self::INVALID_TEXT);
        }
        let mut digits = unwrapped.bytes().filter(|&c| c != b'-').map(hex_digit);
        let mut bytes = [0; 16];
        for byte in &mut bytes {
            let high = digits.next().flatten().ok_or(Self::INVALID_TEXT)?;
            let low = digits.next().flatten().ok_or(Self::INVALID_TEXT)?;
            *byte = high << 4 | low;
        }
        Ok(Self(bytes))
    }
}

/// An 8-bit RGBA color (Go `color.RGBA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgba {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha.
    pub a: u8,
}

/// A fixed-size set of bit flags (gophertunnel `Bitset`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bitset {
    len: usize,
    words: Vec<u64>,
}

impl Bitset {
    /// A bitset of `len` cleared bits.
    pub fn new(len: usize) -> Self {
        Self {
            len,
            words: vec![0; len.div_ceil(64)],
        }
    }

    /// Whether bit `index` is set; `false` past the end.
    pub fn get(&self, index: usize) -> bool {
        self.words
            .get(index / 64)
            .is_some_and(|w| (w >> (index % 64)) & 1 == 1)
    }

    /// Whether the bitset has no bits.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The number of bits.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Sets bit `index`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidValue`] for an `index` past the end, changing nothing.
    pub fn set(&mut self, index: usize) -> Result<()> {
        self.update(index, |w, mask| w | mask)
    }

    /// Clears bit `index`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidValue`] for an `index` past the end, changing nothing.
    pub fn unset(&mut self, index: usize) -> Result<()> {
        self.update(index, |w, mask| w & !mask)
    }

    /// The number of significant bits: one past the highest set bit.
    fn bit_len(&self) -> usize {
        self.words
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &w)| w != 0)
            .map_or(0, |(i, w)| i * 64 + 64 - w.leading_zeros() as usize)
    }

    /// Bits `start..start + 7` as the low bits of a byte.
    fn seven_bits(&self, start: usize) -> u8 {
        (0..7).fold(0, |acc, i| acc | (u8::from(self.get(start + i)) << i))
    }

    /// Applies `f` to the word of bit `index`; an error for an `index` past the end.
    ///
    /// `f` takes the word and the mask of the bit.
    fn update(&mut self, index: usize, f: impl FnOnce(u64, u64) -> u64) -> Result<()> {
        let len = self.len;
        let Some(word) = self.words.get_mut(index / 64).filter(|_| index < len) else {
            return Err(BITSET_INDEX_PAST_END);
        };
        *word = f(*word, 1 << (index % 64));
        Ok(())
    }
}

impl Reader<'_> {
    /// Reads a `BEARGB`: a big-endian int32 with alpha in the low byte.
    pub(crate) fn be_argb(&mut self) -> Result<Rgba> {
        let [b, g, r, a] = self.array()?;
        Ok(Rgba { r, g, b, a })
    }

    /// Reads a `Bitset`: a varuint of at most `size` bits.
    pub(crate) fn bitset(&mut self, size: usize) -> Result<Bitset> {
        // One allocation of the words; a bitset takes at least one input byte.
        self.limit(1, 1, size.div_ceil(64).saturating_mul(size_of::<u64>()))?;
        let mut x = Bitset::new(size);
        for start in (0..size).step_by(7) {
            let b = self.uint8()?;
            if start.saturating_add(8 - b.leading_zeros() as usize) > size {
                return Err(BITSET_TOO_MANY_BITS);
            }
            for i in 0..7 {
                if (b >> i) & 1 == 1 {
                    x.set(start + i)?;
                }
            }
            if b & 0x80 == 0 {
                return Ok(x);
            }
        }
        Err(BITSET_TOO_MANY_BITS)
    }

    /// Reads a `ByteFloat`: a rotation in 1/256 turns.
    pub(crate) fn byte_float(&mut self) -> Result<f32> {
        Ok(f32::from(self.uint8()?) * BYTE_FLOAT_STEP)
    }

    /// Reads a `ChunkPos`.
    pub(crate) fn chunk_pos(&mut self) -> Result<ChunkPos> {
        Ok([self.varint32()?, self.varint32()?])
    }

    /// Reads an `RGB`: three float32s in 0..=1; alpha is 0, like gophertunnel.
    pub(crate) fn rgb(&mut self) -> Result<Rgba> {
        let [r, g, b] = self.vec3()?;
        Ok(Rgba {
            r: go_float_to_u8(r * RGB_CHANNEL_MAX),
            g: go_float_to_u8(g * RGB_CHANNEL_MAX),
            b: go_float_to_u8(b * RGB_CHANNEL_MAX),
            a: 0,
        })
    }

    /// Reads an `RGBA`: a little-endian uint32 with red in the low byte.
    pub(crate) fn rgba(&mut self) -> Result<Rgba> {
        let [r, g, b, a] = self.array()?;
        Ok(Rgba { r, g, b, a })
    }

    /// Reads a `SoundPos`: a block position in eighths of a block.
    #[expect(
        clippy::cast_precision_loss,
        reason = "consistency: gophertunnel converts int32 to float32 the same way"
    )]
    pub(crate) fn sound_pos(&mut self) -> Result<[f32; 3]> {
        let [x, y, z] = self.block_pos()?;
        Ok([
            x as f32 / SOUND_POS_UNITS_PER_BLOCK,
            y as f32 / SOUND_POS_UNITS_PER_BLOCK,
            z as f32 / SOUND_POS_UNITS_PER_BLOCK,
        ])
    }

    /// Reads a `BlockPos`.
    pub(crate) fn block_pos(&mut self) -> Result<BlockPos> {
        Ok([self.varint32()?, self.varint32()?, self.varint32()?])
    }

    /// Reads a `SubChunkPos`.
    pub(crate) fn sub_chunk_pos(&mut self) -> Result<SubChunkPos> {
        Ok([self.int32()?, self.int32()?, self.int32()?])
    }

    /// Reads a `UUID`.
    pub(crate) fn uuid(&mut self) -> Result<Uuid> {
        let mut b: [u8; 16] = self.array()?;
        let (high, low) = b.split_at_mut(8);
        high.reverse();
        low.reverse();
        Ok(Uuid(b))
    }

    /// Reads a `Vec2`.
    pub(crate) fn vec2(&mut self) -> Result<[f32; 2]> {
        Ok([self.float32()?, self.float32()?])
    }

    /// Reads a `Vec3`.
    pub(crate) fn vec3(&mut self) -> Result<[f32; 3]> {
        Ok([self.float32()?, self.float32()?, self.float32()?])
    }
}

impl Writer {
    /// Writes a `BEARGB`.
    pub(crate) fn be_argb(&mut self, x: Rgba) {
        self.bytes(&[x.b, x.g, x.r, x.a]);
    }

    /// Writes a `Bitset`, failing if the bitset does not have `size` bits.
    pub(crate) fn bitset(&mut self, x: &Bitset, size: usize) -> Result<()> {
        if x.len != size {
            return Err(Error::InvalidValue {
                field: "bitset",
                reason: "size differs from the field's size",
            });
        }
        let bytes = x.bit_len().div_ceil(7).max(1);
        for i in 0..bytes {
            let more = if i + 1 < bytes { 0x80 } else { 0 };
            self.uint8(x.seven_bits(i * 7) | more);
        }
        Ok(())
    }

    /// Writes a `ByteFloat`.
    pub(crate) fn byte_float(&mut self, x: f32) {
        self.uint8(go_float_to_u8(x / BYTE_FLOAT_STEP));
    }

    /// Writes a `ChunkPos`.
    pub(crate) fn chunk_pos(&mut self, x: ChunkPos) {
        for v in x {
            self.varint32(v);
        }
    }

    /// Writes an `RGB`.
    pub(crate) fn rgb(&mut self, x: Rgba) {
        self.vec3([x.r, x.g, x.b].map(|c| f32::from(c) / RGB_CHANNEL_MAX));
    }

    /// Writes an `RGBA`.
    pub(crate) fn rgba(&mut self, x: Rgba) {
        self.bytes(&[x.r, x.g, x.b, x.a]);
    }

    /// Writes a `SoundPos`.
    pub(crate) fn sound_pos(&mut self, x: [f32; 3]) {
        self.block_pos(x.map(|v| go_float_to_i32(v * SOUND_POS_UNITS_PER_BLOCK)));
    }

    /// Writes a `BlockPos`.
    pub(crate) fn block_pos(&mut self, x: BlockPos) {
        for v in x {
            self.varint32(v);
        }
    }

    /// Writes a `SubChunkPos`.
    pub(crate) fn sub_chunk_pos(&mut self, x: SubChunkPos) {
        for v in x {
            self.int32(v);
        }
    }

    /// Writes a `UUID`.
    pub(crate) fn uuid(&mut self, x: Uuid) {
        let mut b = x.0;
        let (high, low) = b.split_at_mut(8);
        high.reverse();
        low.reverse();
        self.bytes(&b);
    }

    /// Writes a `Vec2`.
    pub(crate) fn vec2(&mut self, x: [f32; 2]) {
        for v in x {
            self.float32(v);
        }
    }

    /// Writes a `Vec3`.
    pub(crate) fn vec3(&mut self, x: [f32; 3]) {
        for v in x {
            self.float32(v);
        }
    }
}

/// The low byte of `f` converted like Go's `uint8(f)` on amd64.
///
/// The float is truncated to int64 (out of range and NaN give 0) and its low byte is kept.
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: the range check keeps the cast exact, like gophertunnel"
)]
fn go_float_to_u8(f: f32) -> u8 {
    if f.abs() < FLOAT_TO_INT64_LIMIT {
        (f as i64).to_le_bytes()[0]
    } else {
        0
    }
}

/// `f` converted like Go's `int32(f)` on amd64: out of range and NaN give `i32::MIN`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: the range check keeps the cast exact, like gophertunnel"
)]
fn go_float_to_i32(f: f32) -> i32 {
    if (-FLOAT_TO_INT32_LIMIT..FLOAT_TO_INT32_LIMIT).contains(&f) {
        f as i32
    } else {
        i32::MIN
    }
}

/// The value of an ASCII hex digit, or `None` for any other byte.
fn hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::fuzz;

    fn encoded(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        f(&mut w);
        w.into_inner()
    }

    #[test]
    fn uuids_reverse_each_half_like_gophertunnel() {
        let id = Uuid::from_bytes(std::array::from_fn(|i| u8::try_from(i).unwrap()));
        let bytes = encoded(|w| w.uuid(id));
        assert_eq!(
            bytes,
            [7, 6, 5, 4, 3, 2, 1, 0, 15, 14, 13, 12, 11, 10, 9, 8]
        );
        assert_eq!(Reader::new(&bytes, 0, 0).uuid(), Ok(id));
    }

    #[test]
    fn uuids_write_lowercase_hyphenated_text() {
        let id = Uuid::from_bytes(std::array::from_fn(|i| u8::try_from(i * 17).unwrap()));
        assert_eq!(id.to_string(), "00112233-4455-6677-8899-aabbccddeeff");
        assert_eq!(format!("{id:?}"), id.to_string());
        assert_eq!(
            Uuid::default().to_string(),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn uuids_read_the_forms_go_accepts_and_no_others() {
        let notch = Uuid::from_bytes([
            0xb5, 0x0a, 0xd3, 0x85, 0x82, 0x9d, 0x31, 0x41, 0xa2, 0x16, 0x7e, 0x7d, 0x75, 0x39,
            0xba, 0x7f,
        ]);
        for text in [
            "b50ad385-829d-3141-a216-7e7d7539ba7f",
            "B50AD385-829D-3141-A216-7E7D7539BA7F",
            "{b50ad385-829d-3141-a216-7e7d7539ba7f}",
            "urn:uuid:b50ad385-829d-3141-a216-7e7d7539ba7f",
            "URN:UUID:b50ad385-829d-3141-a216-7e7d7539ba7f",
            "b50ad385829d3141a2167e7d7539ba7f",
        ] {
            assert_eq!(text.parse::<Uuid>(), Ok(notch), "{text}");
        }
        for text in [
            "",
            "b50ad385-829d-3141-a216-7e7d7539ba7",
            "b50ad385-829d-3141-a216_7e7d7539ba7f",
            "b50ad385x829d-3141-a216-7e7d7539ba7f",
            "g50ad385-829d-3141-a216-7e7d7539ba7f",
            "(b50ad385-829d-3141-a216-7e7d7539ba7f)",
            "urn:uuix:b50ad385-829d-3141-a216-7e7d7539ba7f",
            "+50ad385829d3141a2167e7d7539ba7f",
            "b50ad385-829d3141a2167e7d7539ba7f",
            "b50ad385-829d-3141-a216-7e7d7539ba7-f",
        ] {
            assert!(text.parse::<Uuid>().is_err(), "{text}");
        }
    }

    #[test]
    fn colors_use_gophertunnel_byte_orders() {
        let c = Rgba {
            r: 1,
            g: 2,
            b: 3,
            a: 4,
        };
        assert_eq!(encoded(|w| w.rgba(c)), [1, 2, 3, 4]);
        assert_eq!(encoded(|w| w.be_argb(c)), [3, 2, 1, 4]);
        assert_eq!(Reader::new(&[1, 2, 3, 4], 0, 0).rgba(), Ok(c));
        assert_eq!(Reader::new(&[3, 2, 1, 4], 0, 0).be_argb(), Ok(c));
        let rgb = encoded(|w| w.rgb(c));
        assert_eq!(rgb[..4], (1.0_f32 / 255.0).to_le_bytes());
        assert_eq!(
            Reader::new(&rgb, 0, 0).rgb(),
            Ok(Rgba { a: 0, ..c }),
            "alpha is not on the wire"
        );
    }

    #[test]
    fn rgb_channels_survive_the_float_round_trip() {
        for v in 0..=255_u8 {
            let c = Rgba {
                r: v,
                g: v,
                b: v,
                a: 0,
            };
            assert_eq!(Reader::new(&encoded(|w| w.rgb(c)), 0, 0).rgb(), Ok(c));
        }
    }

    #[test]
    fn rotations_wrap_like_go_conversions() {
        assert_eq!(encoded(|w| w.byte_float(90.0)), [64]);
        assert_eq!(encoded(|w| w.byte_float(-90.0)), [192]);
        assert_eq!(encoded(|w| w.byte_float(f32::NAN)), [0]);
        assert_eq!(encoded(|w| w.byte_float(f32::INFINITY)), [0]);
        assert_eq!(Reader::new(&[64], 0, 0).byte_float(), Ok(90.0));
    }

    #[test]
    fn positions_round_trip() {
        let bytes = encoded(|w| {
            w.vec3([1.0, -2.0, 0.5]);
            w.vec2([3.0, 4.0]);
            w.block_pos([1, -1, i32::MAX]);
            w.chunk_pos([-5, 6]);
            w.sub_chunk_pos([7, -8, 9]);
            w.sound_pos([1.5, -2.0, 0.125]);
        });
        assert_eq!(bytes[bytes.len() - 3..], [24, 31, 2]);
        let mut r = Reader::new(&bytes, 0, 0);
        assert_eq!(r.vec3(), Ok([1.0, -2.0, 0.5]));
        assert_eq!(r.vec2(), Ok([3.0, 4.0]));
        assert_eq!(r.block_pos(), Ok([1, -1, i32::MAX]));
        assert_eq!(r.chunk_pos(), Ok([-5, 6]));
        assert_eq!(r.sub_chunk_pos(), Ok([7, -8, 9]));
        assert_eq!(r.sound_pos(), Ok([1.5, -2.0, 0.125]));
        assert!(r.remaining().is_empty());
        assert_eq!(encoded(|w| w.sound_pos([f32::NAN, 1e30, 0.0])), {
            let mut w = Writer::new(Vec::new(), 0);
            w.block_pos([i32::MIN, i32::MIN, 0]);
            w.into_inner()
        });
    }

    #[test]
    fn bitsets_write_a_varuint_of_their_bits() {
        let size = 131;
        let mut x = Bitset::new(size);
        assert_eq!(encoded(|w| w.bitset(&x, size).unwrap()), [0]);
        x.set(7).unwrap();
        assert_eq!(encoded(|w| w.bitset(&x, size).unwrap()), [0x80, 0x01]);
        x.set(0).unwrap();
        x.set(130).unwrap();
        x.unset(7).unwrap();
        assert_eq!(x.set(131), Err(BITSET_INDEX_PAST_END));
        assert_eq!(x.unset(131), Err(BITSET_INDEX_PAST_END));
        let bytes = encoded(|w| w.bitset(&x, size).unwrap());
        assert_eq!(bytes.len(), 19);
        let got = Reader::new(&bytes, 64, 0).bitset(size).unwrap();
        assert_eq!(got, x);
        assert!(got.get(0) && got.get(130) && !got.get(7) && !got.get(500));
        assert!(Writer::new(Vec::new(), 0).bitset(&x, 130).is_err());
    }

    #[test]
    fn bitsets_reject_bits_past_their_size() {
        assert!(Reader::new(&[0x80, 0x02], 64, 0).bitset(8).is_err());
        assert!(Reader::new(&[0x80, 0x01], 64, 0).bitset(8).is_ok());
        assert!(Reader::new(&[0x80, 0x80], 64, 0).bitset(14).is_err());
        assert!(Reader::new(&[0x00], 64, 0).bitset(0).is_err());
        assert_eq!(Reader::new(&[0x00], 0, 0).bitset(8), Err(Error::Size));
    }

    #[test]
    fn mutated_input_never_panics() {
        let mut x = Bitset::new(131);
        x.set(100).unwrap();
        let seed = encoded(|w| {
            w.uuid(Uuid::from_bytes([9; 16]));
            w.sound_pos([1.0, 2.0, 3.0]);
            w.bitset(&x, 131).unwrap();
            w.be_argb(Rgba::default());
        });
        let reencode = |data: &[u8]| -> Result<Vec<u8>> {
            let mut r = Reader::new(data, 1 << 20, 0);
            let id = r.uuid()?;
            let pos = r.sound_pos()?;
            let bits = r.bitset(131)?;
            let color = r.be_argb()?;
            let mut w = Writer::new(Vec::new(), 0);
            w.uuid(id);
            w.sound_pos(pos);
            w.bitset(&bits, 131)?;
            w.be_argb(color);
            Ok(w.into_inner())
        };
        let decoded = fuzz(&seed, 4000, |data| {
            let again = reencode(data)?;
            assert_eq!(reencode(&again), Ok(again));
            Ok(())
        });
        assert!(decoded > 0);
    }
}
