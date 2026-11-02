//! A chunk column: 24 sub-chunks from y -64 to 319, 3D biomes, the height of each
//! column and the original NBT of the block states Anchor does not know, and its network form.
//!
//! Sub-chunks and biomes are shared `Arc`s: a clone (a save snapshot) costs reference counts, and
//! a write copies only the sub-chunk it changes while a snapshot still holds it (`Arc::make_mut`).

use std::fmt;
use std::sync::Arc;

use crate::block::State;
use crate::nbt;
use crate::protocol::Writer;

use super::light::{self, ChunkLight};
use super::paletted::{Derive, Paletted, offset};
use super::sub_chunk::{Entry, SubChunk};
use super::{
    ChunkPos, Error, Generator, LOWEST_SUB_CHUNK, MAX_Y, MIN_Y, Result, StoredBlock, UnknownBlock,
};

/// Columns in a chunk.
pub(super) const COLUMNS: usize = 256;
/// The most unknown states a chunk can use: one per slot of both layers.
const MAX_UNKNOWN: usize = SUB_CHUNKS * 2 * 4096;
/// The [`Chunk::heights`] value of a column of air.
pub(crate) const NO_HEIGHT: i16 = i16::MIN;
/// The biome every slot of a new chunk has.
pub(crate) const PLAINS: u32 = 1;
/// Sub-chunks in a column: y -64 to 319.
pub(crate) const SUB_CHUNKS: usize = 24;

/// A 16×16 column of blocks from y -64 to 319, with biomes.
///
/// A [`Generator`] fills an empty one: all air, with plains biomes. Coordinates are local: x and
/// z from 0 to 15, y the world height (-64 to 319).
///
/// A clone shares the blocks and biomes and has no light: light is never saved, and a copy is
/// lit again where it is loaded.
// pmmp-class: Chunk
pub struct Chunk {
    sub_chunks: [Option<Arc<SubChunk>>; SUB_CHUNKS],
    biomes: [Arc<Paletted<u32>>; SUB_CHUNKS],
    /// The highest non-air layer 0 block of each column ([`column()`]), or [`NO_HEIGHT`].
    heights: Box<[i16; COLUMNS]>,
    /// The original NBT of each unknown state, by [`Entry::unknown_index`].
    unknown: Arc<Vec<nbt::Compound>>,
    /// The sub-chunks changed since the world's last save snapshot, one bit per index.
    dirty: u32,
    /// Whether the server file holds this chunk, so a save writes only the changed sub-chunks.
    /// Otherwise (generated) the next save writes all 24.
    split: bool,
    /// The chunk's light once it is worked out, kept apart from the shared sub-chunks so a light
    /// change never copies one.
    light: Option<Box<ChunkLight>>,
}

impl Chunk {
    /// An empty chunk: air, plains biomes. A generator gets one to fill; a test of a generator
    /// can make its own.
    pub fn new() -> Self {
        let plains = Arc::new(Paletted::uniform(PLAINS));
        Self {
            sub_chunks: Default::default(),
            biomes: std::array::from_fn(|_| Arc::clone(&plains)),
            heights: Box::new([NO_HEIGHT; COLUMNS]),
            unknown: Arc::new(Vec::new()),
            dirty: 0,
            split: false,
            light: None,
        }
    }

    /// Generates the chunk at `pos` (a pure function of the seed and the position),
    /// then compacts its palettes: the order of first use, as BDS writes them (P6), whatever
    /// order the generator wrote in.
    pub(crate) fn generate(generator: &dyn Generator, seed: i64, pos: ChunkPos) -> Self {
        let mut chunk = Self::new();
        generator.generate(seed, pos, &mut chunk);
        for sub in chunk.sub_chunks.iter_mut().flatten() {
            Arc::make_mut(sub).compact();
        }
        // A width 0 storage is compact already; the untouched ones stay shared.
        for biomes in chunk.biomes.iter_mut().filter(|b| b.bits() != 0) {
            Arc::make_mut(biomes).compact();
        }
        chunk
    }

    /// A chunk from stored parts (external input): the sub-chunks from the bottom,
    /// the biomes of each, and the unknown states the layers' unknown and inert entries index.
    /// Fails if an entry indexes past the unknown list or the list is longer than one per slot.
    /// Sub-chunks of air are dropped.
    pub(crate) fn from_parts(
        sub_chunks: [Option<SubChunk>; SUB_CHUNKS],
        biomes: [Paletted<u32>; SUB_CHUNKS],
        unknown: Vec<nbt::Compound>,
    ) -> Result<Self> {
        if !unknown_indexes_fit(sub_chunks.iter().flatten(), unknown.len()) {
            return Err(Error::Malformed(
                "more unknown block states than slots, or a palette entry indexes past them",
            ));
        }
        Ok(Self::assemble(
            sub_chunks.map(|s| s.filter(|s| !s.is_empty()).map(Arc::new)),
            biomes.map(Arc::new),
            unknown,
            0,
            false,
            None,
        ))
    }

    /// The block at local (x, y, z), or `None` if the position is outside the chunk or the
    /// block is a state Anchor does not know.
    #[doc(alias = "Chunk::getBlockStateId")]
    pub fn block(&self, x: u8, y: i32, z: u8) -> Option<State> {
        self.entry(x, y, z)?.state()
    }

    /// The biome ID at local (x, y, z), or `None` if the position is outside the chunk.
    #[doc(alias = "Chunk::getBiomeId")]
    pub fn biome(&self, x: u8, y: i32, z: u8) -> Option<u32> {
        let (index, y) = split_y(y).filter(|_| x < 16 && z < 16)?;
        Some(self.biomes.get(index)?.get(offset(x, y, z)))
    }

    /// The top y in the local column (x, z) that holds a block other than air, or `None` if the
    /// column is all air or outside the chunk.
    #[doc(alias = "Chunk::getHighestBlockAt")]
    pub fn highest_block(&self, x: u8, z: u8) -> Option<i32> {
        if x >= 16 || z >= 16 {
            return None;
        }
        let h = *self.heights.get(column(x, z))?;
        (h != NO_HEIGHT).then_some(i32::from(h))
    }

    /// The block at local (x, y, z) as the chunk holds it, or `None` outside the chunk.
    pub(super) fn stored_block(&self, x: u8, y: i32, z: u8) -> Option<StoredBlock> {
        Some(match self.entry(x, y, z)? {
            Entry::Known(state) => StoredBlock::Known(state),
            Entry::Unknown(i) | Entry::Inert(_, i) => {
                let index = usize::try_from(i).ok().filter(|&i| i < self.unknown.len());
                #[expect(
                    clippy::expect_used,
                    reason = "invariant: from_parts and the checkpoint read only chunks that pass unknown_indexes_fit"
                )]
                let index = index.expect("a chunk entry indexes past its unknown states");
                StoredBlock::Unknown(UnknownBlock {
                    list: Arc::clone(&self.unknown),
                    index,
                })
            }
        })
    }

    /// The layer 0 entry at local (x, y, z), or `None` outside the chunk.
    #[inline]
    pub(crate) fn entry(&self, x: u8, y: i32, z: u8) -> Option<Entry> {
        let (index, y) = split_y(y).filter(|_| x < 16 && z < 16)?;
        Some(match self.sub_chunks.get(index)? {
            Some(sub) => sub.get(offset(x, y, z)),
            None => Entry::AIR,
        })
    }

    /// The sub-chunk at `index` (0 is the lowest), or `None` if it is air.
    pub(crate) fn sub_chunk(&self, index: usize) -> Option<&SubChunk> {
        self.sub_chunks.get(index)?.as_deref()
    }

    /// The biomes of the sub-chunk at `index` (0 is the lowest).
    pub(crate) fn biome_storage(&self, index: usize) -> Option<&Paletted<u32>> {
        self.biomes.get(index).map(AsRef::as_ref)
    }

    /// The original NBT of the unknown states, by [`Entry::unknown_index`].
    pub(crate) fn unknown(&self) -> &[nbt::Compound] {
        &self.unknown
    }

    /// The highest non-air layer 0 block of each column, indexed `z × 16 + x`, or
    /// [`NO_HEIGHT`].
    pub(crate) fn heights(&self) -> &[i16; COLUMNS] {
        &self.heights
    }

    /// Whether the chunk changed since the world's last save snapshot.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty != 0
    }

    /// The sub-chunks a save of this snapshot writes, one bit per index: the changed ones, or
    /// all of them when the file does not hold the chunk yet.
    pub(crate) fn saved_sub_chunks(&self) -> u32 {
        if self.split {
            self.dirty
        } else {
            (1 << SUB_CHUNKS) - 1
        }
    }

    /// The chunk's light, if it is worked out.
    pub(super) fn light(&self) -> Option<&ChunkLight> {
        self.light.as_deref()
    }

    /// The light values of the layer 0 block at an offset of sub-chunk `index` (air's in an
    /// empty sub-chunk).
    #[inline]
    pub(super) fn light_values_at(&self, index: usize, offset: u16) -> light::Values {
        match self.sub_chunks.get(index) {
            Some(Some(sub)) => sub.light_values(offset),
            _ => Entry::AIR.derive(),
        }
    }

    /// The light values of the layer 0 block at local (x, y, z), or `None` outside the chunk.
    #[inline]
    pub(super) fn light_values(&self, x: u8, y: i32, z: u8) -> Option<light::Values> {
        let (index, y) = split_y(y).filter(|_| x < 16 && z < 16)?;
        Some(self.light_values_at(index, offset(x, y, z)))
    }

    /// The sub-chunks from the bottom up to the highest one that is not air (`SubChunkLimit`).
    pub(crate) fn sub_chunk_limit(&self) -> u8 {
        let Some(top) = self.sub_chunks.iter().rposition(Option::is_some) else {
            return 0;
        };
        #[expect(
            clippy::expect_used,
            reason = "invariant: a chunk has SUB_CHUNKS sub-chunks, far fewer than 256"
        )]
        u8::try_from(top + 1).expect("a sub-chunk limit fits u8")
    }

    /// The `LevelChunk` payload of the sub-chunk request system: the biomes of the 24
    /// sub-chunks (one equal to the one below it is written as 0xff), then 0 border blocks
    /// (dragonfly network encoding).
    pub(crate) fn level_chunk_payload(&self) -> Vec<u8> {
        // Each section once: `None` for one equal to the one below it.
        let sections = || {
            let mut below: Option<&Arc<Paletted<u32>>> = None;
            self.biomes.iter().map(move |biomes| {
                let repeated = below.is_some_and(|b| Arc::ptr_eq(b, biomes) || b == biomes);
                below = Some(biomes);
                (!repeated).then_some(biomes)
            })
        };
        let sections_bytes: usize = sections()
            .map(|section| section.map_or(1, |biomes| biomes.max_encoded_bytes()))
            .sum();
        let mut w = Writer::new(Vec::with_capacity(sections_bytes + 1), 0);
        for section in sections() {
            match section {
                Some(biomes) => biomes.encode(&mut w, |id| id),
                None => w.uint8(0xff),
            }
        }
        w.uint8(0);
        w.into_inner()
    }

    /// The network payload of the sub-chunk at `index` (0 is the lowest), or `None` if it is air
    /// (the client is told `SuccessAllAir`).
    pub(crate) fn sub_chunk_payload(&self, index: usize) -> Option<Vec<u8>> {
        let sub = self.sub_chunk(index)?;
        let y_index =
            i8::try_from(i32::try_from(index).ok()?.checked_add(LOWEST_SUB_CHUNK)?).ok()?;
        let mut w = Writer::new(Vec::with_capacity(sub.max_encoded_bytes()), 0);
        sub.encode(y_index, &mut w);
        Some(w.into_inner())
    }

    /// Sets the block at local (x, y, z). Neighbour connections (fences, stairs) are not worked
    /// out here: a generator writes the states it means.
    #[doc(alias = "Chunk::setBlockStateId")]
    pub fn set_block(&mut self, x: u8, y: i32, z: u8, state: State) -> Result<()> {
        self.put(x, y, z, Entry::Known(state))
            .map(|_| ())
            .ok_or(Error::OutOfRange)
    }

    /// Sets the biome ID at local (x, y, z), such as 1 for plains (the client's biome
    /// definitions number them).
    #[doc(alias = "Chunk::setBiomeId")]
    pub fn set_biome(&mut self, x: u8, y: i32, z: u8, biome: u32) -> Result<()> {
        let (index, y) = split_y(y)
            .filter(|_| x < 16 && z < 16)
            .ok_or(Error::OutOfRange)?;
        let storage = self.biomes.get_mut(index).ok_or(Error::OutOfRange)?;
        Arc::make_mut(storage).set(offset(x, y, z), biome);
        Ok(())
    }

    /// Finds or adds the entry of `block` in this chunk: its index here if it was read from this
    /// chunk's list, else that of an equal stored state, else a new one at the end. Fails with
    /// [`Error::Full`] when the list holds [`MAX_UNKNOWN`] states.
    pub(super) fn unknown_entry(&mut self, block: &UnknownBlock) -> Result<Entry> {
        let nbt = block.nbt();
        let found = if Arc::ptr_eq(&block.list, &self.unknown) {
            Some(block.index)
        } else {
            // A linear scan of a list that holds one state per unknown palette entry the chunk
            // was read with (a state in several palettes once for each) and each distinct state
            // written here once, at most `MAX_UNKNOWN`.
            self.unknown.iter().position(|n| n == nbt)
        };
        let index = if let Some(index) = found {
            index
        } else {
            if self.unknown.len() >= MAX_UNKNOWN {
                return Err(Error::Full);
            }
            // The list grows in place unless a snapshot or a stored block still shares it.
            let list = Arc::make_mut(&mut self.unknown);
            list.push(nbt.clone());
            list.len().saturating_sub(1)
        };
        #[expect(
            clippy::expect_used,
            reason = "invariant: the list holds at most MAX_UNKNOWN states, far below u32::MAX"
        )]
        let index = u32::try_from(index).expect("an unknown state index past u32");
        Ok(Entry::unknown(index, nbt))
    }

    /// Sets the layer 0 entry at local (x, y, z) and returns the old one, or `None` (changing
    /// nothing) outside the chunk. Keeps the column height and drops a sub-chunk that becomes
    /// air.
    pub(crate) fn put(&mut self, x: u8, y: i32, z: u8, entry: Entry) -> Option<Entry> {
        let (index, ly) = split_y(y).filter(|_| x < 16 && z < 16)?;
        let slot = self.sub_chunks.get_mut(index)?;
        let shared = match slot {
            None if entry == Entry::AIR => return Some(Entry::AIR),
            None => slot.insert(Arc::new(SubChunk::new())),
            Some(sub) => sub,
        };
        let at = offset(x, ly, z);
        // Checked before `make_mut`, so writing the same entry never copies a sub-chunk a save
        // snapshot or checkpoint shares.
        if shared.get(at) == entry {
            return Some(entry);
        }
        let sub = Arc::make_mut(shared);
        let old = sub.set(at, entry);
        if entry == Entry::AIR && sub.is_empty() {
            *slot = None;
        }
        let height = self.heights.get(column(x, z)).copied()?;
        let new_height = if entry != Entry::AIR {
            height.max(i16::try_from(y).ok()?)
        } else if i32::from(height) == y {
            self.scan_down(x, z, y - 1)
        } else {
            height
        };
        if let Some(h) = self.heights.get_mut(column(x, z)) {
            *h = new_height;
        }
        Some(old)
    }

    /// Puts the stored state `nbt`, which Anchor does not know, at local (x, y, z) as a chunk read
    /// from the server file holds it; the entry, or `None` outside the chunk.
    #[cfg(test)]
    pub(crate) fn put_unknown(
        &mut self,
        x: u8,
        y: i32,
        z: u8,
        nbt: nbt::Compound,
    ) -> Option<Entry> {
        let mut list = self.unknown.to_vec();
        let entry = Entry::unknown(u32::try_from(list.len()).ok()?, &nbt);
        list.push(nbt);
        self.unknown = Arc::new(list);
        self.put(x, y, z, entry)?;
        Some(entry)
    }

    /// Marks the sub-chunk holding height `y` changed (nothing outside the world height).
    pub(super) fn mark_dirty(&mut self, y: i32) {
        if let Some((index, _)) = split_y(y) {
            self.dirty |= 1 << index;
        }
    }

    /// Takes the copy a save snapshot takes (its sub-chunks shared) with the changes it must
    /// write, after which this chunk is clean and held by the file.
    pub(crate) fn take_save(&mut self) -> Self {
        let save = self.clone();
        self.dirty = 0;
        self.split = true;
        save
    }

    /// Notes that the chunk was read from the server file.
    pub(crate) fn set_split(&mut self) {
        self.split = true;
    }

    /// Works out the chunk's light alone, as if its neighbours were dark (PMMP
    /// `LightPopulationTask`): a pure function of the chunk for the CPU pool, done when a chunk is
    /// loaded or generated. The world connects it to its neighbours when the chunk is loaded.
    pub(crate) fn populate_light(&mut self) {
        self.light = Some(Box::new(light::populate(self)));
    }

    /// The chunk's light, to change it.
    pub(super) fn light_mut(&mut self) -> Option<&mut ChunkLight> {
        self.light.as_deref_mut()
    }

    /// Gives the chunk its worked out light.
    pub(super) fn set_light(&mut self, light: ChunkLight) {
        self.light = Some(Box::new(light));
    }

    /// Takes the chunk's light away.
    pub(super) fn take_light(&mut self) -> Option<Box<ChunkLight>> {
        self.light.take()
    }

    /// A chunk from checkpoint parts: the parts of [`Self::from_parts`] unchanged, with the save
    /// marks and the light. Fails if an entry indexes past the unknown list, the list is longer
    /// than one per slot, or a dirty mark is past the last sub-chunk.
    fn from_kept(
        sub_chunks: [Option<Arc<SubChunk>>; SUB_CHUNKS],
        biomes: [Arc<Paletted<u32>>; SUB_CHUNKS],
        unknown: Vec<nbt::Compound>,
        dirty: u32,
        split: bool,
        light: Option<Box<ChunkLight>>,
    ) -> crate::tick::checkpoint::Result<Self> {
        use crate::tick::checkpoint::Error;
        if !unknown_indexes_fit(sub_chunks.iter().flatten().map(Arc::as_ref), unknown.len()) {
            return Err(Error::Format("chunk unknown states"));
        }
        if dirty >> SUB_CHUNKS != 0 {
            return Err(Error::Format("chunk dirty marks"));
        }
        Ok(Self::assemble(
            sub_chunks, biomes, unknown, dirty, split, light,
        ))
    }

    /// A chunk of these parts, which the caller has checked, with its column heights worked out.
    fn assemble(
        sub_chunks: [Option<Arc<SubChunk>>; SUB_CHUNKS],
        biomes: [Arc<Paletted<u32>>; SUB_CHUNKS],
        unknown: Vec<nbt::Compound>,
        dirty: u32,
        split: bool,
        light: Option<Box<ChunkLight>>,
    ) -> Self {
        let mut chunk = Self {
            sub_chunks,
            biomes,
            heights: Box::new([NO_HEIGHT; COLUMNS]),
            unknown: Arc::new(unknown),
            dirty,
            split,
            light,
        };
        chunk.fill_heights();
        chunk
    }

    /// The highest non-air layer 0 block of the column at or below `top`, or [`NO_HEIGHT`].
    fn scan_down(&self, x: u8, z: u8, top: i32) -> i16 {
        let Some((first, mut ly)) = split_y(top.min(MAX_Y)) else {
            return NO_HEIGHT;
        };
        for index in (0..=first).rev() {
            if let Some(Some(sub)) = self.sub_chunks.get(index)
                && !sub
                    .layers()
                    .first()
                    .is_some_and(|l| l.is_uniform(Entry::AIR))
            {
                for y in (0..=ly).rev() {
                    if sub.get(offset(x, y, z)) != Entry::AIR {
                        #[expect(
                            clippy::expect_used,
                            reason = "invariant: an index below SUB_CHUNKS and a world height fit i32 and i16"
                        )]
                        let height = i16::try_from(
                            (i32::try_from(index).expect("a sub-chunk index fits i32")
                                + LOWEST_SUB_CHUNK)
                                * 16
                                + i32::from(y),
                        )
                        .expect("a world height fits i16");
                        return height;
                    }
                }
            }
            ly = 15;
        }
        NO_HEIGHT
    }

    /// Works out the height of every column from the blocks.
    fn fill_heights(&mut self) {
        for x in 0..16 {
            for z in 0..16 {
                let top = self.scan_down(x, z, MAX_Y);
                if let Some(h) = self.heights.get_mut(column(x, z)) {
                    *h = top;
                }
            }
        }
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chunk")
            .field("sub_chunks", &self.sub_chunks.iter().flatten().count())
            .field("unknown", &self.unknown.len())
            .field("dirty", &self.dirty)
            .field("lit", &self.light.is_some())
            .finish_non_exhaustive()
    }
}

impl Clone for Chunk {
    fn clone(&self) -> Self {
        Self {
            sub_chunks: self.sub_chunks.clone(),
            biomes: self.biomes.clone(),
            heights: self.heights.clone(),
            unknown: Arc::clone(&self.unknown),
            dirty: self.dirty,
            split: self.split,
            light: None,
        }
    }
}

/// A chunk's checkpoint form for the replay tool: its exact sub-chunks and biomes (palette
/// order and unused entries included, which a load from the server file compacts), its unknown
/// states, save marks and light; the heights are worked out again as it reads.
impl crate::tick::checkpoint::Codec for Chunk {
    fn encode(&self, w: &mut crate::tick::checkpoint::Writer) {
        let Self {
            sub_chunks,
            biomes,
            heights: _,
            unknown,
            dirty,
            split,
            light,
        } = self;
        sub_chunks.encode(w);
        biomes.encode(w);
        w.slice(unknown);
        dirty.encode(w);
        split.encode(w);
        light.encode(w);
    }

    fn decode(
        r: &mut crate::tick::checkpoint::Reader<'_>,
    ) -> crate::tick::checkpoint::Result<Self> {
        use crate::tick::checkpoint::Codec;
        let sub_chunks = Codec::decode(r)?;
        let biomes = Codec::decode(r)?;
        let unknown = Codec::decode(r)?;
        let dirty = Codec::decode(r)?;
        let split = Codec::decode(r)?;
        let light = Codec::decode(r)?;
        Self::from_kept(sub_chunks, biomes, unknown, dirty, split, light)
    }
}

/// The sub-chunk index and the y inside it of a height, if it is in the world.
#[inline]
pub(super) fn split_y(y: i32) -> Option<(usize, u8)> {
    if !(MIN_Y..=MAX_Y).contains(&y) {
        return None;
    }
    let index = usize::try_from((y >> 4) - LOWEST_SUB_CHUNK).ok()?;
    Some((index, u8::try_from(y & 15).ok()?))
}

/// The column index of local (x, z).
pub(super) fn column(x: u8, z: u8) -> usize {
    (usize::from(z & 15) << 4) | usize::from(x & 15)
}

/// Whether `unknown_len` unknown states fit the slots ([`MAX_UNKNOWN`]) and every palette entry
/// of `sub_chunks` indexes inside them, as [`Chunk::stored_block`] relies on.
fn unknown_indexes_fit<'a>(
    mut sub_chunks: impl Iterator<Item = &'a SubChunk>,
    unknown_len: usize,
) -> bool {
    unknown_len <= MAX_UNKNOWN
        && sub_chunks.all(|s| {
            s.layers().iter().all(|l| {
                l.palette().iter().all(|e| {
                    e.unknown_index()
                        .is_none_or(|i| usize::try_from(i).is_ok_and(|i| i < unknown_len))
                })
            })
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::block::{self, Connection};
    use crate::crypto::sha256;
    use crate::protocol::{self, Codec, Reader, Rng};
    use crate::world::paletted::tests::check;
    use crate::world::{Flat, Void};

    /// The network ID of `minecraft:kelp` with `kelp_age` 3 in the 1.26.60 reference list (the
    /// value BDS sends).
    const KELP_AGE_3: u32 = 3_718_566_417;

    #[test]
    fn the_sub_chunks_span_the_world_height() {
        assert_eq!(SUB_CHUNKS * 16, usize::try_from(MAX_Y - MIN_Y + 1).unwrap());
    }

    /// States without neighbour rules, so setting one changes nothing else.
    pub(crate) fn plain_states() -> Vec<State> {
        State::all()
            .filter(|s| s.connection() == Connection::None)
            .collect()
    }

    /// The highest non-air block of each column, by reading every block.
    fn brute_heights(c: &Chunk) -> Vec<i16> {
        let mut out = vec![NO_HEIGHT; COLUMNS];
        for x in 0..16 {
            for z in 0..16 {
                if let Some(y) = (MIN_Y..=MAX_Y)
                    .rev()
                    .find(|&y| c.entry(x, y, z) != Some(Entry::AIR))
                {
                    out[column(x, z)] = i16::try_from(y).unwrap();
                }
            }
        }
        out
    }

    /// The model index of local (x, y, z).
    fn cell(x: u8, y: i32, z: u8) -> usize {
        usize::try_from(y - MIN_Y).unwrap() * 256 + column(x, z)
    }

    #[test]
    fn random_puts_match_an_array_and_keep_heights() {
        let states = plain_states();
        let mut rng = Rng(0x00c4_0001);
        let mut chunk = Chunk::new();
        let mut model = vec![Entry::AIR; 384 * 256];
        for round in 0..100_000usize {
            // Phases of many states (palettes widen), few states and air (they narrow and
            // sub-chunks empty), and of one sub-chunk (dense writes).
            let (pool, ys) = match (round / 10_000) % 3 {
                0 => (states.len(), 384),
                1 => (3, 384),
                _ => (2000, 16),
            };
            let x = u8::try_from(rng.below(16)).unwrap();
            let z = u8::try_from(rng.below(16)).unwrap();
            let y = MIN_Y + i32::try_from(rng.below(ys)).unwrap();
            let entry = if rng.below(4) == 0 {
                Entry::AIR
            } else {
                Entry::Known(states[rng.below(pool)])
            };
            assert_eq!(chunk.put(x, y, z, entry), Some(model[cell(x, y, z)]));
            model[cell(x, y, z)] = entry;
            if round % 9973 == 0 {
                assert_eq!(chunk.heights.to_vec(), brute_heights(&chunk));
            }
        }
        for x in 0..16 {
            for z in 0..16 {
                for y in MIN_Y..=MAX_Y {
                    assert_eq!(chunk.entry(x, y, z), Some(model[cell(x, y, z)]));
                }
            }
        }
        assert_eq!(chunk.heights.to_vec(), brute_heights(&chunk));
        for sub in chunk.sub_chunks.iter().flatten() {
            assert!(!sub.is_empty(), "air sub-chunks are dropped");
            for layer in sub.layers() {
                check(layer);
            }
        }
        assert_eq!(chunk.put(16, 0, 0, Entry::AIR), None);
        assert_eq!(chunk.put(0, MAX_Y + 1, 0, Entry::AIR), None);
        assert_eq!(chunk.put(0, MIN_Y - 1, 0, Entry::AIR), None);
    }

    #[test]
    fn random_sets_of_both_layers_match_an_array() {
        let states = plain_states();
        let mut rng = Rng(0x00c4_0002);
        let mut layers = [Paletted::uniform(Entry::AIR), Paletted::uniform(Entry::AIR)];
        let mut model = vec![vec![Entry::AIR; 4096]; 2];
        for round in 0..100_000usize {
            let pool = if (round / 10_000) % 2 == 0 { 2500 } else { 2 };
            let layer = rng.below(2);
            let o = rng.below(4096);
            let entry = Entry::Known(states[rng.below(pool)]);
            let offset = u16::try_from(o).unwrap();
            assert_eq!(layers[layer].set(offset, entry), model[layer][o]);
            model[layer][o] = entry;
        }
        for layer in &layers {
            check(layer);
        }
        let sub = SubChunk::from_layers(layers);
        for (o, (&m0, &m1)) in (0..).zip(model[0].iter().zip(&model[1])) {
            assert_eq!(sub.get(o), m0);
            assert_eq!(sub.layers()[1].get(o), m1);
        }
    }

    // pmmp: Chunk::getBlockStateId, Chunk::setBlockStateId, Chunk::getBiomeId, Chunk::setBiomeId, Chunk::getHighestBlockAt, Chunk
    #[test]
    fn a_clone_keeps_its_contents_while_the_original_changes() {
        let mut chunk = Chunk::generate(&Flat, 1, ChunkPos::new(0, 0));
        chunk.set_block(3, 100, 3, block::DIRT).unwrap();
        assert!(chunk.set_block(3, 400, 3, block::DIRT).is_err());
        let snapshot = chunk.clone();
        assert!(Arc::ptr_eq(
            snapshot.sub_chunks[0].as_ref().unwrap(),
            chunk.sub_chunks[0].as_ref().unwrap()
        ));
        chunk.put(0, -64, 0, Entry::AIR);
        chunk.put(3, 100, 3, Entry::AIR);
        chunk.put(5, 200, 5, Entry::Known(block::DIRT));
        chunk.set_biome(0, 0, 0, 7).unwrap();
        assert_eq!(snapshot.block(0, -64, 0), Some(block::BEDROCK));
        assert_eq!(snapshot.block(3, 100, 3), Some(block::DIRT));
        assert_eq!(snapshot.block(5, 200, 5), Some(block::AIR));
        assert_eq!(snapshot.biome(0, 0, 0), Some(PLAINS));
        assert_eq!(snapshot.highest_block(3, 3), Some(100));
        assert_eq!(chunk.block(0, -64, 0), Some(block::AIR));
        assert_eq!(chunk.highest_block(3, 3), Some(-61));
        assert_eq!(chunk.biome(0, 0, 0), Some(7));
        assert!(!Arc::ptr_eq(
            snapshot.sub_chunks[0].as_ref().unwrap(),
            chunk.sub_chunks[0].as_ref().unwrap()
        ));
    }

    #[test]
    fn a_sub_chunk_payload_is_encoded_without_growing_its_buffer() {
        let mut chunk = Chunk::generate(&Flat, 1, ChunkPos::new(0, 0));
        // 60 states in layer 0: a 6-bit palette.
        for (i, s) in (0u8..60).zip(State::all().step_by(97)) {
            chunk.set_block(i % 16, -60, i / 16, s).unwrap();
        }
        let sub = chunk.sub_chunk(0).unwrap();
        let payload = chunk.sub_chunk_payload(0).unwrap();
        assert!(payload.len() <= sub.max_encoded_bytes());
        assert_eq!(payload.capacity(), sub.max_encoded_bytes());
    }

    #[test]
    fn a_level_chunk_payload_is_encoded_without_growing_its_buffer() {
        let mut chunk = Chunk::new();
        let payload = chunk.level_chunk_payload();
        // The first section, 23 repeats of one byte, and the border byte.
        let bound = chunk.biome_storage(0).unwrap().max_encoded_bytes() + (SUB_CHUNKS - 1) + 1;
        assert_eq!(payload.capacity(), bound);
        chunk.set_biome(1, 70, 2, 4).unwrap();
        let payload = chunk.level_chunk_payload();
        assert!(
            payload.len() > bound,
            "mixed biomes need more than the flat bound"
        );
        let mixed = chunk.biome_storage(8).unwrap().max_encoded_bytes()
            + chunk.biome_storage(0).unwrap().max_encoded_bytes() * 2
            + (SUB_CHUNKS - 3)
            + 1;
        assert_eq!(payload.capacity(), mixed);
    }

    #[test]
    fn adding_unknown_states_grows_an_unshared_list_in_place() {
        let state = |name: &str| {
            nbt::Compound::from([(
                "name".to_owned(),
                nbt::Value::String(format!("minecraft:{name}")),
            )])
        };
        let foreign = Arc::new(vec![state("first_unknown"), state("second_unknown")]);
        let stored = |index| UnknownBlock {
            list: Arc::clone(&foreign),
            index,
        };
        let mut chunk = Chunk::new();
        chunk.unknown_entry(&stored(0)).unwrap();
        let list = Arc::as_ptr(&chunk.unknown);
        chunk.unknown_entry(&stored(1)).unwrap();
        assert_eq!(chunk.unknown().len(), 2);
        assert_eq!(Arc::as_ptr(&chunk.unknown), list, "grown in place");
        chunk.unknown_entry(&stored(1)).unwrap();
        assert_eq!(chunk.unknown().len(), 2, "an equal state adds nothing");
    }

    #[test]
    fn a_dirty_mark_past_the_last_sub_chunk_is_not_a_chunk() {
        let kept = |dirty| {
            Chunk::from_kept(
                Default::default(),
                std::array::from_fn(|_| Arc::new(Paletted::uniform(PLAINS))),
                Vec::new(),
                dirty,
                true,
                None,
            )
        };
        assert!(kept(1 << (SUB_CHUNKS - 1)).is_ok());
        assert!(kept(1 << SUB_CHUNKS).is_err());
    }

    #[test]
    fn writing_the_same_entry_keeps_a_shared_sub_chunk() {
        let mut chunk = Chunk::generate(&Flat, 1, ChunkPos::new(0, 0));
        let snapshot = chunk.clone();
        assert_eq!(
            chunk.put(0, -64, 0, Entry::Known(block::BEDROCK)),
            Some(Entry::Known(block::BEDROCK))
        );
        assert!(Arc::ptr_eq(
            snapshot.sub_chunks[0].as_ref().unwrap(),
            chunk.sub_chunks[0].as_ref().unwrap()
        ));
    }

    /// The flat chunk's payloads: its `LevelChunk` payload is the empty chunk's, and its one
    /// sub-chunk is the bytes BDS sends for its flat world (P6 `spawn` record: 1,049 bytes,
    /// SHA-256 below).
    #[test]
    fn the_flat_chunk_is_the_bytes_bds_sends() {
        let chunk = Chunk::generate(&Flat, 0, ChunkPos::new(0, 0));
        let mut empty = vec![0x01, 0x02];
        empty.extend([0xff; 23]);
        empty.push(0);
        assert_eq!(chunk.level_chunk_payload(), empty);
        assert_eq!(Chunk::new().level_chunk_payload(), empty);
        assert_eq!(chunk.sub_chunk_limit(), 1);
        let sub = chunk.sub_chunk_payload(0).unwrap();
        assert_eq!(sub.len(), 1049);
        let bds: Vec<u8> = "339f98f0a6ae6cf5937eb56ce77a684f7a1cb66f08e7aec3f919125fceb73692"
            .as_bytes()
            .chunks(2)
            .map(|h| u8::from_str_radix(std::str::from_utf8(h).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(sha256(&[&sub]).to_vec(), bds);
        assert_eq!(chunk.sub_chunk_payload(1), None);
        assert!(chunk.heights.iter().all(|&h| h == -61));
        let void = Chunk::generate(&Void, 0, ChunkPos::new(0, 0));
        assert_eq!(void.sub_chunk_limit(), 0);
        assert_eq!(void.level_chunk_payload(), empty);
    }

    // pmmp: Generator::generateChunk, Generator, Flat
    #[test]
    fn generators_give_the_same_bytes_for_the_same_seed_and_position() {
        for pos in [ChunkPos::new(0, 0), ChunkPos::new(-7, 12)] {
            let a = Chunk::generate(&Flat, 42, pos);
            let b = Chunk::generate(&Flat, 42, pos);
            assert_eq!(a.level_chunk_payload(), b.level_chunk_payload());
            for i in 0..SUB_CHUNKS {
                assert_eq!(a.sub_chunk_payload(i), b.sub_chunk_payload(i));
            }
            assert!(!a.is_dirty());
        }
    }

    /// A chunk with many states, unknown states, water logging and two biomes.
    fn varied(height: usize) -> Chunk {
        let states = plain_states();
        let mut rng = Rng(0x00c4_0003);
        let mut chunk = Chunk::generate(&Flat, 0, ChunkPos::new(0, 0));
        for _ in 0..3000 {
            let x = u8::try_from(rng.below(16)).unwrap();
            let z = u8::try_from(rng.below(16)).unwrap();
            let y = MIN_Y + i32::try_from(rng.below(height)).unwrap();
            chunk.put(x, y, z, Entry::Known(states[rng.below(states.len())]));
        }
        chunk.set_biome(1, 70, 2, 4).unwrap();
        // Unknown states and water through the parts constructor.
        let (mut subs, biomes, _) = parts(&chunk);
        let water = State::find(
            "minecraft:water",
            &[("liquid_depth", block::PropertyValue::Int(0))],
        )
        .unwrap();
        let mut sub = subs[1].take().unwrap_or_else(SubChunk::new);
        let mut layers = sub.layers().clone();
        layers[0].set(offset(1, 1, 1), Entry::Unknown(0));
        layers[0].set(offset(2, 1, 1), Entry::Unknown(1));
        layers[1].set(offset(1, 2, 1), Entry::Known(water));
        // Kelp in water: a state only clients know, with its water in layer 1.
        let kelp = nbt::Compound::from([
            (
                "name".to_owned(),
                nbt::Value::String("minecraft:kelp".to_owned()),
            ),
            (
                "states".to_owned(),
                nbt::Value::Compound(nbt::Compound::from([(
                    "kelp_age".to_owned(),
                    nbt::Value::Int(3),
                )])),
            ),
        ]);
        layers[0].set(offset(3, 1, 1), Entry::unknown(2, &kelp));
        layers[1].set(offset(3, 1, 1), Entry::Known(water));
        sub = SubChunk::from_layers(layers);
        subs[1] = Some(sub);
        let unknown = vec![nbt::Compound::new(), nbt::Compound::new(), kelp];
        Chunk::from_parts(subs, biomes, unknown).unwrap()
    }

    /// The stored parts of a chunk, rebuilt through the parts constructors.
    fn parts(
        c: &Chunk,
    ) -> (
        [Option<SubChunk>; SUB_CHUNKS],
        [Paletted<u32>; SUB_CHUNKS],
        Vec<nbt::Compound>,
    ) {
        let subs = std::array::from_fn(|i| {
            c.sub_chunk(i).map(|s| {
                SubChunk::from_layers(
                    s.layers()
                        .clone()
                        .map(|l| Paletted::from_parts(l.bits(), l.words(), l.palette()).unwrap()),
                )
            })
        });
        let biomes = std::array::from_fn(|i| {
            let b = c.biome_storage(i).unwrap();
            Paletted::from_parts(b.bits(), b.words(), b.palette()).unwrap()
        });
        (subs, biomes, c.unknown().to_vec())
    }

    #[test]
    fn sub_chunk_payloads_decode_to_the_network_ids() {
        let chunk = varied(160);
        assert_eq!(chunk.block(1, -47, 1), None, "unknown state");
        assert_eq!(chunk.entry(1, -47, 1), Some(Entry::Unknown(0)));
        // Kelp is sent as the ID of the reference list (and BDS) for `kelp_age` 3, over water.
        let kelp = chunk.entry(3, -47, 1).unwrap();
        assert!(matches!(kelp, Entry::Inert(_, 2)), "{kelp:?}");
        assert_eq!(kelp.network_id(), KELP_AGE_3);
        assert_eq!(chunk.block(3, -47, 1), None, "inert state");
        let mut seen = 0;
        for i in 0..SUB_CHUNKS {
            let Some(payload) = chunk.sub_chunk_payload(i) else {
                assert!(chunk.sub_chunk(i).is_none());
                continue;
            };
            seen += 1;
            let mut r = Reader::new(&payload, 1 << 20, 0);
            let decoded = protocol::SubChunk::decode(&mut r).unwrap();
            assert!(r.remaining().is_empty());
            assert_eq!(decoded.y_index, Some(i8::try_from(i).unwrap() - 4));
            let sub = chunk.sub_chunk(i).unwrap();
            let water = !sub.layers()[1].is_uniform(Entry::AIR);
            assert_eq!(decoded.layers.len(), if water { 2 } else { 1 });
            for (layer, storage) in sub.layers().iter().zip(&decoded.layers) {
                for x in 0..16 {
                    for y in 0..16 {
                        for z in 0..16 {
                            let id = storage.palette()[usize::from(storage.index(x, y, z))];
                            let want = match layer.get(offset(x, y, z)) {
                                Entry::Known(s) => s.network_id(),
                                Entry::Unknown(_) => block::INFO_UPDATE.network_id(),
                                Entry::Inert(s, _) => s.network_id(),
                            };
                            assert_eq!(id, want);
                        }
                    }
                }
            }
        }
        assert!(seen >= 6);
        // Biome sections decode as paletted storages; a repeat is 0xff.
        let payload = chunk.level_chunk_payload();
        let mut r = Reader::new(&payload, 1 << 20, 0);
        let mut sections = Vec::new();
        for _ in 0..SUB_CHUNKS {
            if r.remaining().first() == Some(&0xff) {
                r.uint8().unwrap();
                sections.push(sections.last().cloned().unwrap());
            } else {
                sections.push(protocol::PalettedStorage::decode(&mut r).unwrap());
            }
        }
        assert_eq!(r.remaining(), [0]);
        assert_eq!(
            sections[8].palette()[usize::from(sections[8].index(1, 6, 2))],
            4
        );
        assert_eq!(sections[7].palette(), [PLAINS]);
        assert_eq!(sections[9].palette(), [PLAINS]);
    }

    #[test]
    fn parts_round_trip() {
        let chunk = varied(160);
        let (subs, biomes, unknown) = parts(&chunk);
        let back = Chunk::from_parts(subs, biomes, unknown).unwrap();
        for x in 0..16 {
            for z in 0..16 {
                for y in MIN_Y..=MAX_Y {
                    assert_eq!(back.entry(x, y, z), chunk.entry(x, y, z));
                    let layer1 = |c: &Chunk| {
                        let (i, ly) = split_y(y).unwrap();
                        c.sub_chunk(i).map(|s| s.layers()[1].get(offset(x, ly, z)))
                    };
                    assert_eq!(layer1(&back), layer1(&chunk));
                }
            }
        }
        // The parts constructor compacts, so a second round gives the same parts.
        let (subs, biomes, unknown) = parts(&back);
        let again = Chunk::from_parts(subs, biomes, unknown).unwrap();
        for i in 0..SUB_CHUNKS {
            assert_eq!(again.sub_chunk(i), back.sub_chunk(i));
        }
        assert_eq!(back.level_chunk_payload(), chunk.level_chunk_payload());
        assert_eq!(back.heights, chunk.heights);
        assert_eq!(back.unknown().len(), 3);
        assert!(!back.is_dirty());
    }

    /// One layer's stored parts.
    type Layer = (u8, Vec<u32>, Vec<Entry>);

    #[test]
    fn malformed_parts_never_panic() {
        let chunk = varied(32);
        let base: Vec<Option<[Layer; 2]>> = (0..SUB_CHUNKS)
            .map(|i| {
                chunk.sub_chunk(i).map(|s| {
                    s.layers()
                        .clone()
                        .map(|l| (l.bits(), l.words().to_vec(), l.palette().to_vec()))
                })
            })
            .collect();
        let biomes: Vec<(u8, Vec<u32>, Vec<u32>)> = (0..SUB_CHUNKS)
            .map(|i| {
                let b = chunk.biome_storage(i).unwrap();
                (b.bits(), b.words().to_vec(), b.palette().to_vec())
            })
            .collect();
        let unknown = chunk.unknown().to_vec();
        let mut rng = Rng(0x00c4_0004);
        let (mut ok, mut failed) = (0, 0);
        for _ in 0..10_000 {
            let mut subs = base.clone();
            let mut biomes = biomes.clone();
            let mut unknown = unknown.clone();
            for _ in 0..=rng.below(3) {
                let i = rng.below(SUB_CHUNKS);
                match rng.below(8) {
                    0 => biomes[i].0 = u8::try_from(rng.below(20)).unwrap(),
                    1 => {
                        let n = rng.below(biomes[i].2.len() + 2);
                        biomes[i].2.resize(n, 9);
                    }
                    2 => unknown.truncate(rng.below(3)),
                    _ => {
                        let Some(sub) = subs[i].as_mut() else {
                            subs[i] = None;
                            continue;
                        };
                        let layer = &mut sub[rng.below(2)];
                        match rng.below(5) {
                            0 => layer.0 = u8::try_from(rng.below(20)).unwrap(),
                            1 => {
                                let n = rng.below(layer.1.len() + 2);
                                layer.1.resize(n, 0);
                            }
                            2 if !layer.1.is_empty() => {
                                let w = rng.below(layer.1.len());
                                layer.1[w] ^= 1 << rng.below(32);
                            }
                            3 if !layer.2.is_empty() => {
                                let p = rng.below(layer.2.len());
                                layer.2[p] = Entry::Unknown(u32::try_from(rng.below(5)).unwrap());
                            }
                            _ => layer.2.truncate(rng.below(layer.2.len() + 1)),
                        }
                    }
                }
            }
            let built = (|| {
                let mut out: [Option<SubChunk>; SUB_CHUNKS] = Default::default();
                for (slot, sub) in out.iter_mut().zip(&subs) {
                    if let Some([a, b]) = sub {
                        *slot = Some(SubChunk::from_layers([
                            Paletted::from_parts(a.0, &a.1, &a.2)?,
                            Paletted::from_parts(b.0, &b.1, &b.2)?,
                        ]));
                    }
                }
                let mut storages = Vec::new();
                for b in &biomes {
                    storages.push(Paletted::from_parts(b.0, &b.1, &b.2)?);
                }
                let storages: [Paletted<u32>; SUB_CHUNKS] = storages.try_into().unwrap();
                Chunk::from_parts(out, storages, unknown)
            })();
            match built {
                Ok(c) => {
                    ok += 1;
                    for sub in c.sub_chunks.iter().flatten() {
                        for layer in sub.layers() {
                            check(layer);
                        }
                    }
                    if ok % 64 == 1 {
                        assert_eq!(c.heights.to_vec(), brute_heights(&c));
                    }
                }
                Err(Error::Malformed(_)) => failed += 1,
                Err(e) => panic!("unexpected error {e}"),
            }
        }
        assert!(ok > 100 && failed > 1000, "ok {ok}, failed {failed}");
    }
}
