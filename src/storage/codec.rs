//! Record codecs: the bytes of chunk, player, world and meta values before they are sealed.
//!
//! Every count and length is checked by [`Reader::limit`] before it allocates, every block and
//! biome index must point into its palette, and a value must end exactly where its record ends.
//! Block palette entries are little-endian NBT compounds (Bedrock's disk palette shape
//! `{name, states, version}`), kept as they are, so unknown blocks survive a round trip.
//!
//! Every record has the one layout of the current format (the seal refuses any other). NBT keeps
//! the codec's limits: [`MAX_DEPTH`](crate::nbt::MAX_DEPTH) levels and [`MAX_ROOT_BYTES`] per
//! root compound, except a player's extra compound, which may fill its record
//! ([`MAX_PLAYER_EXTRA_BYTES`]).

use std::borrow::Cow;
use std::mem::size_of;

use super::ChunkKind;
use super::seal::{DECODE_BUDGET, MAX_RECORD_BYTES};
use crate::nbt::{Compound, Encoding, MAX_ROOT_BYTES};
use crate::protocol::{
    BLOCKS, Error, Reader, Result, WIDTHS, Writer, length, length_max, word_count,
};
use crate::world::BlockPos;

/// Most compounds in a chunk's block entity list, and in one [`ExtraRecord`]: a block entity per
/// block of a chunk column, the spills and the entries kept as read, so a list read within it
/// can take a block entity at every block and the spills and still be written.
pub(crate) const MAX_BLOCK_ENTITY_ENTRIES: usize =
    MAX_EXTRA_ENTRIES + MAX_SPILLS + MAX_RAW_BLOCK_ENTITIES;
/// Most compounds in a chunk's list of entities, and in a list an import writes: one per block of
/// a chunk column. Entities are bounded by the world's entity cap long before.
pub(crate) const MAX_EXTRA_ENTRIES: usize = SUB_CHUNKS * BLOCKS;
/// Block layers in a sub-chunk: layer 0 and the waterlogging layer 1.
pub(crate) const MAX_LAYERS: usize = 2;
/// Longest name a record holds (a player's world, a ban's reason and source).
pub(crate) const MAX_NAME_BYTES: usize = 256;
/// Most pages a list takes: the page number of a key is a u16.
const MAX_PAGES: usize = 1 << 16;
/// The most bytes a player's extra compound takes: what its record holds after the longest world
/// name and the position. It holds up to `MAX_SAVED_STACKS` (`tick::inventory`) stacks and the
/// user's data, more than the [`MAX_ROOT_BYTES`] of one NBT root elsewhere; the record's reader budget covers
/// it ([`MAX_NBT_BYTES`](crate::item::MAX_NBT_BYTES)).
pub(crate) const MAX_PLAYER_EXTRA_BYTES: usize = MAX_RECORD_BYTES - (2 + MAX_NAME_BYTES + 3 * 8);
/// Most entries a chunk's block entity list keeps as read that are no block's block entity (no
/// place in the chunk, or a second at a place): every entry of a list an import writes.
pub(crate) const MAX_RAW_BLOCK_ENTITIES: usize = MAX_EXTRA_ENTRIES;
/// Most spills (the items of broken containers waiting to drop) a chunk's block entity list
/// holds; beyond it new spills are lost with an error (a chunk would need this many containers
/// broken while its world is at the entity limit).
pub(crate) const MAX_SPILLS: usize = 1024;
/// Sub-chunks in a chunk column, from index -4 (y -64) to 19 (y 304..=319).
pub(crate) const SUB_CHUNKS: usize = 24;
// A ban the list takes can be stored.
const _: () = assert!(
    crate::permission::MAX_REASON <= MAX_NAME_BYTES
        && crate::permission::MAX_SOURCE <= MAX_NAME_BYTES
);

/// A value that has a sealed form in one storage table.
pub(crate) trait Record: Sized {
    /// Writes the record in the current format.
    fn encode(&self, w: &mut Writer) -> Result<()>;
    /// Reads a record that must take the whole input.
    fn decode(r: &mut Reader<'_>) -> Result<Self>;
}

/// One block layer of a sub-chunk: indices packed as Bedrock packs them, into a palette of
/// block states in disk form.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BlockLayer {
    /// Bits per index: 0, 1, 2, 3, 4, 5, 6, 8 or 16.
    pub(crate) bits: u8,
    /// The packed indices, block (x, y, z) at offset `x << 8 | z << 4 | y`.
    pub(crate) words: Vec<u32>,
    /// Block states as `{name, states, version}` compounds, or whatever NBT an unknown block had.
    pub(crate) palette: Vec<Compound>,
}

/// The biomes of a sub-chunk: indices packed like [`BlockLayer`] into a palette of biome IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Biomes {
    /// Bits per index: 0, 1, 2, 3, 4, 5, 6, 8 or 16.
    pub(crate) bits: u8,
    /// The packed indices.
    pub(crate) words: Vec<u32>,
    /// Biome IDs.
    pub(crate) palette: Vec<u32>,
}

/// One 16×16×16 section of a chunk column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubChunkRecord {
    /// Layer 0, then layer 1 if any; empty when every block is air.
    pub(crate) layers: Vec<BlockLayer>,
    /// The biomes, present for every sub-chunk.
    pub(crate) biomes: Biomes,
}

impl SubChunkRecord {
    /// Reads one sub-chunk, leaving the input after it.
    fn read(r: &mut Reader<'_>) -> Result<Self> {
        let layer_count = r.uint8()?;
        let layer_count = r.limit(layer_count.into(), MAX_LAYERS, size_of::<BlockLayer>())?;
        let mut layers = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            let (bits, words, palette_len) = read_packed(r)?;
            let palette_len = r.limit(length(palette_len)?, palette_len, size_of::<Compound>())?;
            let mut palette = Vec::with_capacity(palette_len);
            for _ in 0..palette_len {
                palette.push(r.nbt(Encoding::LittleEndian)?);
            }
            check_indices(bits, &words, palette.len())?;
            layers.push(BlockLayer {
                bits,
                words,
                palette,
            });
        }
        let (bits, words, palette_len) = read_packed(r)?;
        let palette_len = r.limit(length(palette_len)?, palette_len, size_of::<u32>())?;
        let mut palette = Vec::with_capacity(palette_len);
        for _ in 0..palette_len {
            palette.push(r.uint32()?);
        }
        check_indices(bits, &words, palette.len())?;
        Ok(Self {
            layers,
            biomes: Biomes {
                bits,
                words,
                palette,
            },
        })
    }

    /// Writes the layers and the biomes, refusing parts that do not match their indices.
    fn write(&self, w: &mut Writer) -> Result<()> {
        if self.layers.len() > MAX_LAYERS {
            return Err(Error::InvalidValue {
                field: "sub-chunk",
                reason: "more than two layers",
            });
        }
        w.uint8(length(self.layers.len())?);
        for layer in &self.layers {
            write_packed(w, layer.bits, &layer.words, layer.palette.len())?;
            for entry in &layer.palette {
                w.nbt(entry, Encoding::LittleEndian)?;
            }
        }
        let biomes = &self.biomes;
        write_packed(w, biomes.bits, &biomes.words, biomes.palette.len())?;
        for &id in &biomes.palette {
            w.uint32(id);
        }
        Ok(())
    }
}

/// The terrain of one sub-chunk: the `chunks` value of a sub-chunk key.
impl Record for SubChunkRecord {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        self.write(w)
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let record = Self::read(r)?;
        end(r)?;
        Ok(record)
    }
}

/// The terrain of a chunk column: its 24 sub-chunk values put together.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChunkRecord {
    /// Sub-chunks from index -4 up: entry `i` is index `i - 4`.
    pub(crate) sub_chunks: Box<[SubChunkRecord; SUB_CHUNKS]>,
}

/// A whole column as one record, which no file holds: the tests seal it as a large record.
#[cfg(test)]
impl Record for ChunkRecord {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        self.sub_chunks.iter().try_for_each(|sub| sub.write(w))
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut sub_chunks = Vec::with_capacity(SUB_CHUNKS);
        for _ in 0..SUB_CHUNKS {
            sub_chunks.push(SubChunkRecord::read(r)?);
        }
        end(r)?;
        let sub_chunks = sub_chunks
            .into_boxed_slice()
            .try_into()
            .map_err(|_| Error::Size)?;
        Ok(Self { sub_chunks })
    }
}

/// What is kept of a player between joins: the `players` value.
///
/// The world name, the position as three f64, then `extra`, a little-endian NBT root compound of
/// at most [`MAX_PLAYER_EXTRA_BYTES`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlayerRecord {
    /// The name of the world the player is in.
    pub(crate) world: String,
    /// The position, finite in each component.
    pub(crate) position: [f64; 3],
    /// What the rule modules keep of the player, each under its own keys.
    pub(crate) extra: Compound,
}

impl Record for PlayerRecord {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        write_name(w, &self.world)?;
        for &c in &self.position {
            if !c.is_finite() {
                return Err(not_finite());
            }
            w.float64(c);
        }
        w.nbt_within(&self.extra, Encoding::LittleEndian, MAX_PLAYER_EXTRA_BYTES)
            .map(drop)
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let world = read_name(r)?;
        let mut position = [0.0; 3];
        for c in &mut position {
            *c = r.float64()?;
            if !c.is_finite() {
                return Err(not_finite());
            }
        }
        let extra = r.nbt_within(Encoding::LittleEndian, MAX_PLAYER_EXTRA_BYTES)?;
        end(r)?;
        Ok(Self {
            world,
            position,
            extra,
        })
    }
}

/// A page of the block entities or entities of a chunk: the `chunks`
/// value of an extras page. A u32 count, then that
/// many little-endian NBT root compounds, each as the module that owns the kind wrote it. A list
/// is stored in [`pages`](Self::pages), its entries in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExtraRecord<'a>(pub(crate) Cow<'a, [Compound]>);

impl ExtraRecord<'_> {
    /// Returns the encoded pages that hold `list`, a chunk's list of `kind`, in order, each at most
    /// `max` bytes and read back within the [`DECODE_BUDGET`] of a record's reader: entries are
    /// added to a page until the next would not fit either. An entry is at most an NBT root
    /// ([`MAX_ROOT_BYTES`]) and a quarter of the budget (an error otherwise), so with `max` at
    /// least four times the root every full page holds more than three quarters of `max` or of
    /// the budget. More pages than a key numbers ([`MAX_PAGES`], an error) so need over half that
    /// many full pages of one of the two: at [`MAX_RECORD_BYTES`] pages, hundreds of GiB of NBT or
    /// TiB of reading budget, far past what a server holds in memory. A list over [`max_entries`]
    /// is an error too. An empty list has none.
    pub(crate) fn pages(kind: ChunkKind, list: &[Compound], max: usize) -> Result<Vec<Vec<u8>>> {
        if list.len() > max_entries(kind) {
            return Err(Error::Size);
        }
        let mut pages = Vec::new();
        let mut page: Vec<u8> = Vec::new();
        let (mut count, mut page_alloc) = (0_u32, 0_usize);
        let mut entry = Vec::new();
        for compound in list {
            let mut w = Writer::new(std::mem::take(&mut entry), 0);
            let cost = w.nbt_within(compound, Encoding::LittleEndian, MAX_ROOT_BYTES)?;
            entry = w.into_inner();
            // The decoder takes a list place for each entry ([`Self::decode`]).
            let alloc = cost.alloc.saturating_add(size_of::<Compound>());
            if alloc > DECODE_BUDGET / 4 {
                return Err(Error::Size);
            }
            if count > 0
                && (page.len().saturating_add(entry.len()) > max
                    || page_alloc.saturating_add(alloc) > DECODE_BUDGET)
            {
                close_page(&mut page, count, &mut pages);
                (count, page_alloc) = (0, 0);
            }
            if page.is_empty() {
                page.extend_from_slice(&[0; size_of::<u32>()]);
            }
            page.extend_from_slice(&entry);
            count += 1;
            page_alloc = page_alloc.saturating_add(alloc);
            entry.clear();
        }
        if count > 0 {
            close_page(&mut page, count, &mut pages);
        }
        if pages.len() > MAX_PAGES {
            return Err(Error::Size);
        }
        Ok(pages)
    }
}

impl Record for ExtraRecord<'_> {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.uint32(length_max(self.0.len(), MAX_BLOCK_ENTITY_ENTRIES)?);
        for entry in self.0.iter() {
            w.nbt(entry, Encoding::LittleEndian)?;
        }
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let count = r.uint32()?;
        // A root compound takes at least one byte (a lone TAG_End).
        let count = r.limit(
            count.into(),
            MAX_BLOCK_ENTITY_ENTRIES,
            size_of::<Compound>(),
        )?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(r.nbt(Encoding::LittleEndian)?);
        }
        end(r)?;
        Ok(Self(Cow::Owned(entries)))
    }
}

/// A server data value of the user: the `user` value, keyed by the data's name.
/// The JSON bytes as they are; the user type's deserializer checks them. A commit encodes the
/// tick's bytes in place (borrowed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserRecord<'a>(pub(crate) Cow<'a, [u8]>);

impl Record for UserRecord<'_> {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.bytes(&self.0);
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self(Cow::Owned(r.bytes()?)))
    }
}

/// A persistent world: the `worlds` value of its world file (key `world`). It holds the world's
/// state alone, never a setting: the seed, generator, difficulty, time stop and weather come from
/// the settings and the code at every start.
///
/// The number, then the world time (i64), then the spawn (i32 x, y, z).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorldRecord {
    /// The number that `chunks` keys carry for this world.
    pub(crate) number: u32,
    /// The world time in ticks at the commit (`World::time_ticks`): the day cycle goes on from it.
    pub(crate) time: i64,
    /// Where players appear (`World::spawn`): the settings' spawn only places a new world's.
    pub(crate) spawn: BlockPos,
}

impl Record for WorldRecord {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.uint32(self.number);
        w.int64(self.time);
        w.int32(self.spawn.x);
        w.int32(self.spawn.y);
        w.int32(self.spawn.z);
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let number = r.uint32()?;
        let time = r.int64()?;
        let spawn = BlockPos::new(r.int32()?, r.int32()?, r.int32()?);
        end(r)?;
        Ok(Self {
            number,
            time,
            spawn,
        })
    }
}

/// Engine state committed with every save: the `meta` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    /// The engine tick the saved state belongs to.
    pub(crate) tick: u64,
    /// The commit identity of the saved state (the input log's chain); `None` for a file no
    /// save has written yet.
    pub(crate) identity: Option<u64>,
}

#[cfg(test)]
impl Meta {
    /// Returns the meta value of `tick` without a commit identity.
    pub(crate) fn at(tick: u64) -> Self {
        Self {
            tick,
            identity: None,
        }
    }
}

impl Record for Meta {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.uint64(self.tick);
        w.bool(self.identity.is_some());
        if let Some(identity) = self.identity {
            w.uint64(identity);
        }
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let tick = r.uint64()?;
        let identity = if r.bool()? { Some(r.uint64()?) } else { None };
        end(r)?;
        Ok(Self { tick, identity })
    }
}

/// The value of an operator or whitelist entry in `access`: the key says it all, so the record
/// is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mark;

impl Record for Mark {
    fn encode(&self, _w: &mut Writer) -> Result<()> {
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        end(r)?;
        Ok(Self)
    }
}

/// The value of a name or IP ban in `access` (without the name, which is the
/// key): the reason and the source as names (at most [`MAX_NAME_BYTES`]), the engine tick it was
/// added at, then a flag byte and the engine tick it ends at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BanRecord {
    pub(crate) reason: String,
    pub(crate) source: String,
    pub(crate) created: u64,
    pub(crate) expires: Option<u64>,
}

impl Record for BanRecord {
    fn encode(&self, w: &mut Writer) -> Result<()> {
        write_name(w, &self.reason)?;
        write_name(w, &self.source)?;
        w.uint64(self.created);
        w.bool(self.expires.is_some());
        w.uint64(self.expires.unwrap_or(0));
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let reason = read_name(r)?;
        let source = read_name(r)?;
        let created = r.uint64()?;
        let has_end = r.bool()?;
        let end_tick = r.uint64()?;
        end(r)?;
        Ok(Self {
            reason,
            source,
            created,
            expires: has_end.then_some(end_tick),
        })
    }
}

/// The most compounds in a chunk's list of `kind`.
pub(crate) fn max_entries(kind: ChunkKind) -> usize {
    match kind {
        ChunkKind::BlockEntities => MAX_BLOCK_ENTITY_ENTRIES,
        ChunkKind::Entities => MAX_EXTRA_ENTRIES,
    }
}

/// Reads a width, its words and the palette length that follows (1 without a length for
/// width 0), checking each against its limit.
fn read_packed(r: &mut Reader<'_>) -> Result<(u8, Vec<u32>, usize)> {
    let bits = r.uint8()?;
    if !WIDTHS.contains(&bits) {
        return Err(Error::UnknownEnum {
            name: "storage index bits",
            value: bits.into(),
        });
    }
    let count = r.limit(length(word_count(bits))?, usize::MAX, size_of::<u32>())?;
    let mut words = Vec::with_capacity(count);
    for _ in 0..count {
        words.push(r.uint32()?);
    }
    let palette_len = if bits == 0 {
        1
    } else {
        usize::from(r.uint16()?)
    };
    if palette_len == 0 || palette_len > BLOCKS.min(1 << bits) {
        return Err(Error::InvalidValue {
            field: "storage palette",
            reason: "length is zero or more than the width indexes",
        });
    }
    Ok((bits, words, palette_len))
}

/// Writes a width, its words and the palette length (none for width 0).
fn write_packed(w: &mut Writer, bits: u8, words: &[u32], palette_len: usize) -> Result<()> {
    let valid = WIDTHS.contains(&bits)
        && words.len() == word_count(bits)
        && palette_len != 0
        && palette_len <= BLOCKS.min(1 << bits)
        && indices_below(bits, words, palette_len);
    if !valid {
        return Err(Error::InvalidValue {
            field: "storage",
            reason: "width, word count or palette does not match the indices",
        });
    }
    w.uint8(bits);
    for &word in words {
        w.uint32(word);
    }
    if bits != 0 {
        w.uint16(length(palette_len)?);
    }
    Ok(())
}

/// Fails unless each of the [`BLOCKS`] indices packed in `words` at `bits` per index is below
/// `palette_len`.
pub(crate) fn check_indices(bits: u8, words: &[u32], palette_len: usize) -> Result<()> {
    if indices_below(bits, words, palette_len) {
        Ok(())
    } else {
        Err(Error::InvalidValue {
            field: "storage",
            reason: "index outside the palette",
        })
    }
}

/// Whether each of the [`BLOCKS`] indices packed in `words` is below `palette_len`.
fn indices_below(bits: u8, words: &[u32], palette_len: usize) -> bool {
    if bits == 0 {
        return palette_len > 0;
    }
    // Every index the width can hold is in the palette. Callers pass widths up to 16.
    if palette_len >= 1 << bits {
        return true;
    }
    let per_word = 32 / usize::from(bits);
    let mask = (1_u32 << bits) - 1;
    let mut left = BLOCKS;
    words.iter().all(|&word| {
        let here = left.min(per_word);
        left -= here;
        // The indices of the word from its low bits up; the last word's high bits hold none.
        let mut rest = word;
        (0..here).all(|_| {
            let index = rest & mask;
            rest >>= bits;
            usize::try_from(index).is_ok_and(|index| index < palette_len)
        })
    })
}

/// Writes the entry count at the start of `page` and moves it to `pages`.
#[expect(
    clippy::expect_used,
    reason = "invariant: a page is opened with its count before its first entry"
)]
fn close_page(page: &mut Vec<u8>, count: u32, pages: &mut Vec<Vec<u8>>) {
    let head = page
        .first_chunk_mut::<4>()
        .expect("a page starts with its entry count");
    *head = count.to_le_bytes();
    pages.push(std::mem::take(page));
}

/// Returns the error of a position component that is not finite.
fn not_finite() -> Error {
    Error::InvalidValue {
        field: "player position",
        reason: "not finite",
    }
}

/// Fails unless the input is used up.
fn end(r: &Reader<'_>) -> Result<()> {
    if r.remaining().is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidValue {
            field: "record",
            reason: "bytes after the end",
        })
    }
}

/// Reads a name: a u16 byte length and at most [`MAX_NAME_BYTES`] of UTF-8.
fn read_name(r: &mut Reader<'_>) -> Result<String> {
    let len = r.uint16()?;
    let len = r.limit(len.into(), MAX_NAME_BYTES, 1)?;
    r.utf8(length(len)?)
}

/// Writes a name as [`read_name`] reads it; one over [`MAX_NAME_BYTES`] is [`Error::Size`].
fn write_name(w: &mut Writer, name: &str) -> Result<()> {
    w.uint16(length_max(name.len(), MAX_NAME_BYTES)?);
    w.bytes(name.as_bytes());
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::nbt::{List, Value};
    use crate::protocol::Rng;

    /// The index of the lowest sub-chunk.
    const MIN_SUB_CHUNK: i8 = -4;

    /// Encodes `record` in the current format.
    pub(crate) fn encode(record: &impl Record) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        record.encode(&mut w).unwrap();
        w.into_inner()
    }

    /// Decodes a record that must take all of `bytes`.
    pub(crate) fn decode<R: Record>(bytes: &[u8]) -> Result<R> {
        R::decode(&mut Reader::new(bytes, DECODE_BUDGET, 0))
    }

    /// Returns a random compound of a few kinds of tags, nested up to `depth`.
    pub(crate) fn random_compound(rng: &mut Rng, depth: usize) -> Compound {
        let mut c = Compound::new();
        for i in 0..rng.below(6) {
            let key = format!("k{i}-{}", rng.below(100));
            let value = match rng.below(if depth == 0 { 6 } else { 8 }) {
                0 => Value::Byte(rng.next().to_le_bytes()[0]),
                1 => Value::Int(i32::try_from(rng.below(1 << 30)).unwrap() - (1 << 29)),
                2 => Value::Long(rng.next().cast_signed()),
                3 => Value::Double(f64::from(u32::try_from(rng.below(1 << 20)).unwrap()) / 8.0),
                4 => Value::String("x".repeat(rng.below(40))),
                5 => Value::List(List::Int(
                    (0..rng.below(5))
                        .map(|v| i32::try_from(v).unwrap())
                        .collect(),
                )),
                6 => Value::Compound(random_compound(rng, depth - 1)),
                _ => Value::List(List::Compound(
                    (0..rng.below(3))
                        .map(|_| random_compound(rng, depth - 1))
                        .collect(),
                )),
            };
            c.insert(key, value);
        }
        c
    }

    /// Returns the Bedrock disk palette entry of the block `name` with `states`.
    pub(crate) fn block(name: &str, states: &[(&str, Value)]) -> Compound {
        let states = states
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        Compound::from([
            ("name".to_owned(), Value::String(name.to_owned())),
            ("states".to_owned(), Value::Compound(states)),
            (
                "version".to_owned(),
                Value::Int(crate::import::BLOCK_STATE_VERSION),
            ),
        ])
    }

    /// Packs `indices` (one per block, in offset order) at `bits` per index.
    pub(crate) fn pack(bits: u8, indices: &[u16]) -> Vec<u32> {
        let mut words = vec![0; word_count(bits)];
        if bits == 0 {
            return words;
        }
        let per_word = 32 / usize::from(bits);
        for (i, &index) in indices.iter().enumerate() {
            let shift = (i % per_word) * usize::from(bits);
            words[i / per_word] |= u32::from(index) << shift;
        }
        words
    }

    /// Returns the narrowest width that indexes `len` palette entries.
    pub(crate) fn width(len: usize) -> u8 {
        WIDTHS.into_iter().find(|&b| len <= 1 << b).unwrap()
    }

    /// Returns the biomes of a sub-chunk that is the biome `id` everywhere.
    fn uniform_biomes(id: u32) -> Biomes {
        Biomes {
            bits: 0,
            words: Vec::new(),
            palette: vec![id],
        }
    }

    /// Returns a random chunk: random sub-chunks are air, others have one or two layers of random width
    /// and palette, and the biomes vary.
    pub(crate) fn random_chunk(rng: &mut Rng) -> ChunkRecord {
        let sub_chunks: Vec<SubChunkRecord> = (0..SUB_CHUNKS)
            .map(|_| {
                let layers = (0..rng.below(3))
                    .map(|_| {
                        let len = 1 + rng.below(20);
                        let bits = width(len);
                        let indices: Vec<u16> = (0..BLOCKS)
                            .map(|_| u16::try_from(rng.below(len)).unwrap())
                            .collect();
                        let palette = (0..len)
                            .map(|i| {
                                block(
                                    &format!("minecraft:block_{i}"),
                                    &[("facing", Value::Int(i32::try_from(i).unwrap()))],
                                )
                            })
                            .collect();
                        BlockLayer {
                            bits,
                            words: pack(bits, &indices),
                            palette,
                        }
                    })
                    .collect();
                let len = 1 + rng.below(3);
                let bits = width(len);
                let indices: Vec<u16> = (0..BLOCKS)
                    .map(|_| u16::try_from(rng.below(len)).unwrap())
                    .collect();
                SubChunkRecord {
                    layers,
                    biomes: Biomes {
                        bits,
                        words: pack(bits, &indices),
                        palette: (0..len)
                            .map(|_| u32::try_from(rng.below(200)).unwrap())
                            .collect(),
                    },
                }
            })
            .collect();
        ChunkRecord {
            sub_chunks: sub_chunks.into_boxed_slice().try_into().unwrap(),
        }
    }

    /// Returns a chunk shaped like generated terrain: bedrock, deepslate and stone with ore and rock
    /// pockets, dirt and grass on a sloped surface near y 64, water up to y 62, plants, then air.
    /// Biomes change across the surface sub-chunks.
    pub(crate) fn terrain_chunk(rng: &mut Rng) -> ChunkRecord {
        let small = |rng: &mut Rng, n: usize| i32::try_from(rng.below(n)).unwrap();
        let base = 50 + small(rng, 25);
        let (slope_x, slope_z) = (small(rng, 9) - 4, small(rng, 9) - 4);
        let mut height = [[0; 16]; 16];
        for (x, row) in (0..).zip(&mut height) {
            for (z, h) in (0..).zip(row.iter_mut()) {
                *h = base + (slope_x * x + slope_z * z) / 4 + small(rng, 3) - 1;
            }
        }
        // Pockets: (x, y, z, radius², block).
        let pockets: Vec<(i32, i32, i32, i32, &'static str)> = (0..24)
            .map(|_| {
                let kind = [
                    "minecraft:coal_ore",
                    "minecraft:iron_ore",
                    "minecraft:copper_ore",
                    "minecraft:andesite",
                    "minecraft:granite",
                    "minecraft:diorite",
                    "minecraft:gravel",
                    "minecraft:deepslate_diamond_ore",
                ][rng.below(8)];
                let r = 1 + small(rng, 3);
                (
                    small(rng, 16),
                    small(rng, 120) - 64,
                    small(rng, 16),
                    r * r,
                    kind,
                )
            })
            .collect();
        let plants: Vec<u16> = (0..256)
            .map(|_| match rng.below(40) {
                0..=4 => 1,
                5 => 2,
                _ => 0,
            })
            .collect();
        let sub_chunks: Vec<SubChunkRecord> = (0..SUB_CHUNKS)
            .map(|index| {
                let bottom = (i32::try_from(index).unwrap() + i32::from(MIN_SUB_CHUNK)) * 16;
                let mut names: Vec<&str> = Vec::new();
                let mut indices = vec![0_u16; BLOCKS];
                for (offset, slot) in indices.iter_mut().enumerate() {
                    let (x, z) = (offset >> 8, (offset >> 4) & 15);
                    let y = bottom + i32::try_from(offset & 15).unwrap();
                    let (xi, zi) = (i32::try_from(x).unwrap(), i32::try_from(z).unwrap());
                    let h = height[x][z];
                    let pocket = pockets.iter().find(|&&(px, py, pz, r2, _)| {
                        (px - xi).pow(2) + (py - y).pow(2) + (pz - zi).pow(2) <= r2
                    });
                    let name = terrain_block(y, h, pocket.map(|p| p.4), plants[x * 16 + z]);
                    let at = names.iter().position(|&n| n == name).unwrap_or_else(|| {
                        names.push(name);
                        names.len() - 1
                    });
                    *slot = u16::try_from(at).unwrap();
                }
                let layers = if names == ["minecraft:air"] {
                    Vec::new()
                } else {
                    let bits = width(names.len());
                    vec![BlockLayer {
                        bits,
                        words: pack(bits, &indices),
                        palette: names.iter().map(|&n| terrain_state(n)).collect(),
                    }]
                };
                let surface = (bottom..bottom + 16).contains(&base);
                let biomes = if surface {
                    let split = rng.below(16);
                    let indices: Vec<u16> =
                        (0..BLOCKS).map(|o| u16::from((o >> 8) < split)).collect();
                    Biomes {
                        bits: 1,
                        words: pack(1, &indices),
                        palette: vec![1, 7],
                    }
                } else {
                    uniform_biomes(1)
                };
                SubChunkRecord { layers, biomes }
            })
            .collect();
        ChunkRecord {
            sub_chunks: sub_chunks.into_boxed_slice().try_into().unwrap(),
        }
    }

    /// Returns the block at height `y` of a terrain column whose surface is at `h`.
    fn terrain_block(y: i32, h: i32, pocket: Option<&'static str>, plant: u16) -> &'static str {
        if y == -64 {
            "minecraft:bedrock"
        } else if y < h - 3 {
            match pocket {
                Some(kind) => kind,
                None if y < 0 => "minecraft:deepslate",
                None => "minecraft:stone",
            }
        } else if y < h {
            "minecraft:dirt"
        } else if y == h && h >= 62 {
            "minecraft:grass_block"
        } else if y == h {
            "minecraft:sand"
        } else if y <= 62 {
            "minecraft:water"
        } else if y == h + 1 && plant == 1 {
            "minecraft:short_grass"
        } else if y == h + 1 && plant == 2 {
            "minecraft:poppy"
        } else {
            "minecraft:air"
        }
    }

    /// Returns the disk palette entry of the terrain block `name`.
    fn terrain_state(name: &str) -> Compound {
        match name {
            "minecraft:deepslate" => block(name, &[("pillar_axis", Value::String("y".to_owned()))]),
            "minecraft:water" => block(name, &[("liquid_depth", Value::Int(0))]),
            _ => block(name, &[]),
        }
    }

    /// Returns a chunk without blocks, all plains.
    pub(crate) fn empty_chunk() -> ChunkRecord {
        let sub_chunks: Vec<SubChunkRecord> = (0..SUB_CHUNKS)
            .map(|_| SubChunkRecord {
                layers: Vec::new(),
                biomes: uniform_biomes(1),
            })
            .collect();
        ChunkRecord {
            sub_chunks: sub_chunks.into_boxed_slice().try_into().unwrap(),
        }
    }

    #[test]
    fn random_records_round_trip() {
        let mut rng = Rng(0x0c0d_ec00);
        for _ in 0..20 {
            let chunk = random_chunk(&mut rng);
            let bytes = encode(&chunk);
            assert_eq!(decode::<ChunkRecord>(&bytes).unwrap(), chunk);
            let player = PlayerRecord {
                world: format!("world-{}", rng.below(1000)),
                position: [
                    f64::from(rng.next().to_le_bytes()[0]) / 7.0,
                    -64.0,
                    f64::MAX,
                ],
                extra: random_compound(&mut rng, 3),
            };
            assert_eq!(decode::<PlayerRecord>(&encode(&player)).unwrap(), player);
            let extras = ExtraRecord(
                (0..rng.below(6))
                    .map(|_| random_compound(&mut rng, 3))
                    .collect::<Vec<_>>()
                    .into(),
            );
            let bytes = encode(&extras);
            assert_eq!(decode::<ExtraRecord<'_>>(&bytes).unwrap(), extras);
            assert_eq!(encode(&decode::<ExtraRecord<'_>>(&bytes).unwrap()), bytes);
            let world = WorldRecord {
                number: u32::try_from(rng.below(1 << 20)).unwrap(),
                time: rng.next().cast_signed(),
                spawn: BlockPos::new(
                    u32::try_from(rng.next() >> 32).unwrap().cast_signed(),
                    u32::try_from(rng.next() >> 32).unwrap().cast_signed(),
                    u32::try_from(rng.next() >> 32).unwrap().cast_signed(),
                ),
            };
            assert_eq!(decode::<WorldRecord>(&encode(&world)).unwrap(), world);
            let meta = Meta {
                tick: rng.next(),
                identity: [None, Some(rng.next())][rng.below(2)],
            };
            assert_eq!(decode::<Meta>(&encode(&meta)).unwrap(), meta);
            let user = UserRecord(format!("[{}]", rng.below(1 << 20)).into_bytes().into());
            assert_eq!(decode::<UserRecord<'_>>(&encode(&user)).unwrap(), user);
        }
    }

    /// A list splits into pages of at most the limit, entries in order and never
    /// split; each page is an [`ExtraRecord`] of its entries, and an empty list has no page.
    #[test]
    fn extras_lists_split_into_pages_under_the_limit() {
        let mut rng = Rng(0x0c0d_ec01);
        let list: Vec<Compound> = (0..300).map(|_| random_compound(&mut rng, 3)).collect();
        let whole = encode(&ExtraRecord(list.clone().into()));
        let one = ExtraRecord::pages(ChunkKind::Entities, &list, whole.len()).unwrap();
        assert_eq!(one, std::slice::from_ref(&whole));
        let max = whole.len() / 5;
        let pages = ExtraRecord::pages(ChunkKind::Entities, &list, max).unwrap();
        assert!(pages.len() >= 5, "{} pages", pages.len());
        let mut read = Vec::new();
        for page in &pages {
            assert!(page.len() <= max);
            let entries = decode::<ExtraRecord<'_>>(page).unwrap().0.into_owned();
            assert!(!entries.is_empty());
            read.extend(entries);
        }
        assert_eq!(read, list);
        assert!(
            ExtraRecord::pages(ChunkKind::Entities, &[], max)
                .unwrap()
                .is_empty()
        );
        // A block entity list holds more: the spills and the entries kept as read too.
        let over = vec![Compound::new(); MAX_EXTRA_ENTRIES + 1];
        assert_eq!(
            ExtraRecord::pages(ChunkKind::Entities, &over, usize::MAX),
            Err(Error::Size)
        );
        assert!(ExtraRecord::pages(ChunkKind::BlockEntities, &over, usize::MAX).is_ok());
        let over = vec![Compound::new(); MAX_BLOCK_ENTITY_ENTRIES + 1];
        assert_eq!(
            ExtraRecord::pages(ChunkKind::BlockEntities, &over, usize::MAX),
            Err(Error::Size)
        );
    }

    #[test]
    fn unknown_palette_entries_survive_a_round_trip() {
        let odd = Compound::from([
            ("anything".to_owned(), Value::LongArray(vec![1, -2, 3])),
            (
                "nested".to_owned(),
                Value::List(List::Compound(vec![Compound::new(), block("x", &[])])),
            ),
        ]);
        let mut chunk = empty_chunk();
        chunk.sub_chunks[7].layers.push(BlockLayer {
            bits: 1,
            words: pack(1, &[1; BLOCKS]),
            palette: vec![block("minecraft:air", &[]), odd],
        });
        let bytes = encode(&chunk);
        assert_eq!(decode::<ChunkRecord>(&bytes).unwrap(), chunk);
    }

    #[test]
    fn invalid_records_are_refused_when_encoding() {
        let mut chunk = empty_chunk();
        chunk.sub_chunks[0].layers.push(BlockLayer {
            bits: 1,
            words: pack(1, &[1; BLOCKS]),
            palette: vec![block("a", &[])],
        });
        let mut w = Writer::new(Vec::new(), 0);
        assert!(
            chunk.encode(&mut w).is_err(),
            "index 1 with one palette entry"
        );
        let mut chunk = empty_chunk();
        chunk.sub_chunks[0].biomes = Biomes {
            bits: 7,
            words: Vec::new(),
            palette: vec![1],
        };
        assert!(chunk.encode(&mut w).is_err(), "width 7");
        let player = PlayerRecord {
            world: "w".to_owned(),
            position: [0.0, f64::NAN, 0.0],
            extra: Compound::new(),
        };
        assert!(player.encode(&mut w).is_err());
        // An extra compound over what its record holds.
        let player = PlayerRecord {
            world: "w".to_owned(),
            position: [0.0; 3],
            extra: Compound::from([(
                "big".to_owned(),
                Value::ByteArray(vec![0; MAX_PLAYER_EXTRA_BYTES]),
            )]),
        };
        assert!(player.encode(&mut w).is_err());
        let extras = ExtraRecord(vec![Compound::new(); MAX_BLOCK_ENTITY_ENTRIES + 1].into());
        assert_eq!(extras.encode(&mut w), Err(Error::Size));
    }

    #[test]
    fn over_limit_declarations_are_refused_before_allocating() {
        // Width 0 is fine; a palette length past what width 1 indexes is not.
        let mut bytes = vec![0_u8; 1];
        bytes.extend_from_slice(&[1]);
        bytes.extend(std::iter::repeat_n(0, 128 * 4));
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        assert!(matches!(
            decode::<ChunkRecord>(&bytes),
            Err(Error::InvalidValue { .. })
        ));
        // A name declared longer than the limit fails on the length, before the bytes.
        let mut bytes = u16::try_from(MAX_NAME_BYTES + 1)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_NAME_BYTES + 1));
        assert_eq!(decode::<PlayerRecord>(&bytes), Err(Error::Size));
        // A layer count over two.
        assert_eq!(decode::<ChunkRecord>(&[3]), Err(Error::Size));
        // A 16-bit palette of 4097 entries.
        let mut bytes = vec![1_u8, 16];
        bytes.extend(std::iter::repeat_n(0, 2048 * 4));
        bytes.extend_from_slice(&4097_u16.to_le_bytes());
        assert!(matches!(
            decode::<ChunkRecord>(&bytes),
            Err(Error::InvalidValue { .. })
        ));
        // The allocation budget caps what NBT palettes may claim.
        let chunk = random_chunk(&mut Rng(3));
        let bytes = encode(&chunk);
        let small = ChunkRecord::decode(&mut Reader::new(&bytes, 1024, 0));
        assert_eq!(small, Err(Error::Size));
        // An extras count over the limit, and one the bytes left cannot hold.
        let over = u32::try_from(MAX_BLOCK_ENTITY_ENTRIES + 1).unwrap();
        let mut bytes = over.to_le_bytes().to_vec();
        bytes.resize(MAX_BLOCK_ENTITY_ENTRIES + 8, 0);
        assert_eq!(decode::<ExtraRecord<'_>>(&bytes), Err(Error::Size));
        let mut bytes = 1000_u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 10]);
        assert_eq!(decode::<ExtraRecord<'_>>(&bytes), Err(Error::Size));
        // A compound nested past 512 levels.
        let mut deep = Compound::new();
        for _ in 0..600 {
            deep = Compound::from([("d".to_owned(), Value::Compound(deep))]);
        }
        let mut w = Writer::new(Vec::new(), 0);
        assert!(ExtraRecord(vec![deep].into()).encode(&mut w).is_err());
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[10, 0, 0]);
        for _ in 0..600 {
            bytes.extend_from_slice(&[10, 1, 0, b'd']);
        }
        bytes.extend(std::iter::repeat_n(0, 601));
        assert!(decode::<ExtraRecord<'_>>(&bytes).is_err());
    }

    #[test]
    fn world_records_hold_the_number_time_and_spawn() {
        let world = WorldRecord {
            number: 7,
            time: 123_456_789,
            spawn: BlockPos::new(-crate::world::BORDER, -64, crate::world::BORDER - 1),
        };
        let bytes = encode(&world);
        assert_eq!(bytes.len(), 24, "the number, the time and the spawn alone");
        assert_eq!(decode::<WorldRecord>(&bytes).unwrap(), world);
        assert!(decode::<WorldRecord>(&bytes[..20]).is_err());
        assert!(decode::<WorldRecord>(&[bytes.as_slice(), &[0]].concat()).is_err());
    }

    #[test]
    fn meta_values_keep_the_commit_identity() {
        let meta = Meta {
            tick: 5,
            identity: Some(u64::MAX),
        };
        let bytes = encode(&meta);
        assert_eq!(decode::<Meta>(&bytes).unwrap(), meta);
        assert!(decode::<Meta>(&bytes[..8]).is_err(), "the flag is there");
        let mut bad = bytes;
        bad[8] = 2;
        assert!(decode::<Meta>(&bad).is_err(), "a bool is 0 or 1");
    }

    #[test]
    fn trailing_bytes_and_non_finite_positions_are_refused() {
        let mut bytes = encode(&Meta::at(5));
        bytes.push(0);
        assert!(decode::<Meta>(&bytes).is_err());
        let mut bytes = encode(&PlayerRecord {
            world: "w".to_owned(),
            position: [1.0; 3],
            extra: Compound::new(),
        });
        // The z position sits before the empty compound's 4 bytes (tag, empty name, end).
        let at = bytes.len() - 4 - 8;
        bytes[at..at + 8].copy_from_slice(&f64::INFINITY.to_le_bytes());
        assert_eq!(decode::<PlayerRecord>(&bytes), Err(not_finite()));
        let mut extras = encode(&ExtraRecord(vec![Compound::new()].into()));
        extras.push(0);
        assert!(decode::<ExtraRecord<'_>>(&extras).is_err());
    }

    #[test]
    fn mutated_chunks_never_panic() {
        let mut rng = Rng(0xbad_c0de);
        let seed = encode(&random_chunk(&mut rng));
        let mut decoded = 0;
        for _ in 0..2000 {
            let mut bytes = seed.clone();
            for _ in 0..=rng.below(4) {
                let at = rng.below(bytes.len());
                match rng.below(3) {
                    0 => bytes[at] ^= 1 << rng.below(8),
                    1 => bytes.truncate(at),
                    _ => bytes.insert(at, rng.next().to_le_bytes()[0]),
                }
            }
            decoded += usize::from(decode::<ChunkRecord>(&bytes).is_ok());
        }
        // Flips inside index words can still decode; truncations and insertions cannot.
        assert!(decoded < 2000, "{decoded} decoded");
    }
}
