//! The network block hash: the ID a client with `UseBlockNetworkIDHashes` knows a block state by.
//!
//! It is FNV-1a 32 over the state's little-endian NBT `{name, states}` with the states in name
//! order (the reference list's hash; the official server sends the same values).
//!
//! The state table carries the reference list's IDs, which the golden test of the block module
//! recomputes; custom states get theirs from here when the server makes them.
//!
//! `cargo xtask dump` includes this file by path to hash the reference list's states, so it names
//! nothing outside itself.

/// FNV-1a 32 offset basis.
const OFFSET: u32 = 0x811c_9dc5;
/// FNV-1a 32 prime.
const PRIME: u32 = 0x0100_0193;
/// The NBT tag number of the end of a compound.
const TAG_END: u8 = 0;
/// The NBT tag number of a byte.
const TAG_BYTE: u8 = 1;
/// The NBT tag number of an int.
const TAG_INT: u8 = 3;
/// The NBT tag number of a string.
const TAG_STRING: u8 = 8;
/// The NBT tag number of a compound.
const TAG_COMPOUND: u8 = 10;
/// The hash the reference list gives `minecraft:unknown`.
const UNKNOWN: u32 = 0xffff_fffe;

/// The value of a block state property, typed as the client's block state NBT types it.
///
/// The table's values borrow `'static` strings; a lookup such as
/// `State::find` takes values borrowed from its input. Values order by
/// type (byte, int, string), then by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyValue<'a> {
    /// A `TAG_Byte`; boolean properties are 0 or 1.
    Byte(u8),
    /// A `TAG_Int`.
    Int(i32),
    /// A `TAG_String`.
    String(&'a str),
}

/// An FNV-1a 32 state fed byte by byte, so hashing never allocates.
struct Fnv(u32);

impl Fnv {
    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u32::from(b)).wrapping_mul(PRIME);
        }
    }

    /// Feeds an NBT string: its length as a little-endian `u16`, then its bytes.
    ///
    /// Names and values are short (block identifiers and property values are checked when they
    /// are registered); a longer one is cut at the length the prefix can hold, as no client could
    /// send it.
    fn string(&mut self, s: &str) {
        let len = u16::try_from(s.len()).unwrap_or(u16::MAX);
        self.bytes(&len.to_le_bytes());
        self.bytes(s.as_bytes().get(..usize::from(len)).unwrap_or_default());
    }
}

/// The network hash of the state `name` with `properties`.
///
/// The properties must come in name order (as `State::properties` gives them).
pub(crate) fn network_hash<'a>(
    name: &str,
    properties: impl IntoIterator<Item = (&'a str, PropertyValue<'a>)>,
) -> u32 {
    if name == "minecraft:unknown" {
        return UNKNOWN;
    }
    let mut h = Fnv(OFFSET);
    h.bytes(&[TAG_COMPOUND, 0, 0, TAG_STRING]);
    h.string("name");
    h.string(name);
    h.bytes(&[TAG_COMPOUND]);
    h.string("states");
    for (key, value) in properties {
        match value {
            PropertyValue::Byte(b) => {
                h.bytes(&[TAG_BYTE]);
                h.string(key);
                h.bytes(&[b]);
            }
            PropertyValue::Int(i) => {
                h.bytes(&[TAG_INT]);
                h.string(key);
                h.bytes(&i.to_le_bytes());
            }
            PropertyValue::String(s) => {
                h.bytes(&[TAG_STRING]);
                h.string(key);
                h.string(s);
            }
        }
    }
    h.bytes(&[TAG_END, TAG_END]);
    h.0
}
