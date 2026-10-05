//! NBT values and their little-endian and network little-endian wire formats.
//!
//! Decoding mirrors gophertunnel's `nbt` package for byte equality: the root tag name is read
//! and dropped and written empty, a repeated compound key keeps its last value, and compound keys
//! are written in byte order. A list keeps its element type even when empty ([`List`]), so every
//! list reads and writes back to the same bytes. gophertunnel keeps the type only for empty
//! `TAG_Int` and `TAG_Long` lists and writes other empty lists with element type `TAG_End`.
//! Limits (PLAN.md 10.1): containers nest at most 512 deep counting the root, one root takes at
//! most 4 MiB on the wire (or a store's own limit), and lengths and allocations go through the
//! reader's limit check. Writing a root also counts what reading it back takes from a reader's
//! allocation budget (`Cost`), so a store can keep what it writes readable.
//! Reading and writing keep nested containers on a heap stack, not the thread stack.

use std::collections::{BTreeMap, btree_map};
use std::mem::size_of;
use std::slice;

use crate::protocol::{Error, Reader, Result, Writer};

/// The deepest nesting of compounds and lists, counting the root.
pub(crate) const MAX_DEPTH: usize = 512;
/// The most bytes one root tag may take on the wire.
pub(crate) const MAX_ROOT_BYTES: usize = 4 << 20;
/// The longest string in bytes (gophertunnel `maxStringSize`).
const MAX_STRING_LEN: u16 = 0x7fff;

/// The tag types, by number (the network block hash writes NBT tags too).
pub(crate) const TAG_END: u8 = 0;
pub(crate) const TAG_BYTE: u8 = 1;
const TAG_SHORT: u8 = 2;
pub(crate) const TAG_INT: u8 = 3;
const TAG_LONG: u8 = 4;
const TAG_FLOAT: u8 = 5;
const TAG_DOUBLE: u8 = 6;
const TAG_BYTE_ARRAY: u8 = 7;
pub(crate) const TAG_STRING: u8 = 8;
const TAG_LIST: u8 = 9;
pub(crate) const TAG_COMPOUND: u8 = 10;
const TAG_INT_ARRAY: u8 = 11;
const TAG_LONG_ARRAY: u8 = 12;

/// A compound tag: named values in key byte order.
pub type Compound = BTreeMap<String, Value>;

/// One NBT tag value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `TAG_Byte`, unsigned like gophertunnel.
    Byte(u8),
    /// `TAG_Short`
    Short(i16),
    /// `TAG_Int`
    Int(i32),
    /// `TAG_Long`
    Long(i64),
    /// `TAG_Float`
    Float(f32),
    /// `TAG_Double`
    Double(f64),
    /// `TAG_Byte_Array`
    ByteArray(Vec<u8>),
    /// `TAG_String`
    String(String),
    /// `TAG_List`
    List(List),
    /// `TAG_Compound`
    Compound(Compound),
    /// `TAG_Int_Array`
    IntArray(Vec<i32>),
    /// `TAG_Long_Array`
    LongArray(Vec<i64>),
}

impl Value {
    fn tag(&self) -> u8 {
        match self {
            Self::Byte(_) => TAG_BYTE,
            Self::Short(_) => TAG_SHORT,
            Self::Int(_) => TAG_INT,
            Self::Long(_) => TAG_LONG,
            Self::Float(_) => TAG_FLOAT,
            Self::Double(_) => TAG_DOUBLE,
            Self::ByteArray(_) => TAG_BYTE_ARRAY,
            Self::String(_) => TAG_STRING,
            Self::List(_) => TAG_LIST,
            Self::Compound(_) => TAG_COMPOUND,
            Self::IntArray(_) => TAG_INT_ARRAY,
            Self::LongArray(_) => TAG_LONG_ARRAY,
        }
    }
}

/// A `TAG_List`. The variant is the element type, so all elements have one type and an empty
/// list keeps its type.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum List {
    /// A list with element type `TAG_End`, which is always empty.
    #[default]
    End,
    /// `TAG_Byte` elements.
    Byte(Vec<u8>),
    /// `TAG_Short` elements.
    Short(Vec<i16>),
    /// `TAG_Int` elements.
    Int(Vec<i32>),
    /// `TAG_Long` elements.
    Long(Vec<i64>),
    /// `TAG_Float` elements.
    Float(Vec<f32>),
    /// `TAG_Double` elements.
    Double(Vec<f64>),
    /// `TAG_Byte_Array` elements.
    ByteArray(Vec<Vec<u8>>),
    /// `TAG_String` elements.
    String(Vec<String>),
    /// `TAG_List` elements; each may have its own element type.
    #[expect(
        clippy::enum_variant_names,
        reason = "consistency: every variant is named after its element tag"
    )]
    List(Vec<List>),
    /// `TAG_Compound` elements.
    Compound(Vec<Compound>),
    /// `TAG_Int_Array` elements.
    IntArray(Vec<Vec<i32>>),
    /// `TAG_Long_Array` elements.
    LongArray(Vec<Vec<i64>>),
}

impl List {
    /// Returns the number of elements.
    pub fn len(&self) -> usize {
        match self {
            Self::End => 0,
            Self::Byte(x) => x.len(),
            Self::Short(x) => x.len(),
            Self::Int(x) => x.len(),
            Self::Long(x) => x.len(),
            Self::Float(x) => x.len(),
            Self::Double(x) => x.len(),
            Self::ByteArray(x) => x.len(),
            Self::String(x) => x.len(),
            Self::List(x) => x.len(),
            Self::Compound(x) => x.len(),
            Self::IntArray(x) => x.len(),
            Self::LongArray(x) => x.len(),
        }
    }

    /// Returns whether the list has no elements.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The element tag type.
    fn tag(&self) -> u8 {
        match self {
            Self::End => TAG_END,
            Self::Byte(_) => TAG_BYTE,
            Self::Short(_) => TAG_SHORT,
            Self::Int(_) => TAG_INT,
            Self::Long(_) => TAG_LONG,
            Self::Float(_) => TAG_FLOAT,
            Self::Double(_) => TAG_DOUBLE,
            Self::ByteArray(_) => TAG_BYTE_ARRAY,
            Self::String(_) => TAG_STRING,
            Self::List(_) => TAG_LIST,
            Self::Compound(_) => TAG_COMPOUND,
            Self::IntArray(_) => TAG_INT_ARRAY,
            Self::LongArray(_) => TAG_LONG_ARRAY,
        }
    }
}

/// An NBT wire format (gophertunnel `nbt.Encoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Varint ints, longs and lengths; varuint32 string lengths. Used by packets.
    NetworkLittleEndian,
    /// Fixed-width little-endian ints and lengths; int16 string lengths. Used by item user data.
    LittleEndian,
}

/// A container being read. Nesting uses this explicit stack instead of recursion, so the
/// thread stack stays flat at any depth; the stack holds at most [`MAX_DEPTH`] frames. A list of
/// anything but lists or compounds is read whole when it opens and needs no frame.
enum ReadFrame {
    Compound { map: Compound, name: String },
    Lists { left: usize, items: Vec<List> },
    Compounds { left: usize, items: Vec<Compound> },
}

impl ReadFrame {
    fn compound() -> Self {
        Self::Compound {
            map: Compound::new(),
            name: String::new(),
        }
    }

    /// Adds a finished child: any value to a compound, a list or compound to a list of them.
    fn add(&mut self, value: Value) -> Result<()> {
        match (self, value) {
            (Self::Compound { map, name }, value) => {
                map.insert(std::mem::take(name), value);
            }
            (Self::Lists { items, .. }, Value::List(list)) => items.push(list),
            (Self::Compounds { items, .. }, Value::Compound(map)) => items.push(map),
            _ => return Err(STACK_ORDER),
        }
        Ok(())
    }

    fn finish(self) -> Value {
        match self {
            Self::Compound { map, .. } => Value::Compound(map),
            Self::Lists { items, .. } => Value::List(List::List(items)),
            Self::Compounds { items, .. } => Value::List(List::Compound(items)),
        }
    }
}

/// What opening a list gives: the whole list, or a frame to read its container elements into.
enum Opened {
    Done(List),
    Frame(ReadFrame),
}

/// Returned where a child does not fit its frame or the stack would not end in its root, which
/// the stack discipline rules out.
const STACK_ORDER: Error = Error::InvalidValue {
    field: "nbt",
    reason: "container stack out of order",
};

/// Takes one from a frame's count of elements left; `false` once none are left.
fn take_one(left: &mut usize) -> bool {
    let more = *left > 0;
    *left = left.saturating_sub(1);
    more
}

struct Decoder<'r, 'a> {
    r: &'r mut Reader<'a>,
    encoding: Encoding,
    start: usize,
    /// The most bytes the root may take.
    limit: usize,
}

impl Decoder<'_, '_> {
    fn check_root_size(&self) -> Result<()> {
        let used = self.start.saturating_sub(self.r.remaining().len());
        if used > self.limit {
            return Err(Error::Size);
        }
        Ok(())
    }

    fn int32(&mut self) -> Result<i32> {
        match self.encoding {
            Encoding::NetworkLittleEndian => self.r.varint32(),
            Encoding::LittleEndian => self.r.int32(),
        }
    }

    fn int64(&mut self) -> Result<i64> {
        match self.encoding {
            Encoding::NetworkLittleEndian => self.r.varint64(),
            Encoding::LittleEndian => self.r.int64(),
        }
    }

    /// An array or list length: an int32 that may not be negative.
    fn length(&mut self) -> Result<u64> {
        u64::try_from(self.int32()?).map_err(|_| Error::Size)
    }

    fn string(&mut self) -> Result<String> {
        let len = match self.encoding {
            Encoding::NetworkLittleEndian => u64::from(self.r.varuint32()?),
            Encoding::LittleEndian => u64::try_from(self.r.int16()?).map_err(|_| Error::Size)?,
        };
        if len > u64::from(MAX_STRING_LEN) {
            return Err(Error::Size);
        }
        self.r.utf8(len)
    }

    /// Reads `len` elements with `f` after the limit check.
    fn items<T>(&mut self, len: u64, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let len = self.r.limit(len, usize::MAX, size_of::<T>())?;
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            items.push(f(self)?);
        }
        Ok(items)
    }

    fn byte_array(&mut self) -> Result<Vec<u8>> {
        let len = self.length()?;
        self.r.byte_vec(len)
    }

    fn int_array(&mut self) -> Result<Vec<i32>> {
        let len = self.length()?;
        self.items(len, Self::int32)
    }

    fn long_array(&mut self) -> Result<Vec<i64>> {
        let len = self.length()?;
        self.items(len, Self::int64)
    }

    fn tag_type(&mut self) -> Result<u8> {
        let tag = self.r.uint8()?;
        if tag > TAG_LONG_ARRAY {
            return Err(Error::UnknownEnum {
                name: "nbt tag",
                value: tag.into(),
            });
        }
        Ok(tag)
    }

    /// Reads the root tag header and returns its type; the name is dropped.
    fn root(&mut self) -> Result<u8> {
        let tag = self.tag_type()?;
        if tag != TAG_END {
            self.string()?;
        }
        Ok(tag)
    }

    /// Reads the payload of a tag that is not a container.
    fn scalar(&mut self, tag: u8) -> Result<Value> {
        Ok(match tag {
            TAG_BYTE => Value::Byte(self.r.uint8()?),
            TAG_SHORT => Value::Short(self.r.int16()?),
            TAG_INT => Value::Int(self.int32()?),
            TAG_LONG => Value::Long(self.int64()?),
            TAG_FLOAT => Value::Float(self.r.float32()?),
            TAG_DOUBLE => Value::Double(self.r.float64()?),
            TAG_BYTE_ARRAY => Value::ByteArray(self.byte_array()?),
            TAG_STRING => Value::String(self.string()?),
            TAG_INT_ARRAY => Value::IntArray(self.int_array()?),
            TAG_LONG_ARRAY => Value::LongArray(self.long_array()?),
            _ => {
                return Err(Error::InvalidValue {
                    field: "nbt tag",
                    reason: "TAG_End has no payload",
                });
            }
        })
    }

    /// Reads a list header, then the whole list unless its elements are containers.
    fn open_list(&mut self) -> Result<Opened> {
        let tag = self.tag_type()?;
        let len = self.length()?;
        Ok(Opened::Done(match tag {
            TAG_END if len == 0 => List::End,
            TAG_END => {
                return Err(Error::InvalidValue {
                    field: "nbt list",
                    reason: "TAG_End elements",
                });
            }
            TAG_BYTE => List::Byte(self.r.byte_vec(len)?),
            TAG_SHORT => List::Short(self.items(len, |d| d.r.int16())?),
            TAG_INT => List::Int(self.items(len, Self::int32)?),
            TAG_LONG => List::Long(self.items(len, Self::int64)?),
            TAG_FLOAT => List::Float(self.items(len, |d| d.r.float32())?),
            TAG_DOUBLE => List::Double(self.items(len, |d| d.r.float64())?),
            TAG_BYTE_ARRAY => List::ByteArray(self.items(len, Self::byte_array)?),
            TAG_STRING => List::String(self.items(len, Self::string)?),
            TAG_INT_ARRAY => List::IntArray(self.items(len, Self::int_array)?),
            TAG_LONG_ARRAY => List::LongArray(self.items(len, Self::long_array)?),
            TAG_LIST => {
                let left = self.r.limit(len, usize::MAX, size_of::<List>())?;
                let items = Vec::with_capacity(left);
                return Ok(Opened::Frame(ReadFrame::Lists { left, items }));
            }
            _ => {
                let left = self.r.limit(len, usize::MAX, size_of::<Compound>())?;
                let items = Vec::with_capacity(left);
                return Ok(Opened::Frame(ReadFrame::Compounds { left, items }));
            }
        }))
    }

    /// Reads the payload of the root compound and everything in it.
    fn compound(&mut self) -> Result<Compound> {
        let mut stack = Vec::with_capacity(16);
        stack.push(ReadFrame::compound());
        while let Some(top) = stack.last_mut() {
            self.check_root_size()?;
            let next = match top {
                ReadFrame::Compound { name, .. } => {
                    let tag = self.tag_type()?;
                    if tag != TAG_END {
                        *name = self.string()?;
                        self.r.limit(1, usize::MAX, size_of::<(String, Value)>())?;
                    }
                    (tag != TAG_END).then_some(tag)
                }
                ReadFrame::Lists { left, .. } => take_one(left).then_some(TAG_LIST),
                ReadFrame::Compounds { left, .. } => take_one(left).then_some(TAG_COMPOUND),
            };
            let value = match next {
                Some(tag @ (TAG_LIST | TAG_COMPOUND)) => {
                    if stack.len() >= MAX_DEPTH {
                        return Err(Error::Size);
                    }
                    let opened = if tag == TAG_LIST {
                        self.open_list()?
                    } else {
                        Opened::Frame(ReadFrame::compound())
                    };
                    match opened {
                        Opened::Done(list) => Value::List(list),
                        Opened::Frame(frame) => {
                            stack.push(frame);
                            continue;
                        }
                    }
                }
                Some(tag) => self.scalar(tag)?,
                None => {
                    let Some(done) = stack.pop() else { break };
                    match done.finish() {
                        Value::Compound(map) if stack.is_empty() => return Ok(map),
                        value => value,
                    }
                }
            };
            stack.last_mut().ok_or(STACK_ORDER)?.add(value)?;
        }
        Err(STACK_ORDER)
    }
}

/// What an encoded root takes: its bytes, and what decoding them takes from a reader's
/// allocation budget ([`Reader::limit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cost {
    /// The bytes written.
    pub(crate) bytes: usize,
    /// The budget [`Reader::nbt`] takes to read them back.
    pub(crate) alloc: usize,
}

/// A container being written; the iterative counterpart of [`ReadFrame`].
enum WriteFrame<'v> {
    Compound(btree_map::Iter<'v, String, Value>),
    Lists(slice::Iter<'v, List>),
    Compounds(slice::Iter<'v, Compound>),
}

/// The next thing a write frame gives.
enum Next<'v> {
    Scalar(&'v Value),
    List(&'v List),
    Compound(&'v Compound),
    End,
}

struct Encoder<'w> {
    w: &'w mut Writer,
    encoding: Encoding,
    /// The buffer length writing must not pass: the root's start plus its byte limit.
    end: usize,
    /// What decoding what is written takes from a reader's budget: the [`Decoder`]'s charges
    /// ([`Reader::limit`]), taken here at the same places and with the same element types.
    alloc: usize,
}

impl Encoder<'_> {
    /// Fails once the buffer passed [`Self::end`]: checked after each scalar (at most 10 bytes
    /// over) and before each byte run, so the buffer never grows far past the limit.
    fn check(&self) -> Result<()> {
        if self.w.as_bytes().len() > self.end {
            return Err(Error::Size);
        }
        Ok(())
    }

    /// Counts `count` elements of `size` bytes, as the decoder's limit check takes them.
    fn charge(&mut self, count: usize, size: usize) {
        self.alloc = self.alloc.saturating_add(count.saturating_mul(size));
    }

    /// Fails if `n` more bytes would pass [`Self::end`]; checked before a byte run is copied.
    fn room(&self, n: usize) -> Result<()> {
        if self.w.as_bytes().len().saturating_add(n) > self.end {
            return Err(Error::Size);
        }
        Ok(())
    }

    fn int32(&mut self, x: i32) {
        match self.encoding {
            Encoding::NetworkLittleEndian => self.w.varint32(x),
            Encoding::LittleEndian => self.w.int32(x),
        }
    }

    fn int64(&mut self, x: i64) {
        match self.encoding {
            Encoding::NetworkLittleEndian => self.w.varint64(x),
            Encoding::LittleEndian => self.w.int64(x),
        }
    }

    fn length(&mut self, len: usize) -> Result<()> {
        self.int32(crate::protocol::length(len)?);
        Ok(())
    }

    fn string(&mut self, x: &str) -> Result<()> {
        if x.len() > usize::from(MAX_STRING_LEN) {
            return Err(Error::Size);
        }
        self.room(x.len())?;
        self.charge(x.len(), 1);
        match self.encoding {
            Encoding::NetworkLittleEndian => self.w.string(x),
            Encoding::LittleEndian => self.w.string_utf(x),
        }
    }

    fn byte_array(&mut self, x: &[u8]) -> Result<()> {
        self.length(x.len())?;
        self.room(x.len())?;
        self.charge(x.len(), 1);
        self.w.bytes(x);
        Ok(())
    }

    fn int_array(&mut self, x: &[i32]) -> Result<()> {
        self.length(x.len())?;
        self.charge(x.len(), size_of::<i32>());
        x.iter().try_for_each(|v| {
            self.int32(*v);
            self.check()
        })
    }

    fn long_array(&mut self, x: &[i64]) -> Result<()> {
        self.length(x.len())?;
        self.charge(x.len(), size_of::<i64>());
        x.iter().try_for_each(|v| {
            self.int64(*v);
            self.check()
        })
    }

    /// Writes the payload of a value that is not a container.
    fn scalar(&mut self, value: &Value) -> Result<()> {
        match value {
            Value::Byte(x) => self.w.uint8(*x),
            Value::Short(x) => self.w.int16(*x),
            Value::Int(x) => self.int32(*x),
            Value::Long(x) => self.int64(*x),
            Value::Float(x) => self.w.float32(*x),
            Value::Double(x) => self.w.float64(*x),
            Value::ByteArray(x) => self.byte_array(x)?,
            Value::String(x) => self.string(x)?,
            Value::IntArray(x) => self.int_array(x)?,
            Value::LongArray(x) => self.long_array(x)?,
            Value::List(_) | Value::Compound(_) => {}
        }
        Ok(())
    }

    /// Writes a list header, then the whole list unless its elements are containers; for those
    /// it returns the frame that writes them.
    fn list<'v>(&mut self, list: &'v List) -> Result<Option<WriteFrame<'v>>> {
        self.w.uint8(list.tag());
        self.length(list.len())?;
        let element = match list {
            List::End => 0,
            List::Byte(_) => size_of::<u8>(),
            List::Short(_) => size_of::<i16>(),
            List::Int(_) => size_of::<i32>(),
            List::Long(_) => size_of::<i64>(),
            List::Float(_) => size_of::<f32>(),
            List::Double(_) => size_of::<f64>(),
            List::ByteArray(_) => size_of::<Vec<u8>>(),
            List::String(_) => size_of::<String>(),
            List::List(_) => size_of::<List>(),
            List::Compound(_) => size_of::<Compound>(),
            List::IntArray(_) => size_of::<Vec<i32>>(),
            List::LongArray(_) => size_of::<Vec<i64>>(),
        };
        self.charge(list.len(), element);
        match list {
            List::End => {}
            List::Byte(x) => {
                self.room(x.len())?;
                self.w.bytes(x);
            }
            List::Short(x) => x.iter().try_for_each(|v| {
                self.w.int16(*v);
                self.check()
            })?,
            List::Int(x) => x.iter().try_for_each(|v| {
                self.int32(*v);
                self.check()
            })?,
            List::Long(x) => x.iter().try_for_each(|v| {
                self.int64(*v);
                self.check()
            })?,
            List::Float(x) => x.iter().try_for_each(|v| {
                self.w.float32(*v);
                self.check()
            })?,
            List::Double(x) => x.iter().try_for_each(|v| {
                self.w.float64(*v);
                self.check()
            })?,
            List::ByteArray(x) => x.iter().try_for_each(|v| self.byte_array(v))?,
            List::String(x) => x.iter().try_for_each(|v| self.string(v))?,
            List::IntArray(x) => x.iter().try_for_each(|v| self.int_array(v))?,
            List::LongArray(x) => x.iter().try_for_each(|v| self.long_array(v))?,
            List::List(x) => return Ok(Some(WriteFrame::Lists(x.iter()))),
            List::Compound(x) => return Ok(Some(WriteFrame::Compounds(x.iter()))),
        }
        Ok(None)
    }

    /// Writes the payload of the root compound `root` and everything in it.
    fn compound(&mut self, root: &Compound) -> Result<()> {
        let mut stack = Vec::with_capacity(16);
        stack.push(WriteFrame::Compound(root.iter()));
        while let Some(top) = stack.last_mut() {
            self.check()?;
            let next = match top {
                WriteFrame::Compound(entries) => {
                    if let Some((name, value)) = entries.next() {
                        self.w.uint8(value.tag());
                        self.string(name)?;
                        self.charge(1, size_of::<(String, Value)>());
                        match value {
                            Value::List(list) => Next::List(list),
                            Value::Compound(map) => Next::Compound(map),
                            value => Next::Scalar(value),
                        }
                    } else {
                        self.w.uint8(TAG_END);
                        Next::End
                    }
                }
                WriteFrame::Lists(items) => items.next().map_or(Next::End, Next::List),
                WriteFrame::Compounds(items) => items.next().map_or(Next::End, Next::Compound),
            };
            match next {
                Next::Scalar(value) => self.scalar(value)?,
                Next::End => {
                    stack.pop();
                }
                Next::List(_) | Next::Compound(_) if stack.len() >= MAX_DEPTH => {
                    return Err(Error::Size);
                }
                Next::List(list) => {
                    if let Some(frame) = self.list(list)? {
                        stack.push(frame);
                    }
                }
                Next::Compound(map) => stack.push(WriteFrame::Compound(map.iter())),
            }
        }
        Ok(())
    }
}

impl Reader<'_> {
    /// `NBT`: a root compound. A lone `TAG_End` reads as an empty compound (gophertunnel
    /// `AllowZero`).
    pub(crate) fn nbt(&mut self, encoding: Encoding) -> Result<Compound> {
        self.nbt_within(encoding, MAX_ROOT_BYTES)
    }

    /// [`Self::nbt`] for a root of at most `limit` bytes, a store's own limit.
    pub(crate) fn nbt_within(&mut self, encoding: Encoding, limit: usize) -> Result<Compound> {
        let start = self.remaining().len();
        let mut d = Decoder {
            r: self,
            encoding,
            start,
            limit,
        };
        let map = match d.root()? {
            TAG_END => Compound::new(),
            TAG_COMPOUND => d.compound()?,
            _ => {
                return Err(Error::InvalidValue {
                    field: "nbt root",
                    reason: "not a compound",
                });
            }
        };
        d.check_root_size()?;
        Ok(map)
    }
}

impl Writer {
    /// `NBT`: a root compound with an empty name, at most [`MAX_ROOT_BYTES`].
    pub(crate) fn nbt(&mut self, x: &Compound, encoding: Encoding) -> Result<()> {
        self.nbt_within(x, encoding, MAX_ROOT_BYTES).map(drop)
    }

    /// [`Self::nbt`] for a root of at most `limit` bytes, a store's own limit (one over
    /// [`MAX_ROOT_BYTES`] reads back only through [`Reader::nbt_within`]): a larger one is a size
    /// error, found as writing passes the limit, so the buffer never holds much more than `limit`
    /// bytes of it whatever `x` holds. On an error the buffer holds part of the root. Returns what
    /// the root takes.
    pub(crate) fn nbt_within(
        &mut self,
        x: &Compound,
        encoding: Encoding,
        limit: usize,
    ) -> Result<Cost> {
        let start = self.as_bytes().len();
        self.uint8(TAG_COMPOUND);
        let end = start.saturating_add(limit);
        let mut e = Encoder {
            w: self,
            encoding,
            end,
            alloc: 0,
        };
        e.string("")?;
        e.compound(x)?;
        let alloc = e.alloc;
        let bytes = self.as_bytes().len().saturating_sub(start);
        if bytes > limit {
            return Err(Error::Size);
        }
        Ok(Cost { bytes, alloc })
    }
}

/// A compound in a generated static table (PLAN.md 10.4): entries in key byte order.
pub(crate) type StaticCompound = &'static [(&'static str, Static)];

/// An NBT value in a generated static table. Tables cannot hold [`Value`] (it owns heap data),
/// so they hold this, and the engine turns it into a [`Value`] once at start. It has the types
/// the generated data uses; the generator fails on any other. The nesting is that of the
/// generated data, so the recursive functions stay shallow.
#[derive(Debug)]
pub(crate) enum Static {
    Byte(u8),
    Short(i16),
    Int(i32),
    Float(f32),
    ByteArray(&'static [u8]),
    String(&'static str),
    List(StaticList),
    Compound(StaticCompound),
}

/// A [`List`] in a generated static table, of the element types the generated data uses.
#[derive(Debug)]
pub(crate) enum StaticList {
    End,
    Int(&'static [i32]),
    Float(&'static [f32]),
    String(&'static [&'static str]),
    Compound(&'static [StaticCompound]),
}

/// Builds the [`Compound`] of a static compound.
pub(crate) fn compound(entries: StaticCompound) -> Compound {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.to_value()))
        .collect()
}

/// Whether every entry of `part` is in `whole` with an equal value (floats compare by bits).
pub(crate) fn contains(whole: &Compound, part: StaticCompound) -> bool {
    part.iter()
        .all(|(k, v)| whole.get(*k).is_some_and(|w| v.equals(w)))
}

impl Static {
    /// Builds the [`Value`].
    pub(crate) fn to_value(&self) -> Value {
        match self {
            Self::Byte(x) => Value::Byte(*x),
            Self::Short(x) => Value::Short(*x),
            Self::Int(x) => Value::Int(*x),
            Self::Float(x) => Value::Float(*x),
            Self::ByteArray(x) => Value::ByteArray(x.to_vec()),
            Self::String(x) => Value::String((*x).to_owned()),
            Self::List(x) => Value::List(x.to_list()),
            Self::Compound(x) => Value::Compound(compound(x)),
        }
    }

    /// Whether `value` equals this value; floats compare by bits.
    fn equals(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Byte(a), Value::Byte(b)) => a == b,
            (Self::Short(a), Value::Short(b)) => a == b,
            (Self::Int(a), Value::Int(b)) => a == b,
            (Self::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::ByteArray(a), Value::ByteArray(b)) => *a == b.as_slice(),
            (Self::String(a), Value::String(b)) => a == b,
            (Self::List(a), Value::List(b)) => a.equals(b),
            (Self::Compound(a), Value::Compound(b)) => a.len() == b.len() && contains(b, a),
            _ => false,
        }
    }
}

impl StaticList {
    fn to_list(&self) -> List {
        match self {
            Self::End => List::End,
            Self::Int(x) => List::Int(x.to_vec()),
            Self::Float(x) => List::Float(x.to_vec()),
            Self::String(x) => List::String(x.iter().map(|s| (*s).to_owned()).collect()),
            Self::Compound(x) => List::Compound(x.iter().map(|c| compound(c)).collect()),
        }
    }

    fn equals(&self, list: &List) -> bool {
        fn all<A, B>(a: &[A], b: &[B], eq: impl Fn(&A, &B) -> bool) -> bool {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| eq(x, y))
        }
        match (self, list) {
            (Self::End, List::End) => true,
            (Self::Int(a), List::Int(b)) => *a == b.as_slice(),
            (Self::Float(a), List::Float(b)) => all(a, b, |x, y| x.to_bits() == y.to_bits()),
            (Self::String(a), List::String(b)) => all(a, b, |x, y| x == y),
            (Self::Compound(a), List::Compound(b)) => {
                all(a, b, |x, y| x.len() == y.len() && contains(y, x))
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fuzz;

    const BUDGET: usize = 1 << 26;

    fn read(data: &[u8], encoding: Encoding) -> Result<Compound> {
        let mut r = Reader::new(data, BUDGET, 0);
        let map = r.nbt(encoding)?;
        if !r.remaining().is_empty() {
            return Err(Error::InvalidValue {
                field: "test input",
                reason: "bytes left after the root",
            });
        }
        Ok(map)
    }

    fn write(map: &Compound, encoding: Encoding) -> Result<Vec<u8>> {
        let mut w = Writer::new(Vec::new(), 0);
        w.nbt(map, encoding)?;
        Ok(w.into_inner())
    }

    fn sample() -> Compound {
        Compound::from([
            ("a".to_owned(), Value::Byte(1)),
            ("b".to_owned(), Value::List(List::Int(vec![1, -1]))),
            ("c".to_owned(), Value::String("hi".to_owned())),
        ])
    }

    fn every_tag() -> Compound {
        let inner = Compound::from([("k".to_owned(), Value::Short(-3))]);
        let lists = [
            List::End,
            List::Byte(vec![4, 5]),
            List::Short(vec![-2, 9]),
            List::Int(Vec::new()),
            List::Long(vec![i64::MIN]),
            List::Float(vec![0.5]),
            List::Double(vec![-1.5, 2.0]),
            List::ByteArray(vec![vec![1], Vec::new()]),
            List::String(vec!["x".to_owned(), String::new()]),
            List::List(vec![List::String(Vec::new()), List::Int(vec![3])]),
            List::Compound(vec![inner.clone(), Compound::new()]),
            List::IntArray(vec![vec![-1, 0]]),
            List::LongArray(vec![vec![7], Vec::new()]),
        ];
        let mut map = Compound::from([
            ("byte".to_owned(), Value::Byte(0xff)),
            ("short".to_owned(), Value::Short(i16::MIN)),
            ("int".to_owned(), Value::Int(i32::MIN)),
            ("long".to_owned(), Value::Long(i64::MAX)),
            ("float".to_owned(), Value::Float(1.25)),
            ("double".to_owned(), Value::Double(-0.5)),
            ("bytes".to_owned(), Value::ByteArray(vec![1, 2, 3])),
            ("string".to_owned(), Value::String("é".to_owned())),
            ("compound".to_owned(), Value::Compound(inner)),
            ("ints".to_owned(), Value::IntArray(vec![-1, 0, i32::MAX])),
            ("longs".to_owned(), Value::LongArray(vec![i64::MIN, 7])),
        ]);
        for (i, list) in lists.into_iter().enumerate() {
            map.insert(format!("list{i}"), Value::List(list));
        }
        map
    }

    #[test]
    fn network_little_endian_matches_gophertunnel_bytes() {
        let bytes = write(&sample(), Encoding::NetworkLittleEndian).unwrap();
        let expected = [
            0x0a, 0x00, // root compound, empty name
            0x01, 0x01, b'a', 0x01, // byte
            0x09, 0x01, b'b', 0x03, 0x04, 0x02, 0x01, // list of 2 varint ints
            0x08, 0x01, b'c', 0x02, b'h', b'i', // string
            0x00,
        ];
        assert_eq!(bytes, expected);
        assert_eq!(read(&expected, Encoding::NetworkLittleEndian), Ok(sample()));
    }

    #[test]
    fn little_endian_matches_gophertunnel_bytes() {
        let bytes = write(&sample(), Encoding::LittleEndian).unwrap();
        let expected = [
            0x0a, 0x00, 0x00, // root compound, empty name
            0x01, 0x01, 0x00, b'a', 0x01, // byte
            0x09, 0x01, 0x00, b'b', 0x03, 0x02, 0x00, 0x00, 0x00, // list header
            0x01, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, // ints
            0x08, 0x01, 0x00, b'c', 0x02, 0x00, b'h', b'i', // string
            0x00,
        ];
        assert_eq!(bytes, expected);
        assert_eq!(read(&expected, Encoding::LittleEndian), Ok(sample()));
    }

    #[test]
    fn every_tag_round_trips_in_both_encodings() {
        for encoding in [Encoding::NetworkLittleEndian, Encoding::LittleEndian] {
            let bytes = write(&every_tag(), encoding).unwrap();
            assert_eq!(read(&bytes, encoding), Ok(every_tag()));
            assert_eq!(write(&read(&bytes, encoding).unwrap(), encoding), Ok(bytes));
        }
    }

    /// What writing a root counts is exactly what reading it back takes from the budget and the
    /// bytes it wrote, for every tag and random compounds, in both encodings; a root over its
    /// own limit reads back only through a reader with that limit.
    #[test]
    fn a_written_root_counts_the_bytes_and_budget_its_reading_takes() {
        let mut rng = crate::protocol::Rng(91);
        let mut roots = vec![every_tag(), Compound::new()];
        roots.extend((0..200).map(|_| crate::storage::codec::tests::random_compound(&mut rng, 3)));
        for encoding in [Encoding::NetworkLittleEndian, Encoding::LittleEndian] {
            for root in &roots {
                let mut w = Writer::new(Vec::new(), 0);
                let cost = w.nbt_within(root, encoding, MAX_ROOT_BYTES).unwrap();
                assert_eq!(cost.bytes, w.as_bytes().len());
                let mut r = Reader::new(w.as_bytes(), BUDGET, 0);
                assert_eq!(r.nbt(encoding).as_ref(), Ok(root));
                assert_eq!(BUDGET - r.budget(), cost.alloc, "{root:?}");
            }
        }
        let big = Compound::from([("a".to_owned(), Value::ByteArray(vec![0; MAX_ROOT_BYTES]))]);
        let mut w = Writer::new(Vec::new(), 0);
        let cost = w
            .nbt_within(&big, Encoding::LittleEndian, 2 * MAX_ROOT_BYTES)
            .unwrap();
        assert!(cost.bytes > MAX_ROOT_BYTES);
        let budget = 4 * MAX_ROOT_BYTES;
        let mut r = Reader::new(w.as_bytes(), budget, 0);
        assert_eq!(r.nbt(Encoding::LittleEndian), Err(Error::Size));
        let mut r = Reader::new(w.as_bytes(), budget, 0);
        assert_eq!(
            r.nbt_within(Encoding::LittleEndian, cost.bytes),
            Ok(big.clone())
        );
        let mut r = Reader::new(w.as_bytes(), budget, 0);
        assert_eq!(
            r.nbt_within(Encoding::LittleEndian, cost.bytes - 1),
            Err(Error::Size)
        );
    }

    #[test]
    fn a_lone_end_tag_reads_as_an_empty_compound() {
        assert_eq!(
            read(&[0x00], Encoding::NetworkLittleEndian),
            Ok(Compound::new())
        );
        assert_eq!(
            write(&Compound::new(), Encoding::NetworkLittleEndian),
            Ok(vec![0x0a, 0x00, 0x00])
        );
    }

    #[test]
    fn empty_lists_keep_their_element_type() {
        for tag in TAG_END..=TAG_LONG_ARRAY {
            let data = [0x0a, 0x00, 0x09, 0x01, b'l', tag, 0x00, 0x00];
            let map = read(&data, Encoding::NetworkLittleEndian).unwrap();
            let Some(Value::List(list)) = map.get("l") else {
                panic!("tag {tag} did not read as a list");
            };
            assert!(list.is_empty());
            assert_eq!(list.tag(), tag);
            assert_eq!(
                write(&map, Encoding::NetworkLittleEndian),
                Ok(data.to_vec())
            );
        }
        let end_elements = [0x0a, 0x00, 0x09, 0x01, b'l', 0x00, 0x02, 0x00];
        assert!(read(&end_elements, Encoding::NetworkLittleEndian).is_err());
    }

    #[test]
    fn repeated_keys_keep_the_last_value() {
        let data = [
            0x0a, 0x00, 0x01, 0x01, b'a', 0x01, 0x01, 0x01, b'a', 0x02, 0x00,
        ];
        let map = read(&data, Encoding::NetworkLittleEndian).unwrap();
        assert_eq!(map, Compound::from([("a".to_owned(), Value::Byte(2))]));
    }

    #[test]
    fn keys_are_written_in_byte_order() {
        let map = Compound::from([
            ("b".to_owned(), Value::Byte(0)),
            ("B".to_owned(), Value::Byte(0)),
            ("a".to_owned(), Value::Byte(0)),
        ]);
        let bytes = write(&map, Encoding::NetworkLittleEndian).unwrap();
        assert_eq!([bytes[4], bytes[8], bytes[12]], [b'B', b'a', b'b']);
    }

    fn nested_compounds(depth: usize) -> Vec<u8> {
        let mut data = [0x0a, 0x00].repeat(depth);
        data.extend(vec![0x00; depth]);
        data
    }

    #[test]
    fn nesting_deeper_than_512_is_a_size_error() {
        assert!(read(&nested_compounds(512), Encoding::NetworkLittleEndian).is_ok());
        assert_eq!(
            read(&nested_compounds(513), Encoding::NetworkLittleEndian),
            Err(Error::Size)
        );
        let mut deep = Compound::new();
        for _ in 0..512 {
            deep = Compound::from([(String::new(), Value::Compound(deep))]);
        }
        assert_eq!(
            write(&deep, Encoding::NetworkLittleEndian),
            Err(Error::Size)
        );
    }

    #[test]
    fn a_bounded_write_stops_at_its_limit_before_copying_more() {
        const LIMIT: usize = 4096;
        let huge = [
            Value::ByteArray(vec![0; 16 << 20]),
            Value::IntArray(vec![-1; 1 << 20]),
            Value::LongArray(vec![-1; 1 << 20]),
            Value::List(List::Int(vec![-1; 1 << 20])),
            Value::List(List::Byte(vec![0; 16 << 20])),
            Value::List(List::Compound(vec![Compound::new(); 1 << 20])),
        ];
        for encoding in [Encoding::LittleEndian, Encoding::NetworkLittleEndian] {
            for value in &huge {
                let map = Compound::from([("v".to_owned(), value.clone())]);
                let mut w = Writer::new(Vec::new(), 0);
                assert_eq!(w.nbt_within(&map, encoding, LIMIT), Err(Error::Size));
                // At most one scalar past the limit.
                assert!(w.as_bytes().len() <= LIMIT + 10, "{}", w.as_bytes().len());
            }
            // A root of exactly the limit fits; one more byte does not.
            let fit =
                |len: usize| Compound::from([("v".to_owned(), Value::ByteArray(vec![0; len]))]);
            // The array length's varint takes two bytes from 128 to 16,383.
            let overhead = write(&fit(LIMIT / 2), encoding).unwrap().len() - LIMIT / 2;
            let mut w = Writer::new(Vec::new(), 0);
            assert_eq!(
                w.nbt_within(&fit(LIMIT - overhead), encoding, LIMIT)
                    .map(|c| c.bytes),
                Ok(LIMIT)
            );
            assert_eq!(w.as_bytes().len(), LIMIT);
            let mut w = Writer::new(Vec::new(), 0);
            assert_eq!(
                w.nbt_within(&fit(LIMIT - overhead + 1), encoding, LIMIT),
                Err(Error::Size)
            );
        }
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "safety: a thread with a small stack proves the recursion's stack bound"
    )]
    fn the_deepest_nesting_fits_in_half_a_default_stack() {
        // Reading and writing use a heap frame stack; dropping the value still recurses.
        let run = || {
            let data = nested_compounds(512);
            let map = read(&data, Encoding::NetworkLittleEndian).unwrap();
            assert_eq!(write(&map, Encoding::NetworkLittleEndian), Ok(data));
        };
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(run)
            .unwrap()
            .join()
            .unwrap();
    }

    /// A root compound holding list `l` with `lists` nested lists of one list, ending in an
    /// empty `TAG_End` list: `lists + 2` containers deep.
    fn nested_lists(lists: usize) -> Vec<u8> {
        let mut data = vec![0x0a, 0x00, 0x09, 0x01, b'l'];
        for _ in 0..lists {
            data.extend([0x09, 0x02]); // a list of one list
        }
        data.extend([0x00, 0x00, 0x00]);
        data
    }

    #[test]
    fn nested_lists_count_toward_the_depth() {
        let ok = read(&nested_lists(510), Encoding::NetworkLittleEndian).unwrap();
        assert_eq!(
            write(&ok, Encoding::NetworkLittleEndian),
            Ok(nested_lists(510))
        );
        assert_eq!(
            read(&nested_lists(511), Encoding::NetworkLittleEndian),
            Err(Error::Size)
        );
        let mut deep = List::End;
        for _ in 0..511 {
            deep = List::List(vec![deep]);
        }
        let map = Compound::from([("l".to_owned(), Value::List(deep))]);
        assert_eq!(write(&map, Encoding::NetworkLittleEndian), Err(Error::Size));
    }

    #[test]
    fn a_root_over_4_mib_is_a_size_error() {
        let mut w = Writer::new(vec![0x0a, 0x00, 0x07, 0x00], 0);
        w.varint32(i32::try_from(MAX_ROOT_BYTES).unwrap());
        let mut data = w.into_inner();
        data.extend(vec![0; MAX_ROOT_BYTES]);
        data.push(0x00);
        assert_eq!(read(&data, Encoding::NetworkLittleEndian), Err(Error::Size));
        let big = Compound::from([("a".to_owned(), Value::ByteArray(vec![0; MAX_ROOT_BYTES]))]);
        assert_eq!(write(&big, Encoding::NetworkLittleEndian), Err(Error::Size));
    }

    #[test]
    fn declared_lengths_over_the_input_are_size_errors() {
        // A byte array claiming 64 bytes with 1 left.
        let data = [0x0a, 0x00, 0x07, 0x00, 0x80, 0x01, 0x00];
        assert_eq!(read(&data, Encoding::NetworkLittleEndian), Err(Error::Size));
        // A negative list length.
        let data = [0x0a, 0x00, 0x09, 0x00, 0x01, 0x01, 0x00];
        assert_eq!(read(&data, Encoding::NetworkLittleEndian), Err(Error::Size));
        // A list of shorts claiming 64 elements with 1 byte left.
        let data = [0x0a, 0x00, 0x09, 0x00, 0x02, 0x80, 0x01, 0x00];
        assert_eq!(read(&data, Encoding::NetworkLittleEndian), Err(Error::Size));
        // A string longer than 32767 bytes.
        let data = [0x0a, 0x00, 0x08, 0x00, 0x80, 0x80, 0x02, 0x00];
        assert_eq!(read(&data, Encoding::NetworkLittleEndian), Err(Error::Size));
        // A negative little-endian string length.
        let data = [0x0a, 0x00, 0x00, 0x08, 0xff, 0xff];
        assert_eq!(read(&data, Encoding::LittleEndian), Err(Error::Size));
    }

    #[test]
    fn budget_exhaustion_is_a_size_error() {
        let bytes = write(&every_tag(), Encoding::NetworkLittleEndian).unwrap();
        let mut r = Reader::new(&bytes, 64, 0);
        assert_eq!(r.nbt(Encoding::NetworkLittleEndian), Err(Error::Size));
    }

    #[test]
    fn invalid_values_are_rejected() {
        let long = Compound::from([("s".to_owned(), Value::String("a".repeat(0x8000)))]);
        assert_eq!(write(&long, Encoding::LittleEndian), Err(Error::Size));
        let long_in_list = Compound::from([(
            "l".to_owned(),
            Value::List(List::String(vec!["a".repeat(0x8000)])),
        )]);
        assert_eq!(
            write(&long_in_list, Encoding::NetworkLittleEndian),
            Err(Error::Size)
        );
        let bad_utf8 = [0x0a, 0x00, 0x08, 0x00, 0x01, 0xff, 0x00];
        assert_eq!(
            read(&bad_utf8, Encoding::NetworkLittleEndian),
            Err(Error::InvalidUtf8)
        );
        let unknown_tag = [0x0a, 0x00, 0x0d, 0x00, 0x00];
        assert!(read(&unknown_tag, Encoding::NetworkLittleEndian).is_err());
        let unknown_element = [0x0a, 0x00, 0x09, 0x00, 0x0d, 0x00, 0x00];
        assert!(read(&unknown_element, Encoding::NetworkLittleEndian).is_err());
        assert!(read(&[0x01, 0x00, 0x05], Encoding::NetworkLittleEndian).is_err());
    }

    #[test]
    fn mutated_input_never_panics() {
        for encoding in [Encoding::NetworkLittleEndian, Encoding::LittleEndian] {
            let seed = write(&every_tag(), encoding).unwrap();
            let decoded = fuzz(&seed, 4000, |data| {
                let mut r = Reader::new(data, BUDGET, 0);
                let again = write(&r.nbt(encoding)?, encoding)?;
                assert_eq!(write(&read(&again, encoding)?, encoding), Ok(again));
                Ok(())
            });
            assert!(decoded > 0);
        }
    }
}
