//! Lighting one chunk alone (PMMP `LightPopulationTask`, `BlockLightUpdate::recalculateChunk`,
//! `SkyLightUpdate::recalculateChunk`): its own emitters and its open sky, as if every neighbour
//! were dark. Light from and to neighbours crosses later, face by face ([`super::Engine`]).
//!
//! It works on a snapshot of the chunk (shared sub-chunks), so it can run on the CPU pool or in
//! budgeted steps on the tick while the live chunk changes; the tick replays the changes it notes.
//! While it runs it keeps one byte per block for each channel and for the filter (about 300 KB),
//! allocated on its first step and freed when it finishes.

use std::collections::VecDeque;
use std::fmt;

use super::super::chunk::{COLUMNS, SUB_CHUNKS, column};
use super::super::paletted::offset;
use super::super::{BlockPos, Chunk, MAX_Y, MIN_Y};
use super::{ChunkLight, FULL, Nibbles, emission, filter, sky_height};

/// Blocks in a chunk: a node index is `sub-chunk << 12 | offset`.
const NODES: usize = SUB_CHUNKS * 4096;
/// The channel bit of a queued node: sky light, else block light.
const SKY_BIT: u32 = 1 << 17;

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

/// The working memory of a running population.
struct Buffers {
    /// The filter plus 1 of each block, by node.
    filters: Vec<u8>,
    /// Block light by node, then sky light by node.
    levels: Vec<u8>,
    /// One bit per entry of `levels`: queued now.
    queued: Vec<u64>,
    /// Nodes to spread from, with [`SKY_BIT`] for sky light.
    queue: VecDeque<u32>,
}

/// The light of one chunk being worked out.
pub(crate) struct Population {
    chunk: Chunk,
    heights: [i16; COLUMNS],
    phase: Phase,
    buffers: Option<Box<Buffers>>,
    /// Blocks of the live chunk changed after the snapshot, and the most it keeps.
    changed: Vec<BlockPos>,
    changed_limit: usize,
}

impl Population {
    /// Starts lighting `snapshot`. The live chunk's later changes go to [`Self::note`], at most
    /// `changed_limit` of them.
    pub(crate) fn new(snapshot: Chunk, changed_limit: usize) -> Self {
        Self {
            chunk: snapshot,
            heights: [i16::try_from(MIN_Y).unwrap_or(i16::MIN); COLUMNS],
            phase: Phase::Scan(0, 0),
            buffers: None,
            changed: Vec::new(),
            changed_limit,
        }
    }

    /// Notes a block of the live chunk that changed after the snapshot; `false` if the list is
    /// full (the caller starts again from a new snapshot).
    pub(crate) fn note(&mut self, pos: BlockPos) -> bool {
        if self.changed.len() >= self.changed_limit {
            return false;
        }
        self.changed.push(pos);
        true
    }

    /// Works until done or until `units` is spent: one unit per block read, column, sub-chunk
    /// filled or node spread. Returns whether it is done.
    pub(crate) fn run(&mut self, units: &mut u32) -> bool {
        let mut b = self.buffers.take().unwrap_or_else(|| {
            Box::new(Buffers {
                filters: vec![0; NODES],
                levels: vec![0; NODES * 2],
                queued: vec![0; NODES * 2 / 64],
                queue: VecDeque::with_capacity(4096),
            })
        });
        let done = self.work(&mut b, units);
        self.buffers = Some(b);
        done
    }

    fn work(&mut self, b: &mut Buffers, units: &mut u32) -> bool {
        while *units > 0 {
            match self.phase {
                Phase::Scan(s, at) => self.scan(b, s, at, units),
                Phase::Heights(c) => {
                    *units -= 1;
                    let (x, z) = local(c);
                    if let Some(h) = self.heights.get_mut(c) {
                        *h = sky_height(&self.chunk, x, z);
                    }
                    self.phase = next(c, COLUMNS, Phase::Heights, Phase::Sky(0));
                }
                Phase::Sky(s) => {
                    *units -= 1;
                    self.fill_sky(b, s);
                    self.phase = next(s, SUB_CHUNKS, Phase::Sky, Phase::SkySeeds(0));
                }
                Phase::SkySeeds(c) => {
                    *units -= 1;
                    self.seed_sky(b, c);
                    self.phase = next(c, COLUMNS, Phase::SkySeeds, Phase::Spread);
                }
                Phase::Spread => {
                    let Some(node) = b.queue.pop_front() else {
                        return true;
                    };
                    *units -= 1;
                    spread(b, node);
                }
            }
        }
        self.phase == Phase::Spread && b.queue.is_empty()
    }

    /// The finished light (uniform sub-chunks take no memory) and the blocks noted since the
    /// snapshot.
    pub(crate) fn finish(self) -> (ChunkLight, Vec<BlockPos>) {
        let levels = self.buffers.map(|b| b.levels).unwrap_or_default();
        let pack = |base: usize, s: usize| {
            let start = base + s * 4096;
            levels
                .get(start..start + 4096)
                .map_or(Nibbles::Uniform(0), Nibbles::from_values)
        };
        let light = ChunkLight {
            arrays: [
                std::array::from_fn(|s| pack(0, s)),
                std::array::from_fn(|s| pack(NODES, s)),
            ],
            heights: self.heights,
        };
        (light, self.changed)
    }

    /// Reads sub-chunk `s` from offset `at`, as many blocks as `units` allows: each block's
    /// filter, and the emitters as block light sources. An air sub-chunk is one unit.
    fn scan(&mut self, b: &mut Buffers, s: usize, at: u16, units: &mut u32) {
        let after = if s + 1 < SUB_CHUNKS {
            Phase::Scan(s + 1, 0)
        } else {
            Phase::Heights(0)
        };
        let base = s * 4096;
        let Some(sub) = self.chunk.sub_chunk(s) else {
            *units -= 1;
            if let Some(filters) = b.filters.get_mut(base..base + 4096) {
                filters.fill(1);
            }
            self.phase = after;
            return;
        };
        let end = u32::from(at).saturating_add(*units).min(4096);
        for o in u32::from(at)..end {
            let o16 = u16::try_from(o).unwrap_or(0);
            let entry = sub.get(o16);
            let node = base + usize::from(o16);
            if let Some(f) = b.filters.get_mut(node) {
                *f = filter(entry);
            }
            let light = emission(entry);
            if light > 0 {
                if let Some(l) = b.levels.get_mut(node) {
                    *l = light;
                }
                push(b, node_of(s, o16));
            }
        }
        *units -= end - u32::from(at);
        self.phase = if end < 4096 {
            Phase::Scan(s, u16::try_from(end).unwrap_or(0))
        } else {
            after
        };
    }

    /// Lights sub-chunk `s` from the open sky: 15 at and above each column's sky height, and the
    /// top block of a column blocked at the top lit from above the world (PMMP
    /// `SkyLightUpdate::getEffectiveLight` above `Y_MAX`).
    fn fill_sky(&self, work: &mut Buffers, sub: usize) {
        let base = MIN_Y + i32::try_from(sub).unwrap_or(0) * 16;
        let lowest = self.heights.iter().copied().min().map_or(MIN_Y, i32::from);
        let highest = self.heights.iter().copied().max().map_or(MIN_Y, i32::from);
        if base + 15 < lowest && sub + 1 < SUB_CHUNKS {
            return;
        }
        let start = NODES + sub * 4096;
        if base >= highest {
            if let Some(levels) = work.levels.get_mut(start..start + 4096) {
                levels.fill(FULL);
            }
            return;
        }
        for col in 0..COLUMNS {
            let height = self.heights.get(col).map_or(MIN_Y, |&h| i32::from(h));
            let (x, z) = local(col);
            for ly in 0..16u8 {
                let at = usize::from(offset(x, ly, z));
                let y = base + i32::from(ly);
                let value = if y >= height {
                    FULL
                } else if y == MAX_Y {
                    let filter = work.filters.get(sub * 4096 + at).copied();
                    FULL.saturating_sub(filter.unwrap_or(FULL))
                } else {
                    continue;
                };
                if let Some(level) = work.levels.get_mut(start + at) {
                    *level = value;
                }
            }
        }
    }

    /// Queues the sky blocks of column `col` that can light a block: from the sky height up to
    /// one below the highest sky height of the columns beside it (PMMP's `nodeColumnEnd`, one
    /// higher to also light the side of a blocked neighbour), and a lit top block.
    fn seed_sky(&self, work: &mut Buffers, col: usize) {
        let (x, z) = local(col);
        let height = |dx: i32, dz: i32| {
            let (nx, nz) = (i32::from(x) + dx, i32::from(z) + dz);
            match (u8::try_from(nx), u8::try_from(nz)) {
                (Ok(nx), Ok(nz)) if nx < 16 && nz < 16 => self
                    .heights
                    .get(column(nx, nz))
                    .map_or(MIN_Y, |&h| i32::from(h)),
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
                push(work, node | SKY_BIT);
            }
        }
        if own > MAX_Y
            && let Some(node) = node_at(x, MAX_Y, z)
        {
            push(work, node | SKY_BIT);
        }
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

/// The index in [`Buffers::levels`] (and the bit in [`Buffers::queued`]) of a queued node.
fn slot(queued: u32) -> usize {
    let node = usize::try_from(queued & !SKY_BIT).unwrap_or(0);
    if queued & SKY_BIT == 0 {
        node
    } else {
        node + NODES
    }
}

/// Queues a node unless it is queued already.
fn push(b: &mut Buffers, queued: u32) {
    let i = slot(queued);
    if let Some(word) = b.queued.get_mut(i / 64) {
        let bit = 1 << (i % 64);
        if *word & bit == 0 {
            *word |= bit;
            b.queue.push_back(queued);
        }
    }
}

/// Spreads a queued node's light to its neighbours in the chunk.
fn spread(b: &mut Buffers, queued: u32) {
    let i = slot(queued);
    if let Some(word) = b.queued.get_mut(i / 64) {
        *word &= !(1 << (i % 64));
    }
    let level = b.levels.get(i).copied().unwrap_or(0);
    if level <= 1 {
        return;
    }
    let node = queued & !SKY_BIT;
    let base = i - usize::try_from(node).unwrap_or(0);
    for dir in 0..6 {
        let Some(n) = neighbour(node, dir) else {
            continue;
        };
        let at = usize::try_from(n).unwrap_or(0);
        let potential = level.saturating_sub(b.filters.get(at).copied().unwrap_or(FULL));
        if let Some(l) = b.levels.get_mut(base + at)
            && potential > *l
        {
            *l = potential;
            if potential > 1 {
                push(b, n | (queued & SKY_BIT));
            }
        }
    }
}

/// The phase after item `i` of `count`: the same phase at `i + 1`, or `then`.
fn next(i: usize, count: usize, same: fn(usize) -> Phase, then: Phase) -> Phase {
    if i + 1 < count { same(i + 1) } else { then }
}

/// Local (x, z) of a column index.
fn local(c: usize) -> (u8, u8) {
    let c = u8::try_from(c & 0xff).unwrap_or(0);
    (c & 15, c >> 4)
}

/// The node of an offset in sub-chunk `s`.
fn node_of(s: usize, offset: u16) -> u32 {
    (u32::try_from(s).unwrap_or(0) << 12) | u32::from(offset)
}

/// The node of local (x, z) at world height `y`, if it is in the world.
fn node_at(x: u8, y: i32, z: u8) -> Option<u32> {
    let s = usize::try_from((y - MIN_Y) >> 4).ok()?;
    if s >= SUB_CHUNKS {
        return None;
    }
    let ly = u8::try_from((y - MIN_Y) & 15).ok()?;
    Some(node_of(s, offset(x, ly, z)))
}

/// The node next to `node` in direction `dir` (east, west, south, north, up, down), if it is in
/// the chunk.
fn neighbour(node: u32, dir: u8) -> Option<u32> {
    let sub = node >> 12;
    let at = node & 4095;
    let (x, z, y) = (at >> 8, (at >> 4) & 15, at & 15);
    let subs = u32::try_from(SUB_CHUNKS).unwrap_or(0);
    match dir {
        0 => (x < 15).then(|| node + 256),
        1 => (x > 0).then(|| node - 256),
        2 => (z < 15).then(|| node + 16),
        3 => (z > 0).then(|| node - 16),
        4 if y < 15 => Some(node + 1),
        4 => (sub + 1 < subs).then(|| ((sub + 1) << 12) | (at & !15)),
        _ if y > 0 => Some(node - 1),
        _ => (sub > 0).then(|| ((sub - 1) << 12) | (at & !15) | 15),
    }
}
