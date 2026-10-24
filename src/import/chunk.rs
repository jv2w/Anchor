//! Bedrock chunk keys and values, decoded into raw data (no block state upgrade).
//!
//! Keys (PMMP `LevelDB::chunkIndex`, dragonfly `mcdb` `index`; checked on BDS 1.26.52 output):
//! chunk x and z as little-endian `i32`, a little-endian `i32` dimension for the Nether (1) and
//! the End (2) only, a tag byte, and for sub-chunks the sub-chunk y as a signed byte. Entities
//! since 1.18.30: `digp` + chunk position holds the chunk's 8-byte actor ids, `actorprefix` + id
//! each actor's NBT; older chunks keep entities under tag 50.
//!
//! Anchor imports blocks and block entities only. The values it reads (PMMP
//! `LevelDB::loadChunk`, BDS 1.26.52 output for the current forms):
//! - Version (44, or 118 in old worlds): one byte, 0 to 42 (PMMP `ChunkVersion`, 42 = 1.21.120;
//!   BDS 1.26.52 still writes 42).
//! - Sub-chunk (47): a version byte, then for 0 and the misnumbered 2 to 7 4,096 block ids and
//!   2,048 bytes of meta nibbles in x-z-y order (optionally 4,096 bytes of light after, as before
//!   1.1); 1 one block layer; 8 a layer count and the layers; 9 a layer count, the sub-chunk y
//!   (which must equal the key's) and the layers. A layer is a header byte (bits per index × 2;
//!   a set low bit means network ids, never on disk), the packed words, a little-endian `i32`
//!   palette length (absent with 0 bits: one entry) and that many little-endian NBT compounds.
//!   Chunk versions 24 to 26 (1.16.220 to 1.16.230 caves and cliffs experiments) store
//!   sub-chunk y + 4 in the key.
//! - Legacy terrain (48, chunk versions 0 to 2): the whole 16 × 128 × 16 column: 32,768 ids,
//!   16,384 bytes of meta nibbles (index x << 11 | z << 7 | y), 32,768 bytes of light, a 256-byte
//!   height map and 1,024 bytes of biome colours (layout checked against PMMP's
//!   `SubChunkConverter::convertSubChunkFromLegacyColumn`); only the blocks are read.
//! - Block entities (49): little-endian NBT compounds back to back.
//! - Legacy block extra data (52, before 1.2.13): an `i32` count, then (`i32` position, `u16`
//!   id | meta << 8) pairs; the position packs x, z, y as 4, 4, 8 bits (7 bits of y before
//!   chunk version 3). PMMP applies it as a second layer to sub-chunks of versions 0 to 7.
//!
//! Every other tag is not read: biomes (`Data3D` 43, `Data2D` 45), old entities (50), pending
//! ticks (51), finalization (54), the 1.18+ metadata hash (63, 64, 65), ...; [`Raw::ignored`]
//! lists those present, and the tags a newer one replaced. Anything malformed in a value that is
//! read is an error naming the chunk and the key, where PMMP would sometimes log and go on: a
//! skipped value loses blocks.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

use crate::import::{DECODE_BUDGET, Error, Result};
use crate::nbt::{Compound, Encoding};
use crate::protocol::{BLOCKS, Reader, WIDTHS, word_count};
use crate::storage::codec::{BlockLayer, MAX_EXTRA_ENTRIES, check_indices};

/// Chunk tags.
pub(crate) const VERSION: u8 = 0x2c;
pub(crate) const SUB_CHUNK: u8 = 0x2f;
const LEGACY_TERRAIN: u8 = 0x30;
pub(crate) const BLOCK_ENTITIES: u8 = 0x31;
const LEGACY_EXTRA: u8 = 0x34;
const OLD_VERSION: u8 = 0x76;
/// The tags whose values are kept for decoding, with their names in Mojang's LevelDB
/// documentation for errors.
const READ_TAGS: [(u8, &str); 5] = [
    (VERSION, "Version"),
    (LEGACY_TERRAIN, "LegacyTerrain"),
    (BLOCK_ENTITIES, "BlockEntity"),
    (LEGACY_EXTRA, "LegacyBlockExtraData"),
    (OLD_VERSION, "LegacyVersion"),
];
/// The name of [`SUB_CHUNK`] keys, as [`READ_TAGS`] names the others.
const SUB_CHUNK_NAME: &str = "SubChunkPrefix";
/// The newest chunk version read (PMMP `ChunkVersion::v1_21_120`).
pub(crate) const MAX_CHUNK_VERSION: u8 = 42;
/// The newest sub-chunk version read.
pub(crate) const MAX_SUB_CHUNK_VERSION: u8 = 9;
/// The sub-chunks imported: Anchor's world height.
const MIN_Y: i32 = crate::world::MIN_Y >> 4;
const MAX_Y: i32 = crate::world::MAX_Y >> 4;
/// How far chunk versions 24 to 26 shift the sub-chunk y in keys.
const CAVES_CLIFFS_OFFSET: i8 = 4;
/// The most value bytes kept for one chunk.
const MAX_CHUNK_BYTES: usize = 64 << 20;
/// Legacy terrain: ids, meta, light, height map, biome colours.
const LEGACY_IDS: usize = 32_768;
const LEGACY_META: usize = 16_384;
const LEGACY_LIGHT: usize = 32_768;
const LEGACY_TERRAIN_LEN: usize = LEGACY_IDS + LEGACY_META + LEGACY_LIGHT + 256 + 1024;
/// A legacy sub-chunk: ids and meta nibbles, and optionally light after.
const LEGACY_SECTION: usize = 4096 + 2048;
const LEGACY_SECTION_LIGHT: usize = LEGACY_SECTION + 4096;

/// What a database key is, for [`super::Catalog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// A key of the overworld chunk (`x`, `z`); `version` for its version key.
    Overworld { x: i32, z: i32, version: bool },
    /// A key of a chunk in `dimension`; `version` for its version key.
    Dimension { dimension: i32, version: bool },
    /// `~local_player` or `player_*`.
    Player,
    /// `actorprefix*`: one entity.
    Entity,
    /// `digp*`: a chunk's list of entities.
    EntityList,
    /// Any other key.
    Other,
}

/// Whether `tag` is a chunk tag (43 to 65 and 118, 119).
fn is_chunk_tag(tag: u8) -> bool {
    matches!(tag, 0x2b..=0x41 | 0x76 | 0x77)
}

fn le_i32(bytes: &[u8], at: usize) -> Option<i32> {
    let chunk = bytes.get(at..)?.first_chunk::<4>()?;
    Some(i32::from_le_bytes(*chunk))
}

/// Classifies a database key.
pub(crate) fn classify(key: &[u8]) -> Key {
    if key.starts_with(b"digp") && matches!(key.len(), 12 | 16) {
        return Key::EntityList;
    }
    if key.starts_with(b"actorprefix") && key.len() == 19 {
        return Key::Entity;
    }
    if key == b"~local_player" || key.starts_with(b"player_") {
        return Key::Player;
    }
    let tag = |at: usize| key.get(at).copied().unwrap_or(0);
    let is_version = |t: u8| t == VERSION || t == OLD_VERSION;
    match (key.len(), le_i32(key, 0), le_i32(key, 4), le_i32(key, 8)) {
        (9, Some(x), Some(z), _) if is_chunk_tag(tag(8)) => Key::Overworld {
            x,
            z,
            version: is_version(tag(8)),
        },
        (10, Some(x), Some(z), _) if tag(8) == SUB_CHUNK => Key::Overworld {
            x,
            z,
            version: false,
        },
        (13, _, _, Some(dimension)) if is_chunk_tag(tag(12)) => Key::Dimension {
            dimension,
            version: is_version(tag(12)),
        },
        (14, _, _, Some(dimension)) if tag(12) == SUB_CHUNK => Key::Dimension {
            dimension,
            version: false,
        },
        _ => Key::Other,
    }
}

/// The first 8 bytes of every key of the overworld chunk (`x`, `z`).
pub(crate) fn prefix(x: i32, z: i32) -> [u8; 8] {
    let [x0, x1, x2, x3] = x.to_le_bytes();
    let [z0, z1, z2, z3] = z.to_le_bytes();
    [x0, x1, x2, x3, z0, z1, z2, z3]
}

/// The smallest key after every key that starts with `prefix`; `None` if there is none.
pub(crate) fn after(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if let Some(next) = last.checked_add(1) {
            end.push(next);
            return Some(end);
        }
    }
    None
}

/// The values of one overworld chunk, as read from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entries {
    /// The chunk's x coordinate.
    pub(crate) x: i32,
    /// The chunk's z coordinate.
    pub(crate) z: i32,
    tags: BTreeMap<u8, Vec<u8>>,
    /// Sub-chunk values by the y in their key, within the range any chunk version imports.
    sub_chunks: BTreeMap<i8, Vec<u8>>,
    ignored: BTreeSet<u8>,
    bytes: usize,
}

impl Entries {
    pub(crate) fn new(x: i32, z: i32) -> Self {
        Self {
            x,
            z,
            tags: BTreeMap::new(),
            sub_chunks: BTreeMap::new(),
            ignored: BTreeSet::new(),
            bytes: 0,
        }
    }

    /// The value bytes kept for the chunk.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Counts `len` more bytes kept for the chunk.
    fn count(&mut self, len: usize) -> Result<()> {
        self.bytes = self.bytes.saturating_add(len);
        if self.bytes > MAX_CHUNK_BYTES {
            return Err(Error::TooLarge {
                what: "chunk data",
                limit: MAX_CHUNK_BYTES as u64,
            });
        }
        Ok(())
    }

    /// Adds the value of `key`, a key starting with this chunk's 8-byte prefix.
    pub(crate) fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        match key.get(8..).unwrap_or_default() {
            &[tag] if READ_TAGS.iter().any(|&(t, _)| t == tag) => {
                self.count(value.len())?;
                self.tags.insert(tag, value.to_vec());
            }
            &[SUB_CHUNK, y] => {
                let y = i8::from_le_bytes([y]);
                if (MIN_Y..=MAX_Y + i32::from(CAVES_CLIFFS_OFFSET)).contains(&i32::from(y)) {
                    self.count(value.len())?;
                    self.sub_chunks.insert(y, value.to_vec());
                } else {
                    self.ignored.insert(SUB_CHUNK);
                }
            }
            // Nether and End keys of the same position: counted by the catalog.
            [_, _, _, _, tag, ..] if is_chunk_tag(*tag) => {}
            [tag, ..] => {
                self.ignored.insert(*tag);
            }
            [] => {}
        }
        Ok(())
    }

    /// Names the key of this chunk ending in `suffix` (a tag, and a sub-chunk's y) in `error`.
    fn at_key(&self, suffix: &[u8], error: Error) -> Error {
        let kind = match suffix.first() {
            Some(&SUB_CHUNK) => SUB_CHUNK_NAME,
            Some(tag) => READ_TAGS
                .iter()
                .find(|(t, _)| t == tag)
                .map_or("unknown", |&(_, name)| name),
            None => "unknown",
        };
        let mut key = String::with_capacity(2 * (8 + suffix.len()));
        for byte in prefix(self.x, self.z).iter().chain(suffix) {
            for digit in [byte >> 4, byte & 0xf] {
                key.extend(char::from_digit(digit.into(), 16));
            }
        }
        Error::Key {
            kind,
            key,
            source: Box::new(error),
        }
    }
}

/// A chunk's data as stored, before block states are upgraded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Raw {
    /// The chunk version.
    pub(crate) version: u8,
    /// The stored sub-chunks in the world height, by ascending y; the others hold only air.
    pub(crate) sections: Vec<Section>,
    /// Block entity compounds.
    pub(crate) block_entities: Vec<Compound>,
    /// Legacy block extra data (tag 52): blocks of a second layer for sub-chunks of versions 0
    /// to 7.
    pub(crate) legacy_blocks: Vec<LegacyBlock>,
    /// Tags present but not imported, ascending.
    pub(crate) ignored: Vec<u8>,
}

/// One stored sub-chunk.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Section {
    /// The sub-chunk y: blocks `16 × y` to `16 × y + 15`.
    pub(crate) y: i8,
    /// The sub-chunk version: 0 to 7 legacy, 1 one layer, 8 and 9 layered.
    pub(crate) version: u8,
    /// The block layers, layer 0 first.
    pub(crate) layers: Vec<Layer>,
}

/// One block layer of a sub-chunk.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Layer {
    /// Indices into a palette of NBT block states as stored (`{name, states, version}`, or
    /// `{name, val}` before 1.13).
    Paletted(BlockLayer),
    /// Legacy numeric ids and meta.
    Legacy(LegacyLayer),
}

/// A layer of legacy numeric blocks; block (x, y, z) is at index `x << 8 | z << 4 | y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyLayer {
    /// The block ids.
    pub(crate) ids: Box<[u8; 4096]>,
    /// The meta values, two per byte: index `i` in byte `i >> 1`, the low nibble for even `i`.
    pub(crate) meta: Box<[u8; 2048]>,
}

impl LegacyLayer {
    /// The id and meta of block (`x`, `y`, `z`), each in 0..16.
    pub(crate) fn get(&self, x: usize, y: usize, z: usize) -> (u8, u8) {
        let i = (x << 8 | z << 4 | y) & 0xfff;
        let id = self.ids.get(i).copied().unwrap_or(0);
        let meta = self.meta.get(i >> 1).copied().unwrap_or(0) >> ((i & 1) * 4) & 0xf;
        (id, meta)
    }
}

/// A block of legacy block extra data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LegacyBlock {
    /// Block x in the chunk, 0 to 15.
    pub(crate) x: u8,
    /// Block y, 0 to 255.
    pub(crate) y: u8,
    /// Block z in the chunk, 0 to 15.
    pub(crate) z: u8,
    /// The legacy id.
    pub(crate) id: u8,
    /// The legacy meta, 0 to 15.
    pub(crate) meta: u8,
}

/// Decodes a chunk's values into raw data. An error names the chunk, and the key when one value
/// is at fault.
pub(crate) fn decode(entries: &Entries) -> Result<Raw> {
    decode_unlocated(entries).map_err(|e| Error::Chunk {
        x: entries.x,
        z: entries.z,
        source: Box::new(e),
    })
}

fn decode_unlocated(entries: &Entries) -> Result<Raw> {
    let mut ignored = entries.ignored.clone();
    let tag = |t: u8| entries.tags.get(&t).map(Vec::as_slice);
    let (version_tag, version_bytes) = match (tag(VERSION), tag(OLD_VERSION)) {
        (Some(version), old) => {
            if old.is_some() {
                ignored.insert(OLD_VERSION);
            }
            (VERSION, version)
        }
        (None, Some(old)) => (OLD_VERSION, old),
        (None, None) => return Err(Error::Malformed("chunk has no version")),
    };
    let version = chunk_version(version_bytes).map_err(|e| entries.at_key(&[version_tag], e))?;
    let mut budget = DECODE_BUDGET;
    let sections = terrain(entries, version, &mut ignored, &mut budget)?;
    let block_entities = match tag(BLOCK_ENTITIES) {
        Some(value) => {
            nbt_list(value, &mut budget).map_err(|e| entries.at_key(&[BLOCK_ENTITIES], e))?
        }
        None => Vec::new(),
    };
    let legacy_blocks = match tag(LEGACY_EXTRA) {
        Some(value) => {
            legacy_extra(value, version).map_err(|e| entries.at_key(&[LEGACY_EXTRA], e))?
        }
        None => Vec::new(),
    };
    Ok(Raw {
        version,
        sections,
        block_entities,
        legacy_blocks,
        ignored: ignored.into_iter().collect(),
    })
}

/// The chunk version a version value holds.
fn chunk_version(value: &[u8]) -> Result<u8> {
    let &[version] = value else {
        return Err(Error::Malformed("chunk version is not one byte"));
    };
    if version > MAX_CHUNK_VERSION {
        return Err(Error::NewerFormat {
            what: "chunk version",
            version: version.into(),
            supported: MAX_CHUNK_VERSION.into(),
        });
    }
    Ok(version)
}

/// The sub-chunks of a chunk of `version`; superseded tags go to `ignored`.
fn terrain(
    entries: &Entries,
    version: u8,
    ignored: &mut BTreeSet<u8>,
    budget: &mut usize,
) -> Result<Vec<Section>> {
    let tag = |t: u8| entries.tags.get(&t).map(Vec::as_slice);
    if version <= 2 {
        if !entries.sub_chunks.is_empty() {
            ignored.insert(SUB_CHUNK);
        }
        let terrain = tag(LEGACY_TERRAIN).ok_or(Error::Malformed("chunk has no legacy terrain"))?;
        return legacy_terrain(terrain).map_err(|e| entries.at_key(&[LEGACY_TERRAIN], e));
    }
    if tag(LEGACY_TERRAIN).is_some() {
        ignored.insert(LEGACY_TERRAIN);
    }
    let offset = if (24..=26).contains(&version) {
        CAVES_CLIFFS_OFFSET
    } else {
        0
    };
    let mut sections = Vec::with_capacity(entries.sub_chunks.len());
    for (&key_y, value) in &entries.sub_chunks {
        let y = key_y - offset;
        if (MIN_Y..=MAX_Y).contains(&i32::from(y)) {
            let section = section(value, key_y, y, budget)
                .map_err(|e| entries.at_key(&[SUB_CHUNK, key_y.to_le_bytes()[0]], e))?;
            sections.push(section);
        } else {
            ignored.insert(SUB_CHUNK);
        }
    }
    Ok(sections)
}

/// Maps a non-NBT read failure to the chunk error.
fn short(_: crate::protocol::Error) -> Error {
    Error::Malformed("value ends early or a count exceeds it")
}

/// Little-endian NBT compounds back to back.
fn nbt_list(value: &[u8], budget: &mut usize) -> Result<Vec<Compound>> {
    let mut r = Reader::new(value, *budget, 0);
    let mut list = Vec::new();
    while !r.remaining().is_empty() {
        if list.len() == MAX_EXTRA_ENTRIES {
            return Err(Error::TooLarge {
                what: "compounds in a chunk value",
                limit: MAX_EXTRA_ENTRIES as u64,
            });
        }
        list.push(r.nbt(Encoding::LittleEndian).map_err(Error::Nbt)?);
    }
    *budget = r.budget();
    Ok(list)
}

/// A sub-chunk value stored under `key_y`, holding sub-chunk `y`.
fn section(value: &[u8], key_y: i8, y: i8, budget: &mut usize) -> Result<Section> {
    let (&version, body) = value
        .split_first()
        .ok_or(Error::Malformed("empty sub-chunk"))?;
    let layers = match version {
        0 | 2..=7 => {
            if body.len() != LEGACY_SECTION && body.len() != LEGACY_SECTION_LIGHT {
                return Err(Error::Malformed("legacy sub-chunk length"));
            }
            vec![Layer::Legacy(legacy_layer(body)?)]
        }
        1 | 8 | 9 => {
            let mut r = Reader::new(body, *budget, 0);
            let count = if version == 1 {
                1
            } else {
                r.uint8().map_err(short)?
            };
            if version == 9 && r.int8().map_err(short)? != key_y {
                return Err(Error::Malformed("sub-chunk y differs from its key"));
            }
            let count = r
                .limit(count.into(), usize::from(u8::MAX), size_of::<Layer>())
                .map_err(short)?;
            let mut layers = Vec::with_capacity(count);
            for _ in 0..count {
                layers.push(Layer::Paletted(block_layer(&mut r)?));
            }
            if !r.remaining().is_empty() {
                return Err(Error::Malformed("sub-chunk continues after its layers"));
            }
            *budget = r.budget();
            layers
        }
        _ => {
            return Err(Error::NewerFormat {
                what: "sub-chunk version",
                version: version.into(),
                supported: MAX_SUB_CHUNK_VERSION.into(),
            });
        }
    };
    Ok(Section { y, version, layers })
}

/// The first 6,144 bytes of a legacy sub-chunk body: ids and meta.
fn legacy_layer(body: &[u8]) -> Result<LegacyLayer> {
    let ids = body
        .get(..4096)
        .and_then(|b| <[u8; 4096]>::try_from(b).ok())
        .ok_or(Error::Malformed("legacy sub-chunk length"))?;
    let meta = body
        .get(4096..LEGACY_SECTION)
        .and_then(|b| <[u8; 2048]>::try_from(b).ok())
        .ok_or(Error::Malformed("legacy sub-chunk length"))?;
    Ok(LegacyLayer {
        ids: Box::new(ids),
        meta: Box::new(meta),
    })
}

/// Bits per index (checked) and the packed words of a block layer.
fn packed(r: &mut Reader<'_>, bits: u8) -> Result<Vec<u32>> {
    if !WIDTHS.contains(&bits) {
        return Err(Error::Malformed("bits per index"));
    }
    let count = word_count(bits);
    let count = r
        .limit(count as u64, count, size_of::<u32>())
        .map_err(short)?;
    let mut words = Vec::with_capacity(count);
    for _ in 0..count {
        words.push(r.uint32().map_err(short)?);
    }
    Ok(words)
}

fn block_layer(r: &mut Reader<'_>) -> Result<BlockLayer> {
    let header = r.uint8().map_err(short)?;
    if header & 1 != 0 {
        return Err(Error::Malformed("block layer holds network ids"));
    }
    let bits = header >> 1;
    let words = packed(r, bits)?;
    // The palette length: 1 without a length for 0 bits.
    let len = if bits == 0 {
        1
    } else {
        usize::try_from(r.int32().map_err(short)?)
            .ok()
            .filter(|&len| len >= 1 && len <= BLOCKS.min(1 << bits))
            .ok_or(Error::Malformed("palette length"))?
    };
    let len = r
        .limit(len as u64, len, size_of::<Compound>())
        .map_err(short)?;
    let mut palette = Vec::with_capacity(len);
    for _ in 0..len {
        palette.push(r.nbt(Encoding::LittleEndian).map_err(Error::Nbt)?);
    }
    check_indices(bits, &words, palette.len())
        .map_err(|_| Error::Malformed("block index outside the palette"))?;
    Ok(BlockLayer {
        bits,
        words,
        palette,
    })
}

/// Legacy terrain: eight legacy sub-chunks from y 0.
#[expect(
    clippy::indexing_slicing,
    reason = "invariant: the length is checked first and every index is below its array's length"
)]
fn legacy_terrain(value: &[u8]) -> Result<Vec<Section>> {
    if value.len() != LEGACY_TERRAIN_LEN {
        return Err(Error::Malformed("legacy terrain length"));
    }
    let ids = &value[..LEGACY_IDS];
    let meta = &value[LEGACY_IDS..LEGACY_IDS + LEGACY_META];
    let mut sections = Vec::with_capacity(8);
    for sy in 0..8_usize {
        let mut layer = LegacyLayer {
            ids: Box::new([0; 4096]),
            meta: Box::new([0; 2048]),
        };
        for x in 0..16_usize {
            for z in 0..16_usize {
                for y in 0..16_usize {
                    let column = x << 11 | z << 7 | (sy << 4 | y);
                    let at = x << 8 | z << 4 | y;
                    layer.ids[at] = ids[column];
                    let nibble = meta[column >> 1] >> ((column & 1) * 4) & 0xf;
                    layer.meta[at >> 1] |= nibble << ((at & 1) * 4);
                }
            }
        }
        sections.push(Section {
            y: i8::try_from(sy).unwrap_or(0),
            version: 0,
            layers: vec![Layer::Legacy(layer)],
        });
    }
    Ok(sections)
}

/// Legacy block extra data.
fn legacy_extra(value: &[u8], version: u8) -> Result<Vec<LegacyBlock>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let (count, pairs) = value
        .split_first_chunk::<4>()
        .ok_or(Error::Malformed("legacy extra data shorter than its count"))?;
    let count = usize::try_from(i32::from_le_bytes(*count))
        .map_err(|_| Error::Malformed("legacy extra data count"))?;
    let (pairs, rest) = pairs.as_chunks::<6>();
    if pairs.len() != count || !rest.is_empty() {
        return Err(Error::Malformed(
            "legacy extra data length differs from its count",
        ));
    }
    Ok(pairs
        .iter()
        .map(|&[k0, k1, k2, k3, v0, v1]| {
            let key = u32::from_le_bytes([k0, k1, k2, k3]);
            let (x, z, y) = if version >= 3 {
                (key >> 12 & 0xf, key >> 8 & 0xf, key & 0xff)
            } else {
                (key >> 11 & 0xf, key >> 7 & 0xf, key & 0x7f)
            };
            let byte = |n: u32| u8::try_from(n).unwrap_or(0);
            LegacyBlock {
                x: byte(x),
                y: byte(y),
                z: byte(z),
                id: v0,
                meta: v1 & 0xf,
            }
        })
        .collect())
}

#[cfg(test)]
pub(crate) mod tests;
