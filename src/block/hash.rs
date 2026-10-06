//! The network block hash: the ID a client with `UseBlockNetworkIDHashes`
//! knows a block state by, FNV-1a 32 over the state's little-endian NBT `{name, states}` with
//! the states in name order (dragonfly `networkBlockHash`; BDS sends the same values).
//!
//! The state table carries the reference list's IDs, which the golden test below recomputes;
//! custom states get theirs from here when the server makes them.

use super::PropertyValue;
use crate::nbt::{TAG_BYTE, TAG_COMPOUND, TAG_END, TAG_INT, TAG_STRING};

/// FNV-1a 32 offset basis.
const OFFSET: u32 = 0x811c_9dc5;
/// FNV-1a 32 prime.
const PRIME: u32 = 0x0100_0193;
/// The hash dragonfly gives `minecraft:unknown`.
const UNKNOWN: u32 = 0xffff_fffe;

/// An FNV-1a 32 state fed byte by byte, so hashing never allocates.
struct Fnv(u32);

impl Fnv {
    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u32::from(b)).wrapping_mul(PRIME);
        }
    }

    /// An NBT string: its length as a little-endian `u16`, then its bytes. Names and values are
    /// short (block identifiers and property values are checked when they are registered); a
    /// longer one is cut at the length the prefix can hold, as no client could send it.
    fn string(&mut self, s: &str) {
        let len = u16::try_from(s.len()).unwrap_or(u16::MAX);
        self.bytes(&len.to_le_bytes());
        self.bytes(s.as_bytes().get(..usize::from(len)).unwrap_or_default());
    }
}

/// The network hash of the state `name` with `properties`, which must come in name order (as
/// [`State::properties`](super::State::properties) gives them).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::State;

    const REFERENCE: &str = include_str!("../../xtask/data/reference/blocks.json");

    /// The typed property value of the reference list's JSON (`{"byte": 1}`).
    fn value(json: &serde_json::Value) -> (u8, i64, String) {
        let (kind, v) = json.as_object().unwrap().iter().next().unwrap();
        match kind.as_str() {
            "byte" => (TAG_BYTE, v.as_i64().unwrap(), String::new()),
            "int" => (TAG_INT, v.as_i64().unwrap(), String::new()),
            "string" => (TAG_STRING, 0, v.as_str().unwrap().to_owned()),
            other => panic!("type {other}"),
        }
    }

    /// Every state of the 1.26.50 reference list hashes to its `CloudburstMC` network ID, and every
    /// state of the table to the ID the table holds.
    #[test]
    fn hashes_equal_every_reference_network_id() {
        let reference: Vec<serde_json::Value> = serde_json::from_str(REFERENCE).unwrap();
        assert_eq!(reference.len(), 22_091);
        for b in &reference {
            let name = b["name"].as_str().unwrap();
            let mut states: Vec<(String, (u8, i64, String))> = b["states"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), value(v)))
                .collect();
            states.sort_by(|a, b| a.0.cmp(&b.0));
            let properties = states.iter().map(|(k, (tag, n, s))| {
                let v = match *tag {
                    TAG_BYTE => PropertyValue::Byte(u8::try_from(*n).unwrap()),
                    TAG_INT => PropertyValue::Int(i32::try_from(*n).unwrap()),
                    _ => PropertyValue::String(s),
                };
                (k.as_str(), v)
            });
            let want = u32::try_from(b["network_id"].as_u64().unwrap()).unwrap();
            assert_eq!(network_hash(name, properties), want, "{name}");
        }
        for s in State::all() {
            let hash = network_hash(s.name(), s.properties().map(|p| (p.name(), p.value())));
            assert_eq!(hash, s.network_id(), "{s:?}");
        }
        assert_eq!(network_hash("minecraft:unknown", []), UNKNOWN);
    }
}
