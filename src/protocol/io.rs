//! Byte reader and writer for packet codecs, the decode limits, and the codec error.
//!
//! [`Reader`] and [`Writer`] mirror gophertunnel's `protocol.IO`. Every `IO` method has a
//! `Reader` method that returns the value and a `Writer` method that takes it. The method name
//! is the Go name in `snake_case`, with acronyms written as one word:
//! `Uint8` → `uint8`, `Varuint64` → `varuint64`, `BEInt32` → `be_int32`, `StringUTF` →
//! `string_utf`, `ByteSlice` → `byte_slice`, `UUID` → `uuid`, `BEARGB` → `be_argb`,
//! `ActorUniqueIDVaruint64` → `actor_unique_id_varuint64`. `NBTList` has no form because no
//! packet uses it.
//! Writers take `Copy` values by value and other values by reference. A writer returns
//! [`Result`] only when it can fail (a length that does not fit its prefix, a value the wire
//! cannot carry); the infallible ones return nothing.
//!
//! Decode limits all go through [`Reader::limit`] and all fail with
//! [`Error::Size`]: a declared byte length is at most the remaining input, a list count is at
//! most [`MAX_LIST_LEN`] and the remaining input, and every allocation first takes
//! `count × size_of` from the reader's allocation budget; a map's entries take its tree nodes
//! ([`map_charges`]). The `_max` readers also take a field's own limit (Mojang `maxLength`,
//! `maxItems`), which the generated decoders pass. Lists nest at most [`MAX_LIST_DEPTH`] deep.

use std::fmt;
use std::mem::size_of;
use std::str;
use std::sync::Arc;

/// The most entries a node of a `BTreeMap` holds (std `CAPACITY`).
const MAP_NODE_ENTRIES: usize = 11;
/// The fewest entries a node of a `BTreeMap` other than the root holds while the map only grows.
///
/// This is std's `MIN_LEN_AFTER_SPLIT`.
const MAP_NODE_MIN_ENTRIES: usize = 5;
/// The deepest lists nest, a packet's own lists included.
///
/// Only a type that holds itself (`DataStorePropertyValue`) nests past a few, and its decoder
/// recurses once per list: without a bound, one packet nests until the thread stack overflows,
/// which ends the process.
pub(crate) const MAX_LIST_DEPTH: usize = 64;
/// The largest element count of a protocol list (gophertunnel `maxSliceLength`).
pub(crate) const MAX_LIST_LEN: usize = 1024;

/// A failure to decode or encode a protocol value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// An input that ends before its value is complete.
    UnexpectedEnd,
    /// A length, count, nesting depth or allocation over its limit.
    ///
    /// It is also a length that does not fit its wire prefix.
    Size,
    /// A variable-length integer that does not end within 5 (32-bit) or 10 (64-bit) bytes.
    VarintOverflow,
    /// A bool byte that is neither 0 nor 1.
    InvalidBool(u8),
    /// A string that is not valid UTF-8.
    InvalidUtf8,
    /// A tag or type selector with no known meaning.
    UnknownEnum {
        /// The enum the selector belongs to.
        name: &'static str,
        /// The selector read from the input.
        value: i64,
    },
    /// A value that is not allowed in its field.
    InvalidValue {
        /// The field holding the value.
        field: &'static str,
        /// Why the value is not allowed.
        reason: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => f.write_str("input ended inside a value"),
            Self::Size => f.write_str("length, count, depth or allocation over its limit"),
            Self::VarintOverflow => f.write_str("varint does not terminate"),
            Self::InvalidBool(b) => write!(f, "bool byte {b} is neither 0 nor 1"),
            Self::InvalidUtf8 => f.write_str("string is not valid utf-8"),
            Self::UnknownEnum { name, value } => write!(f, "unknown {name} {value}"),
            Self::InvalidValue { field, reason } => write!(f, "invalid {field}: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

/// The result of a codec operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A value with a single wire form, read from a [`Reader`] and written to a [`Writer`].
pub(crate) trait Codec: Sized {
    /// Reads one value.
    fn decode(r: &mut Reader<'_>) -> Result<Self>;
    /// Writes the value.
    fn encode(&self, w: &mut Writer) -> Result<()>;
}

/// A reader of protocol values from a byte slice, under the decode limits.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    input: &'a [u8],
    budget: usize,
    shield_id: i32,
    /// The lists the value being read is in.
    depth: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `input` that may allocate `budget` bytes.
    ///
    /// `shield_id` is the shield's item network ID, which changes the item wire format.
    pub(crate) fn new(input: &'a [u8], budget: usize, shield_id: i32) -> Self {
        Self {
            input,
            budget,
            shield_id,
            depth: 0,
        }
    }

    /// The allocation budget left, to hand to the reader of the next packet in a batch.
    pub(crate) fn budget(&self) -> usize {
        self.budget
    }

    /// The input not read yet.
    pub(crate) fn remaining(&self) -> &'a [u8] {
        self.input
    }

    /// The shield's item network ID.
    pub(crate) fn shield_id(&self) -> i32 {
        self.shield_id
    }

    /// Checks a count and takes its allocation from the budget, returning the count.
    ///
    /// This is the one length and count check. It fails with [`Error::Size`] unless `count` is at
    /// most `max` and at most the remaining input bytes (every element takes at least one byte),
    /// and `count × elem_size` bytes fit the allocation budget, which it then takes.
    pub(crate) fn limit(&mut self, count: u64, max: usize, elem_size: usize) -> Result<usize> {
        let count = usize::try_from(count).map_err(|_| Error::Size)?;
        if count > max || count > self.input.len() {
            return Err(Error::Size);
        }
        let bytes = count.checked_mul(elem_size).ok_or(Error::Size)?;
        self.budget = self.budget.checked_sub(bytes).ok_or(Error::Size)?;
        Ok(count)
    }

    /// Reads a list of `count` elements with `f`, under the [`MAX_LIST_LEN`] limit.
    pub(crate) fn list<T>(
        &mut self,
        count: u64,
        f: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        self.list_max(count, MAX_LIST_LEN, f)
    }

    /// Reads a list like [`Reader::list`] for a field that holds at most `max` elements.
    ///
    /// It fails with [`Error::Size`] once [`MAX_LIST_DEPTH`] lists are open.
    pub(crate) fn list_max<T>(
        &mut self,
        count: u64,
        max: usize,
        mut f: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        if self.depth >= MAX_LIST_DEPTH {
            return Err(Error::Size);
        }
        let count = self.limit(count, max.min(MAX_LIST_LEN), size_of::<T>())?;
        let mut items = Vec::with_capacity(count);
        self.depth += 1;
        let mut result = Ok(());
        for _ in 0..count {
            match f(self) {
                Ok(item) => items.push(item),
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        self.depth -= 1;
        result.map(|()| items)
    }

    /// Runs `f` on a reader over `input` that shares this reader's state.
    ///
    /// The state is the budget, the shield ID and the list depth.
    pub(crate) fn nested<T>(
        &mut self,
        input: &[u8],
        f: impl FnOnce(&mut Reader<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut inner = Reader::new(input, self.budget, self.shield_id);
        inner.depth = self.depth;
        let value = f(&mut inner);
        self.budget = inner.budget;
        value
    }

    /// Takes the next `N` bytes.
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let (head, rest) = self
            .input
            .split_first_chunk::<N>()
            .ok_or(Error::UnexpectedEnd)?;
        self.input = rest;
        Ok(*head)
    }

    /// Takes `len` bytes after the limit check, as a slice of the input.
    pub(crate) fn raw(&mut self, len: u64) -> Result<&'a [u8]> {
        let len = self.limit(len, usize::MAX, 0)?;
        self.take(len)
    }

    /// Takes `len` bytes after the limit check as an owned buffer.
    pub(crate) fn byte_vec(&mut self, len: u64) -> Result<Vec<u8>> {
        self.byte_vec_max(len, usize::MAX)
    }

    /// Takes `len` bytes after the limit check as an owned string.
    pub(crate) fn utf8(&mut self, len: u64) -> Result<String> {
        self.utf8_max(len, usize::MAX)
    }

    /// Reads a `Uint8`.
    pub(crate) fn uint8(&mut self) -> Result<u8> {
        let [b] = self.array()?;
        Ok(b)
    }

    /// Reads an `Int8`.
    pub(crate) fn int8(&mut self) -> Result<i8> {
        Ok(self.uint8()?.cast_signed())
    }

    /// Reads a `Bool`, accepting only 0 and 1.
    pub(crate) fn bool(&mut self) -> Result<bool> {
        match self.uint8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(Error::InvalidBool(b)),
        }
    }

    /// Reads a `Uint16`.
    pub(crate) fn uint16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// Reads an `Int16`.
    pub(crate) fn int16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }

    /// Reads a `Uint32`.
    pub(crate) fn uint32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// Reads an `Int32`.
    pub(crate) fn int32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// Reads a `BEInt32`.
    pub(crate) fn be_int32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    /// Reads a `Uint64`.
    pub(crate) fn uint64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Reads an `Int64`.
    pub(crate) fn int64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// Reads a `Float32`.
    pub(crate) fn float32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    /// Reads a `Float64`.
    pub(crate) fn float64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.array()?))
    }

    /// Reads a `Varuint32` of at most 5 bytes.
    ///
    /// Bits past 32 in the fifth byte are dropped, like gophertunnel.
    pub(crate) fn varuint32(&mut self) -> Result<u32> {
        let mut value = 0_u32;
        for shift in (0..35).step_by(7) {
            let b = self.uint8()?;
            value |= u32::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::VarintOverflow)
    }

    /// Reads a `Varint32`: zigzag over [`Self::varuint32`].
    pub(crate) fn varint32(&mut self) -> Result<i32> {
        let u = self.varuint32()?;
        let x = (u >> 1).cast_signed();
        Ok(if u & 1 == 0 { x } else { !x })
    }

    /// Reads a `Varuint64` of at most 10 bytes.
    ///
    /// Bits past 64 in the tenth byte are dropped, like gophertunnel.
    pub(crate) fn varuint64(&mut self) -> Result<u64> {
        let mut value = 0_u64;
        for shift in (0..70).step_by(7) {
            let b = self.uint8()?;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::VarintOverflow)
    }

    /// Reads a `Varint64`: zigzag over [`Self::varuint64`].
    pub(crate) fn varint64(&mut self) -> Result<i64> {
        let u = self.varuint64()?;
        let x = (u >> 1).cast_signed();
        Ok(if u & 1 == 0 { x } else { !x })
    }

    /// Reads an `ActorRuntimeID`: a varuint64.
    pub(crate) fn actor_runtime_id(&mut self) -> Result<u64> {
        self.varuint64()
    }

    /// Reads an `ActorRuntimeIDVarint64`.
    pub(crate) fn actor_runtime_id_varint64(&mut self) -> Result<i64> {
        self.varint64()
    }

    /// Reads an `ActorRuntimeIDVaruint32`.
    pub(crate) fn actor_runtime_id_varuint32(&mut self) -> Result<u32> {
        self.varuint32()
    }

    /// Reads an `ActorUniqueID`: a varint64.
    pub(crate) fn actor_unique_id(&mut self) -> Result<i64> {
        self.varint64()
    }

    /// Reads an `ActorUniqueIDInt64`.
    pub(crate) fn actor_unique_id_int64(&mut self) -> Result<i64> {
        self.int64()
    }

    /// Reads an `ActorUniqueIDUint64`.
    pub(crate) fn actor_unique_id_uint64(&mut self) -> Result<u64> {
        self.uint64()
    }

    /// Reads an `ActorUniqueIDVaruint64`.
    pub(crate) fn actor_unique_id_varuint64(&mut self) -> Result<u64> {
        self.varuint64()
    }

    /// Reads a `String`: a varuint32 byte length and UTF-8 bytes.
    pub(crate) fn string(&mut self) -> Result<String> {
        self.string_max(usize::MAX)
    }

    /// Reads a `String` of a field that holds at most `max` bytes.
    pub(crate) fn string_max(&mut self, max: usize) -> Result<String> {
        let len = self.varuint32()?;
        self.utf8_max(len.into(), max)
    }

    /// Reads a `StringUTF`: an int16 byte length and UTF-8 bytes.
    ///
    /// A negative length is a size error.
    pub(crate) fn string_utf(&mut self) -> Result<String> {
        let len = u64::try_from(self.int16()?).map_err(|_| Error::Size)?;
        self.utf8(len)
    }

    /// Reads a `ByteSlice`: a varuint32 byte length and the bytes.
    pub(crate) fn byte_slice(&mut self) -> Result<Vec<u8>> {
        self.byte_slice_max(usize::MAX)
    }

    /// Reads a `ByteSlice` of a field that holds at most `max` bytes.
    pub(crate) fn byte_slice_max(&mut self, max: usize) -> Result<Vec<u8>> {
        let len = self.varuint32()?;
        self.byte_vec_max(len.into(), max)
    }

    /// Reads the `Bytes` that are the rest of the input.
    pub(crate) fn bytes(&mut self) -> Result<Vec<u8>> {
        self.byte_vec(length(self.input.len())?)
    }

    /// Takes the next `len` bytes without a limit check; callers check `len` first.
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (head, rest) = self
            .input
            .split_at_checked(len)
            .ok_or(Error::UnexpectedEnd)?;
        self.input = rest;
        Ok(head)
    }

    /// Takes `len` bytes like [`Reader::byte_vec`], failing past `max` bytes.
    fn byte_vec_max(&mut self, len: u64, max: usize) -> Result<Vec<u8>> {
        let len = self.limit(len, max, 1)?;
        Ok(self.take(len)?.to_vec())
    }

    /// Takes `len` bytes like [`Reader::utf8`], failing past `max` bytes.
    fn utf8_max(&mut self, len: u64, max: usize) -> Result<String> {
        let len = self.limit(len, max, 1)?;
        let bytes = self.take(len)?;
        str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| Error::InvalidUtf8)
    }
}

/// A writer of protocol values into a byte buffer.
#[derive(Debug)]
pub(crate) struct Writer {
    buf: Vec<u8>,
    shield_id: i32,
}

impl Writer {
    /// A writer that appends to `buf`.
    ///
    /// `shield_id` is the shield's item network ID, which changes the item wire format.
    pub(crate) fn new(buf: Vec<u8>, shield_id: i32) -> Self {
        Self { buf, shield_id }
    }

    /// The shield's item network ID.
    pub(crate) fn shield_id(&self) -> i32 {
        self.shield_id
    }

    /// The buffer, to append what an encoder that takes a `Vec` writes.
    pub(crate) fn buffer_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Writes what `f` appends, prefixed with its byte length as a varuint32 (a `ByteSlice`).
    #[expect(
        clippy::expect_used,
        reason = "invariant: `f` only appends, so `start` is a length the buffer had before"
    )]
    pub(crate) fn length_prefixed(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let start = self.buf.len();
        f(self)?;
        let body = self
            .buf
            .len()
            .checked_sub(start)
            .expect("the body only grows the buffer");
        let end = self.buf.len();
        self.varuint32(length(body)?);
        let prefix = self.buf.len() - end;
        // Move the prefix written after the body in front of it, without a second buffer.
        self.buf
            .get_mut(start..)
            .expect("start is inside the buffer")
            .rotate_right(prefix);
        Ok(())
    }

    /// Writes a `Uint8`.
    pub(crate) fn uint8(&mut self, x: u8) {
        self.buf.push(x);
    }

    /// Writes an `Int8`.
    pub(crate) fn int8(&mut self, x: i8) {
        self.buf.push(x.cast_unsigned());
    }

    /// Writes a `Bool`.
    pub(crate) fn bool(&mut self, x: bool) {
        self.buf.push(u8::from(x));
    }

    /// Writes a `Uint16`.
    pub(crate) fn uint16(&mut self, x: u16) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes an `Int16`.
    pub(crate) fn int16(&mut self, x: i16) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes a `Uint32`.
    pub(crate) fn uint32(&mut self, x: u32) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes an `Int32`.
    pub(crate) fn int32(&mut self, x: i32) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes a `BEInt32`.
    pub(crate) fn be_int32(&mut self, x: i32) {
        self.buf.extend_from_slice(&x.to_be_bytes());
    }

    /// Writes a `Uint64`.
    pub(crate) fn uint64(&mut self, x: u64) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes an `Int64`.
    pub(crate) fn int64(&mut self, x: i64) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes a `Float32`.
    pub(crate) fn float32(&mut self, x: f32) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes a `Float64`.
    pub(crate) fn float64(&mut self, x: f64) {
        self.buf.extend_from_slice(&x.to_le_bytes());
    }

    /// Writes a `Varuint32`.
    pub(crate) fn varuint32(&mut self, x: u32) {
        self.varuint64(x.into());
    }

    /// Writes a `Varint32`.
    pub(crate) fn varint32(&mut self, x: i32) {
        let u = x.cast_unsigned() << 1;
        self.varuint32(if x < 0 { !u } else { u });
    }

    /// Writes a `Varuint64`.
    pub(crate) fn varuint64(&mut self, mut x: u64) {
        while x >= 0x80 {
            self.buf.push(x.to_le_bytes()[0] | 0x80);
            x >>= 7;
        }
        self.buf.push(x.to_le_bytes()[0]);
    }

    /// Writes a `Varint64`.
    pub(crate) fn varint64(&mut self, x: i64) {
        let u = x.cast_unsigned() << 1;
        self.varuint64(if x < 0 { !u } else { u });
    }

    /// Writes an `ActorRuntimeID`.
    pub(crate) fn actor_runtime_id(&mut self, x: u64) {
        self.varuint64(x);
    }

    /// Writes an `ActorRuntimeIDVarint64`.
    pub(crate) fn actor_runtime_id_varint64(&mut self, x: i64) {
        self.varint64(x);
    }

    /// Writes an `ActorRuntimeIDVaruint32`.
    pub(crate) fn actor_runtime_id_varuint32(&mut self, x: u32) {
        self.varuint32(x);
    }

    /// Writes an `ActorUniqueID`.
    pub(crate) fn actor_unique_id(&mut self, x: i64) {
        self.varint64(x);
    }

    /// Writes an `ActorUniqueIDInt64`.
    pub(crate) fn actor_unique_id_int64(&mut self, x: i64) {
        self.int64(x);
    }

    /// Writes an `ActorUniqueIDUint64`.
    pub(crate) fn actor_unique_id_uint64(&mut self, x: u64) {
        self.uint64(x);
    }

    /// Writes an `ActorUniqueIDVaruint64`.
    pub(crate) fn actor_unique_id_varuint64(&mut self, x: u64) {
        self.varuint64(x);
    }

    /// Writes a `String`.
    pub(crate) fn string(&mut self, x: &str) -> Result<()> {
        self.byte_slice(x.as_bytes())
    }

    /// Writes a `String` of a field that holds at most `max` bytes.
    ///
    /// More is [`Error::Size`], as the reader would answer.
    pub(crate) fn string_max(&mut self, x: &str, max: usize) -> Result<()> {
        self.byte_slice_max(x.as_bytes(), max)
    }

    /// Writes a `StringUTF`, failing with [`Error::Size`] past `i16::MAX` bytes.
    pub(crate) fn string_utf(&mut self, x: &str) -> Result<()> {
        self.int16(length(x.len())?);
        self.bytes(x.as_bytes());
        Ok(())
    }

    /// Writes a `ByteSlice`, failing with [`Error::Size`] past `u32::MAX` bytes.
    pub(crate) fn byte_slice(&mut self, x: &[u8]) -> Result<()> {
        self.byte_slice_max(x, usize::MAX)
    }

    /// Writes a `ByteSlice` of a field that holds at most `max` bytes.
    ///
    /// More is [`Error::Size`].
    pub(crate) fn byte_slice_max(&mut self, x: &[u8], max: usize) -> Result<()> {
        self.varuint32(length_max(x.len(), max)?);
        self.bytes(x);
        Ok(())
    }

    /// Writes `Bytes`.
    pub(crate) fn bytes(&mut self, x: &[u8]) {
        self.buf.extend_from_slice(x);
    }

    /// The written bytes.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The buffer.
    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

/// A value many packets share (a skin).
///
/// Decoding allocates it on its own, so it takes that allocation (the value and the two
/// reference counts) from the budget like a list element.
impl<T: Codec> Codec for Arc<T> {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.limit(1, 1, size_of::<([usize; 2], T)>())?;
        T::decode(r).map(Arc::new)
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        T::encode(self, w)
    }
}

/// A length as its wire prefix type, or [`Error::Size`] past `max` or if it does not fit.
pub(crate) fn length_max<T: TryFrom<usize>>(len: usize, max: usize) -> Result<T> {
    if len > max {
        return Err(Error::Size);
    }
    length(len)
}

/// A length as its wire prefix type, or [`Error::Size`] if it does not fit.
pub(crate) fn length<T: TryFrom<usize>>(len: usize) -> Result<T> {
    T::try_from(len).map_err(|_| Error::Size)
}

/// What a `BTreeMap` takes from a reader's budget for its first entry and for each other.
///
/// The entries are `entry_size` bytes. The map allocates its entries in nodes, a whole node for
/// the first, and while it only grows (decoding inserts, a repeated key replaces) `n` entries take
/// at most `1 + (n - 1) / 5` nodes: the first entry takes a node and every other a fifth of one.
/// A node is counted as an internal one: its parent link and lengths, the entries and 12 child
/// links.
pub(crate) const fn map_charges(entry_size: usize) -> (usize, usize) {
    let node = 2 * size_of::<usize>()
        + MAP_NODE_ENTRIES * entry_size
        + (MAP_NODE_ENTRIES + 1) * size_of::<usize>();
    (node, node.div_ceil(MAP_NODE_MIN_ENTRIES))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const BUDGET: usize = 1 << 20;

    pub(crate) fn reader(input: &[u8]) -> Reader<'_> {
        Reader::new(input, BUDGET, 0)
    }

    pub(crate) fn writer() -> Writer {
        Writer::new(Vec::new(), 0)
    }

    /// A fixed-seed generator for mutation tests (`SplitMix64`).
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        pub(crate) fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(n.max(1)).unwrap()).unwrap()
        }
    }

    /// Mutates `seed` `cases` times and runs `decode` on each result.
    ///
    /// The mutations are bit flips, inserted, removed and replaced bytes, and truncation. A panic
    /// fails the test. Returns how many inputs decoded.
    pub(crate) fn fuzz(
        seed: &[u8],
        cases: usize,
        mut decode: impl FnMut(&[u8]) -> Result<()>,
    ) -> usize {
        let mut rng = Rng(0x00a1_c0de);
        let mut decoded = usize::from(decode(seed).is_ok());
        for _ in 0..cases {
            let mut data = seed.to_vec();
            for _ in 0..=rng.below(4) {
                let at = rng.below(data.len());
                match rng.below(5) {
                    0 if !data.is_empty() => data[at] ^= 1 << rng.below(8),
                    1 => data.insert(at.min(data.len()), rng.next().to_le_bytes()[0]),
                    2 if !data.is_empty() => {
                        data.remove(at);
                    }
                    3 => data.truncate(at),
                    _ if !data.is_empty() => data[at] = [0xff, 0x7f, 0x80, 0][rng.below(4)],
                    _ => {}
                }
            }
            decoded += usize::from(decode(&data).is_ok());
        }
        decoded
    }

    /// Fuzzes a codec from `seed`.
    ///
    /// Every mutated input that `read` accepts must re-encode with `write` to bytes that decode
    /// and re-encode to themselves. Returns how many decoded.
    pub(crate) fn fuzz_round_trip<T>(
        seed: &[u8],
        shield_id: i32,
        read: impl Fn(&mut Reader<'_>) -> Result<T>,
        write: impl Fn(&mut Writer, &T) -> Result<()>,
    ) -> usize {
        let reencode = |data: &[u8]| -> Result<Vec<u8>> {
            let mut r = Reader::new(data, BUDGET, shield_id);
            let value = read(&mut r)?;
            let mut w = Writer::new(Vec::new(), shield_id);
            write(&mut w, &value)?;
            Ok(w.into_inner())
        };
        assert!(reencode(seed).is_ok(), "the seed must decode");
        fuzz(seed, 3000, |data| {
            let again = reencode(data)?;
            assert_eq!(reencode(&again), Ok(again));
            Ok(())
        })
    }

    fn encoded(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = writer();
        f(&mut w);
        w.into_inner()
    }

    #[test]
    fn varints_match_gophertunnel_bytes() {
        assert_eq!(encoded(|w| w.varuint32(0)), [0]);
        assert_eq!(encoded(|w| w.varuint32(127)), [0x7f]);
        assert_eq!(encoded(|w| w.varuint32(128)), [0x80, 0x01]);
        assert_eq!(
            encoded(|w| w.varuint32(u32::MAX)),
            [0xff, 0xff, 0xff, 0xff, 0x0f]
        );
        assert_eq!(encoded(|w| w.varint32(-1)), [0x01]);
        assert_eq!(encoded(|w| w.varint32(1)), [0x02]);
        assert_eq!(
            encoded(|w| w.varint32(i32::MIN)),
            [0xff, 0xff, 0xff, 0xff, 0x0f]
        );
        assert_eq!(
            encoded(|w| w.varint32(i32::MAX)),
            [0xfe, 0xff, 0xff, 0xff, 0x0f]
        );
        assert_eq!(
            encoded(|w| w.varuint64(u64::MAX)),
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
        assert_eq!(encoded(|w| w.varint64(i64::MIN)).len(), 10);
    }

    #[test]
    fn integers_round_trip_at_their_edges() {
        for x in [0, 1, -1, 63, -64, 64, i32::MAX, i32::MIN] {
            assert_eq!(reader(&encoded(|w| w.varint32(x))).varint32(), Ok(x));
        }
        for x in [0, 1, -1, i64::MAX, i64::MIN] {
            assert_eq!(reader(&encoded(|w| w.varint64(x))).varint64(), Ok(x));
        }
        for x in [0, 0x7f, 0x80, u64::MAX] {
            assert_eq!(reader(&encoded(|w| w.varuint64(x))).varuint64(), Ok(x));
        }
        let bytes = encoded(|w| {
            w.uint16(0xbeef);
            w.int16(-2);
            w.uint32(7);
            w.int32(-7);
            w.be_int32(0x0102_0304);
            w.uint64(u64::MAX);
            w.int64(i64::MIN);
            w.float32(1.5);
            w.float64(-2.25);
            w.int8(-3);
            w.bool(true);
        });
        assert_eq!(bytes[12..16], [1, 2, 3, 4]);
        let mut r = reader(&bytes);
        assert_eq!(r.uint16(), Ok(0xbeef));
        assert_eq!(r.int16(), Ok(-2));
        assert_eq!(r.uint32(), Ok(7));
        assert_eq!(r.int32(), Ok(-7));
        assert_eq!(r.be_int32(), Ok(0x0102_0304));
        assert_eq!(r.uint64(), Ok(u64::MAX));
        assert_eq!(r.int64(), Ok(i64::MIN));
        assert_eq!(r.float32(), Ok(1.5));
        assert_eq!(r.float64(), Ok(-2.25));
        assert_eq!(r.int8(), Ok(-3));
        assert_eq!(r.bool(), Ok(true));
        assert!(r.remaining().is_empty());
        assert_eq!(r.uint8(), Err(Error::UnexpectedEnd));
    }

    #[test]
    fn overlong_varints_are_rejected() {
        assert_eq!(reader(&[0x80; 5]).varuint32(), Err(Error::VarintOverflow));
        assert_eq!(reader(&[0x80; 10]).varuint64(), Err(Error::VarintOverflow));
        // gophertunnel drops bits past the width in the last byte.
        assert_eq!(
            reader(&[0xff, 0xff, 0xff, 0xff, 0x7f]).varuint32(),
            Ok(u32::MAX)
        );
        assert_eq!(reader(&[0x80]).varuint32(), Err(Error::UnexpectedEnd));
    }

    #[test]
    fn bools_other_than_zero_and_one_are_rejected() {
        assert_eq!(reader(&[0]).bool(), Ok(false));
        assert_eq!(reader(&[2]).bool(), Err(Error::InvalidBool(2)));
    }

    #[test]
    fn strings_round_trip_and_reject_invalid_utf8() {
        let bytes = encoded(|w| {
            w.string("héllo").unwrap();
            w.string_utf("abc").unwrap();
            w.byte_slice(&[1, 2, 3]).unwrap();
            w.bytes(&[9, 9]);
        });
        assert_eq!(bytes[..7], [6, b'h', 0xc3, 0xa9, b'l', b'l', b'o']);
        let mut r = reader(&bytes);
        assert_eq!(r.string().as_deref(), Ok("héllo"));
        assert_eq!(r.string_utf().as_deref(), Ok("abc"));
        assert_eq!(r.byte_slice(), Ok(vec![1, 2, 3]));
        assert_eq!(r.bytes(), Ok(vec![9, 9]));
        assert_eq!(r.bytes(), Ok(Vec::new()));
        assert_eq!(reader(&[2, 0xc3, 0x28]).string(), Err(Error::InvalidUtf8));
    }

    #[test]
    fn declared_lengths_over_the_input_are_size_errors() {
        assert_eq!(reader(&[4, 1, 2, 3]).string(), Err(Error::Size));
        assert_eq!(reader(&[4, 1, 2, 3]).byte_slice(), Err(Error::Size));
        assert_eq!(reader(&[0xff, 0xff, 1]).string_utf(), Err(Error::Size));
        assert_eq!(reader(&[5, 0, 1]).string_utf(), Err(Error::Size));
    }

    #[test]
    fn list_counts_over_the_limit_are_size_errors() {
        let input = vec![0; 2048];
        let mut r = reader(&input);
        assert_eq!(r.list(1025, Reader::uint8), Err(Error::Size));
        assert_eq!(r.list(1024, Reader::uint8).map(|v| v.len()), Ok(1024));
        assert_eq!(reader(&[0; 3]).list(4, Reader::uint8), Err(Error::Size));
    }

    #[test]
    fn budget_exhaustion_is_a_size_error() {
        let input = [3, b'a', b'b', b'c', 3, b'd', b'e', b'f'];
        let mut r = Reader::new(&input, 5, 0);
        assert_eq!(r.string().as_deref(), Ok("abc"));
        assert_eq!(r.budget(), 2);
        assert_eq!(r.string(), Err(Error::Size));
        let mut r = Reader::new(&[0; 64], 60, 0);
        assert_eq!(r.list(2, |r| r.list(1, Reader::uint64)), Err(Error::Size));
    }

    #[test]
    fn nested_readers_share_the_budget() {
        let mut r = Reader::new(&[], 10, 7);
        let got = r.nested(&[2, b'h', b'i'], |inner| {
            assert_eq!(inner.shield_id(), 7);
            inner.string()
        });
        assert_eq!(got.as_deref(), Ok("hi"));
        assert_eq!(r.budget(), 8);
    }

    #[test]
    fn encoding_never_truncates_a_length() {
        let long = "a".repeat(usize::try_from(i16::MAX).unwrap() + 1);
        assert_eq!(writer().string_utf(&long), Err(Error::Size));
        assert_eq!(length::<u8>(256), Err::<u8, _>(Error::Size));
        assert_eq!(length::<u8>(255), Ok(255_u8));
    }

    #[test]
    fn length_prefixed_writes_the_body_length_first() {
        let mut w = writer();
        w.uint8(9);
        w.length_prefixed(|w| {
            w.bytes(&[1; 200]);
            Ok(())
        })
        .unwrap();
        let bytes = w.into_inner();
        assert_eq!(bytes[..3], [9, 0xc8, 0x01]);
        assert_eq!(bytes.len(), 203);
    }

    #[test]
    fn mutated_primitive_input_never_panics() {
        let seed = encoded(|w| {
            w.varint64(-5);
            w.string("text").unwrap();
            w.string_utf("x").unwrap();
            w.byte_slice(&[1, 2]).unwrap();
            w.bool(true);
        });
        let decoded = fuzz(&seed, 5000, |data| {
            let mut r = reader(data);
            r.varint64()?;
            r.string()?;
            r.string_utf()?;
            r.byte_slice()?;
            r.bool()?;
            r.bytes()?;
            Ok(())
        });
        assert!(decoded > 0);
    }
}
