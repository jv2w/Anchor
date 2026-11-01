//! The light of one chunk worked out alone, as if every neighbour were dark.
//!
//! This is PMMP's `LightPopulationTask` (`BlockLightUpdate::recalculateChunk`,
//! `SkyLightUpdate::recalculateChunk`): the chunk's own emitters and its open sky. Light from and
//! to neighbours crosses later, face by face ([`super::Engine`]).
//!
//! It works on a snapshot of the chunk (shared sub-chunks), so it can run on the CPU pool or in
//! budgeted steps on the tick while the live chunk changes; the tick replays the changes it notes.
//! While it runs it keeps one byte per block for each channel and for the filter
//! ([`Population::WORK_BYTES`]), allocated on its first step and freed when it finishes.

use std::collections::VecDeque;
use std::fmt;

use super::{BOTTOM, Channel, ChunkLight, FULL, Nibbles, sky_height};
use crate::tick::checkpoint::{self, Codec, Error, Reader, Writer};
use crate::world::chunk::{COLUMNS, SUB_CHUNKS, column, split_y};
use crate::world::paletted::offset;
use crate::world::{BlockPos, Chunk, MAX_Y, MIN_Y, SLOTS};

/// The directions to a neighbouring block, east, west, south, north, up and down ([`neighbour`]).
const DIRECTIONS: u8 = 6;
/// The levels of a chunk: one per node and channel, the channels one after the other.
const LEVELS: usize = Channel::COUNT * NODES;
/// Blocks in a chunk: a node index is `sub-chunk × SUB_NODES + offset`.
const NODES: usize = SUB_CHUNKS * SUB_NODES;
/// The nodes a queue holds before it grows.
const QUEUE_CAPACITY: usize = 4096;
/// The words of [`Buffers::queued`]: one bit per level.
const QUEUED_WORDS: usize = LEVELS / 64;
/// The channel bit of a queued node: sky light, else block light. It is above every node index.
const SKY_BIT: usize = 1 << 17;
/// Blocks in a sub-chunk.
const SUB_NODES: usize = SLOTS as usize;

const _: () = assert!(NODES <= SKY_BIT);

/// The light of one chunk being worked out.
#[derive(Clone)]
pub(crate) struct Population {
    chunk: Chunk,
    heights: [i16; COLUMNS],
    phase: Phase,
    buffers: Option<Box<Buffers>>,
    /// Blocks of the live chunk changed after the snapshot.
    changed: Vec<BlockPos>,
}

impl Population {
    /// The bytes of working memory a running population holds.
    pub(crate) const WORK_BYTES: usize =
        NODES + LEVELS + QUEUED_WORDS * size_of::<u64>() + QUEUE_CAPACITY * size_of::<usize>();

    /// A population that starts lighting `snapshot`.
    ///
    /// The live chunk's later changes go to [`Self::note`].
    pub(crate) fn new(snapshot: Chunk) -> Self {
        Self {
            chunk: snapshot,
            heights: [BOTTOM; COLUMNS],
            phase: Phase::Scan(0, 0),
            buffers: None,
            changed: Vec::new(),
        }
    }

    /// Notes a block of the live chunk that changed after the snapshot.
    ///
    /// Returns `false` if `limit` blocks are noted already (the caller starts again from a new
    /// snapshot).
    pub(crate) fn note(&mut self, pos: BlockPos, limit: usize) -> bool {
        if self.changed.len() >= limit {
            return false;
        }
        self.changed.push(pos);
        true
    }

    /// Works until done or until `units` is spent, and returns whether it is done.
    ///
    /// One unit is a block read, a sub-chunk filled or a node spread. A column's sky height reads
    /// down to its highest sky blocker, at least one block. A sub-chunk of one block without
    /// light is one unit. The last column may take more than what was left.
    pub(crate) fn run(&mut self, units: &mut u32) -> bool {
        let mut buffers = self
            .buffers
            .take()
            .unwrap_or_else(|| Box::new(Buffers::new()));
        let done = self.work(&mut buffers, units);
        self.buffers = Some(buffers);
        done
    }

    /// Finishes the population into the light and the blocks noted since the snapshot.
    ///
    /// Uniform sub-chunks of the light take no memory.
    pub(crate) fn finish(self) -> (ChunkLight, Vec<BlockPos>) {
        let levels = self
            .buffers
            .map(|buffers| buffers.levels)
            .unwrap_or_default();
        let pack = |start: usize| {
            levels
                .get(start..start + SUB_NODES)
                .map_or(Nibbles::Uniform(0), Nibbles::from_values)
        };
        let light = ChunkLight {
            arrays: Channel::ALL.map(|channel| {
                let base = channel as usize * NODES;
                std::array::from_fn(|sub| pack(base + sub * SUB_NODES))
            }),
            heights: self.heights,
        };
        (light, self.changed)
    }

    /// Lights sub-chunk `sub` from the open sky.
    ///
    /// The light is 15 at and above each column's sky height, and the top block of a column
    /// blocked at the top is lit from above the world (PMMP `SkyLightUpdate::getEffectiveLight`
    /// above `Y_MAX`).
    #[expect(
        clippy::expect_used,
        clippy::indexing_slicing,
        reason = "invariant: the sub-chunk index is below 24 and the buffers hold every node"
    )]
    fn fill_sky(&self, buffers: &mut Buffers, sub: usize) {
        let sub_index = i32::try_from(sub).expect("a sub-chunk index fits 32 bits");
        let base = MIN_Y + sub_index * 16;
        let lowest = self.heights.iter().copied().min().map_or(MIN_Y, i32::from);
        let highest = self.heights.iter().copied().max().map_or(MIN_Y, i32::from);
        if base + 15 < lowest && sub + 1 < SUB_CHUNKS {
            return;
        }
        let start = NODES + sub * SUB_NODES;
        if base >= highest {
            buffers.levels[start..start + SUB_NODES].fill(FULL);
            return;
        }
        for (column_index, &height) in self.heights.iter().enumerate() {
            let height = i32::from(height);
            let (x, z) = local(column_index);
            for ly in 0..16u8 {
                let at = usize::from(offset(x, ly, z));
                let y = base + i32::from(ly);
                buffers.levels[start + at] = if y >= height {
                    FULL
                } else if y == MAX_Y {
                    FULL.saturating_sub(buffers.filters[sub * SUB_NODES + at])
                } else {
                    continue;
                };
            }
        }
    }

    /// Queues the sky blocks of column `column_index` that can light a block.
    ///
    /// These are the blocks from the sky height up to one below the highest sky height of the
    /// columns beside it (PMMP's `nodeColumnEnd`, one higher to also light the side of a blocked
    /// neighbour), and a lit top block.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a column index is below 256"
    )]
    fn seed_sky(&self, buffers: &mut Buffers, column_index: usize) {
        let (x, z) = local(column_index);
        let height = |dx: i32, dz: i32| {
            let (nx, nz) = (i32::from(x) + dx, i32::from(z) + dz);
            match (u8::try_from(nx), u8::try_from(nz)) {
                (Ok(nx), Ok(nz)) if nx < 16 && nz < 16 => i32::from(self.heights[column(nx, nz)]),
                _ => MIN_Y,
            }
        };
        let own = height(0, 0);
        let beside = [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .into_iter()
            .map(|(dx, dz)| height(dx, dz))
            .max()
            .unwrap_or(MIN_Y);
        for y in own..=(beside - 1).max(own).min(MAX_Y) {
            if let Some(node) = node_at(x, y, z) {
                push(buffers, node | SKY_BIT);
            }
        }
        if own > MAX_Y
            && let Some(node) = node_at(x, MAX_Y, z)
        {
            push(buffers, node | SKY_BIT);
        }
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a phase holds a column index below 256"
    )]
    fn work(&mut self, buffers: &mut Buffers, units: &mut u32) -> bool {
        while *units > 0 {
            match self.phase {
                Phase::Scan(sub, at) => self.scan(buffers, sub, at, units),
                Phase::Heights(column_index) => {
                    let (x, z) = local(column_index);
                    let (height, looked) = sky_height(&self.chunk, x, z, MAX_Y);
                    *units = units.saturating_sub(looked.max(1));
                    self.heights[column_index] = height;
                    self.phase = next(column_index, COLUMNS, Phase::Heights, Phase::Sky(0));
                }
                Phase::Sky(sub) => {
                    *units -= 1;
                    self.fill_sky(buffers, sub);
                    self.phase = next(sub, SUB_CHUNKS, Phase::Sky, Phase::SkySeeds(0));
                }
                Phase::SkySeeds(column_index) => {
                    *units -= 1;
                    self.seed_sky(buffers, column_index);
                    self.phase = next(column_index, COLUMNS, Phase::SkySeeds, Phase::Spread);
                }
                Phase::Spread => {
                    let Some(node) = buffers.queue.pop_front() else {
                        return true;
                    };
                    *units -= 1;
                    spread(buffers, node);
                }
            }
        }
        self.phase == Phase::Spread && buffers.queue.is_empty()
    }

    /// Reads sub-chunk `sub` from offset `at`, as many blocks as `units` allows.
    ///
    /// It reads each block's filter, and the emitters as block light sources. An air sub-chunk,
    /// and a sub-chunk of one block without light, are one unit.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the sub-chunk index is below 24 and the buffers hold every node"
    )]
    fn scan(&mut self, buffers: &mut Buffers, sub: usize, at: u16, units: &mut u32) {
        let after = if sub + 1 < SUB_CHUNKS {
            Phase::Scan(sub + 1, 0)
        } else {
            Phase::Heights(0)
        };
        let base = sub * SUB_NODES;
        let Some(sub_chunk) = self.chunk.sub_chunk(sub) else {
            *units -= 1;
            buffers.filters[base..base + SUB_NODES].fill(1);
            self.phase = after;
            return;
        };
        let [blocks, _] = sub_chunk.layers();
        let uniform = (at == 0 && blocks.bits() == 0).then(|| blocks.derived(0));
        if let Some(values) = uniform
            && values.emission == 0
        {
            *units -= 1;
            buffers.filters[base..base + SUB_NODES].fill(values.filter);
            self.phase = after;
            return;
        }
        // A budget past the sub-chunk's blocks reads it to the end.
        let room = u16::try_from(*units).unwrap_or(u16::MAX);
        let end = at.saturating_add(room).min(SLOTS);
        for block in at..end {
            let values = sub_chunk.light_values(block);
            let node = base + usize::from(block);
            buffers.filters[node] = values.filter;
            if values.emission > 0 {
                buffers.levels[node] = values.emission;
                push(buffers, node);
            }
        }
        *units -= u32::from(end - at);
        self.phase = if end < SLOTS {
            Phase::Scan(sub, end)
        } else {
            after
        };
    }
}

impl fmt::Debug for Population {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Population")
            .field("phase", &self.phase)
            .field("running", &self.buffers.is_some())
            .field("changed", &self.changed.len())
            .finish_non_exhaustive()
    }
}

impl Codec for Population {
    fn encode(&self, w: &mut Writer) {
        let Self {
            chunk,
            heights,
            phase,
            buffers,
            changed,
        } = self;
        chunk.encode(w);
        heights.encode(w);
        phase.encode(w);
        buffers.encode(w);
        changed.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        let population = Self {
            chunk: Codec::decode(r)?,
            heights: Codec::decode(r)?,
            phase: Codec::decode(r)?,
            buffers: Codec::decode(r)?,
            changed: Codec::decode(r)?,
        };
        let heights_fit = population
            .heights
            .iter()
            .all(|&height| (MIN_Y..=MAX_Y + 1).contains(&i32::from(height)));
        if !population.phase.is_in_range() || !heights_fit {
            return Err(Error::Format("light population"));
        }
        Ok(population)
    }
}

/// What a population does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Reading the blocks of each sub-chunk from an offset: filters and emitters.
    Scan(usize, u16),
    /// The sky height of each column, from this column on.
    Heights(usize),
    /// The open sky of each sub-chunk, from this one on.
    Sky(usize),
    /// The sky blocks that light their surroundings, column by column.
    SkySeeds(usize),
    /// Spreading from the queue.
    Spread,
}

impl Phase {
    /// Whether its sub-chunk, column and offset are inside the chunk.
    fn is_in_range(self) -> bool {
        match self {
            Self::Scan(sub, at) => sub < SUB_CHUNKS && at < SLOTS,
            Self::Heights(column_index) | Self::SkySeeds(column_index) => column_index < COLUMNS,
            Self::Sky(sub) => sub < SUB_CHUNKS,
            Self::Spread => true,
        }
    }
}

checkpoint::variants!(Phase {
    0 => Scan(sub, at),
    1 => Heights(column),
    2 => Sky(sub),
    3 => SkySeeds(column),
    4 => Spread,
});

/// The working memory of a running population.
#[derive(Clone)]
struct Buffers {
    /// The filter plus 1 of each block, by node.
    filters: Vec<u8>,
    /// Block light by node, then sky light by node.
    levels: Vec<u8>,
    /// One bit per entry of `levels`: queued now.
    queued: Vec<u64>,
    /// Nodes to spread from, with [`SKY_BIT`] for sky light.
    queue: VecDeque<usize>,
}

impl Buffers {
    /// Empty buffers: every filter and level 0, nothing queued.
    fn new() -> Self {
        Self {
            filters: vec![0; NODES],
            levels: vec![0; LEVELS],
            queued: vec![0; QUEUED_WORDS],
            queue: VecDeque::with_capacity(QUEUE_CAPACITY),
        }
    }
}

impl Codec for Buffers {
    fn encode(&self, w: &mut Writer) {
        let Self {
            filters,
            levels,
            queued,
            queue,
        } = self;
        filters.encode(w);
        levels.encode(w);
        queued.encode(w);
        queue.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        let buffers = Self {
            filters: Codec::decode(r)?,
            levels: Codec::decode(r)?,
            queued: Codec::decode(r)?,
            queue: Codec::decode(r)?,
        };
        let fits = buffers.filters.len() == NODES
            && buffers.levels.len() == LEVELS
            && buffers.queued.len() == QUEUED_WORDS
            && buffers.queue.iter().all(|&queued| is_node(queued));
        if !fits {
            return Err(Error::Format("light buffers"));
        }
        Ok(buffers)
    }
}

/// The phase after item `index` of `count`: the same phase at `index + 1`, or `then`.
fn next(index: usize, count: usize, same: fn(usize) -> Phase, then: Phase) -> Phase {
    if index + 1 < count {
        same(index + 1)
    } else {
        then
    }
}

/// The local (x, z) of a column index.
#[expect(
    clippy::expect_used,
    reason = "invariant: a column index is below the 256 columns of a chunk"
)]
fn local(index: usize) -> (u8, u8) {
    let index = u8::try_from(index).expect("a column index fits a byte");
    (index & 15, index >> 4)
}

/// Spreads a queued node's light to its neighbours in the chunk.
#[expect(
    clippy::indexing_slicing,
    reason = "invariant: a queued node and its neighbours index the buffers"
)]
fn spread(buffers: &mut Buffers, queued: usize) {
    let i = slot(queued);
    buffers.queued[i / 64] &= !(1 << (i % 64));
    let level = buffers.levels[i];
    if level <= 1 {
        return;
    }
    let node = queued & !SKY_BIT;
    let base = i - node;
    for direction in 0..DIRECTIONS {
        let Some(adjacent) = neighbour(node, direction) else {
            continue;
        };
        let potential = level.saturating_sub(buffers.filters[adjacent]);
        let adjacent_level = &mut buffers.levels[base + adjacent];
        if potential > *adjacent_level {
            *adjacent_level = potential;
            if potential > 1 {
                push(buffers, adjacent | (queued & SKY_BIT));
            }
        }
    }
}

/// The node next to `node` in `direction`, if it is in the chunk.
///
/// The directions are east, west, south, north, up and down.
fn neighbour(node: usize, direction: u8) -> Option<usize> {
    let sub = node / SUB_NODES;
    let at = node % SUB_NODES;
    let (x, z, y) = (at >> 8, (at >> 4) & 15, at & 15);
    match direction {
        0 => (x < 15).then(|| node + 256),
        1 => (x > 0).then(|| node - 256),
        2 => (z < 15).then(|| node + 16),
        3 => (z > 0).then(|| node - 16),
        4 if y < 15 => Some(node + 1),
        4 => (sub + 1 < SUB_CHUNKS).then(|| node + SUB_NODES - 15),
        _ if y > 0 => Some(node - 1),
        _ => (sub > 0).then(|| node - SUB_NODES + 15),
    }
}

/// Queues a node unless it is queued already.
#[expect(
    clippy::indexing_slicing,
    reason = "invariant: a queued node indexes the buffers"
)]
fn push(buffers: &mut Buffers, queued: usize) {
    let i = slot(queued);
    let word = &mut buffers.queued[i / 64];
    let bit = 1 << (i % 64);
    if *word & bit == 0 {
        *word |= bit;
        buffers.queue.push_back(queued);
    }
}

/// The index in [`Buffers::levels`] (and the bit in [`Buffers::queued`]) of a queued node.
fn slot(queued: usize) -> usize {
    let node = queued & !SKY_BIT;
    if queued & SKY_BIT == 0 {
        node
    } else {
        node + NODES
    }
}

/// The node of local (x, z) at world height `y`, if it is in the world.
fn node_at(x: u8, y: i32, z: u8) -> Option<usize> {
    let (sub, ly) = split_y(y)?;
    Some(sub * SUB_NODES + usize::from(offset(x, ly, z)))
}

/// Whether `queued` is a block of the chunk, with or without [`SKY_BIT`].
fn is_node(queued: usize) -> bool {
    (queued & !SKY_BIT) < NODES
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::world::light::tests::{encoded, read_back};

    #[test]
    fn a_running_population_is_read_back_from_its_checkpoint() {
        let mut population = Population::new(Chunk::new());
        let mut units = 100;
        assert!(!population.run(&mut units));
        let back = read_back(&population).unwrap();
        assert_eq!(encoded(&back), encoded(&population));
    }

    #[test]
    fn a_checkpoint_with_a_phase_outside_the_chunk_is_refused() {
        for phase in [
            Phase::Scan(SUB_CHUNKS, 0),
            Phase::Scan(0, SLOTS),
            Phase::Heights(COLUMNS),
            Phase::Sky(SUB_CHUNKS),
            Phase::SkySeeds(COLUMNS),
        ] {
            let mut population = Population::new(Chunk::new());
            population.phase = phase;
            assert!(read_back(&population).is_err(), "{phase:?}");
        }
    }

    #[test]
    fn a_checkpoint_with_buffers_of_the_wrong_size_is_refused() {
        let mut population = Population::new(Chunk::new());
        population.run(&mut 1);
        let mut short = population.clone();
        short.buffers.as_mut().unwrap().filters.pop();
        assert!(read_back(&short).is_err());
        let mut stray = population;
        stray.buffers.as_mut().unwrap().queue.push_back(2 * SKY_BIT);
        assert!(read_back(&stray).is_err());
    }

    #[test]
    fn a_checkpoint_with_a_sky_height_outside_the_world_is_refused() {
        let mut population = Population::new(Chunk::new());
        population.heights[0] = i16::MIN;
        assert!(read_back(&population).is_err());
    }
}
